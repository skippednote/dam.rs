//! Reading assets under an access predicate.
//!
//! The property this suite exists for is §7's: **pagination counts alone disclose the existence of assets a
//! caller cannot see**. A post-filter returns exactly the right rows and leaks through `total`, so the count
//! is asserted against the row set for every caller rather than only checked for plausibility.
//!
//! The rest is the things a grid needs that are easy to get subtly wrong: a deterministic order across page
//! boundaries, a tier derived from two columns rather than one, and an asset with no placement yet still
//! appearing in the library it was just uploaded to.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{Duration, Utc};
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::{AssetTier, ProvenanceState, RightsState};
use dam_db::assets::{self, Order};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

fn access(groups: Option<&[Uuid]>) -> policy::AccessPredicate {
    let (ids, all) = match groups {
        Some(ids) => (ids.to_vec(), false),
        None => (vec![], true),
    };
    policy::compile(
        &Grants::from(vec![Grant {
            permissions: vec!["asset:read".to_owned()],
            asset_group_ids: ids,
            all_asset_groups: all,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        }]),
        Action::Read,
        Utc::now(),
    )
}

/// A predicate holding the verb and no groups at all — visible to nothing.
fn scoped_to_nothing() -> policy::AccessPredicate {
    access(Some(&[]))
}

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

struct Spec<'a> {
    filename: &'a str,
    bytes: i64,
    minutes_ago: i64,
    group: Option<Uuid>,
}

async fn asset(pool: &PgPool, spec: &Spec<'_>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, width, height, \
                             version_group_id, created_at) \
         VALUES ($1, $2, $3, 'image/jpeg', $4, 800, 600, $1, now() - make_interval(mins => $5))",
    )
    .bind(id)
    .bind(blake3::hash(spec.filename.as_bytes()).to_hex().to_string())
    .bind(spec.filename)
    .bind(spec.bytes)
    .bind(i32::try_from(spec.minutes_ago).expect("a small number"))
    .execute(pool)
    .await
    .expect("asset");

    if let Some(group) = spec.group {
        sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
            .bind(group)
            .bind(id)
            .execute(pool)
            .await
            .expect("membership");
    }
    id
}

async fn group(pool: &PgPool, key: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO asset_groups (id, key, label) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(key)
        .execute(pool)
        .await
        .expect("group");
    id
}

async fn place(pool: &PgPool, asset_id: Uuid, class: &str, restore: &str) {
    place_in_pool(pool, asset_id, "only", class, restore).await;
}

async fn place_in_pool(
    pool: &PgPool,
    asset_id: Uuid,
    pool_label: &str,
    class: &str,
    restore: &str,
) {
    let expires = if restore == "available" {
        Some(Utc::now() + Duration::days(2))
    } else {
        None
    };
    sqlx::query(
        "INSERT INTO object_placements (object_key, pool_id, asset_id, size_bytes, checksum, \
                                        storage_class, restore_state, restore_expires_at) \
         VALUES ($1, gen_random_uuid(), $2, 10, 'x', $3, $4, $5)",
    )
    .bind(format!("{pool_label}/k/{asset_id}"))
    .bind(asset_id)
    .bind(class)
    .bind(restore)
    .bind(expires)
    .execute(pool)
    .await
    .expect("placement");
}

// ─── §7: the count cannot leak ──────────────────────────────────────────────

#[tokio::test]
async fn the_total_is_counted_under_the_callers_own_predicate() {
    // The leak this whole module is shaped around. Ten assets, three of them in a group the caller holds:
    // a post-filter returns three rows and a total of ten, and the difference is a disclosure — the caller
    // learns their library has seven assets somebody has hidden from them.
    let (_pg, pool) = db().await;
    let mine = group(&pool, "mine").await;
    let theirs = group(&pool, "theirs").await;

    for n in 0..3 {
        asset(
            &pool,
            &Spec {
                filename: &format!("mine-{n}.jpg"),
                bytes: 10,
                minutes_ago: n,
                group: Some(mine),
            },
        )
        .await;
    }
    for n in 0..7 {
        asset(
            &pool,
            &Spec {
                filename: &format!("theirs-{n}.jpg"),
                bytes: 10,
                minutes_ago: 10 + n,
                group: Some(theirs),
            },
        )
        .await;
    }

    let restricted = assets::page(&pool, &access(Some(&[mine])), Order::Newest, 0, 100)
        .await
        .expect("page");
    assert_eq!(restricted.items.len(), 3);
    assert_eq!(
        restricted.total, 3,
        "the total must count what this caller can see, not what exists"
    );

    let admin = assets::page(&pool, &access(None), Order::Newest, 0, 100)
        .await
        .expect("page");
    assert_eq!(admin.items.len(), 10);
    assert_eq!(admin.total, 10);

    // And the count still matches the row set when the page is smaller than the result.
    let paged = assets::page(&pool, &access(Some(&[mine])), Order::Newest, 0, 2)
        .await
        .expect("page");
    assert_eq!(paged.items.len(), 2, "a page of two returns two");
    assert_eq!(
        paged.total, 3,
        "and reports three matches, which is the count a scrollbar needs"
    );
}

#[tokio::test]
async fn a_caller_scoped_to_no_groups_sees_nothing_and_is_told_nothing() {
    let (_pg, pool) = db().await;
    let some_group = group(&pool, "some").await;
    let hidden = asset(
        &pool,
        &Spec {
            filename: "hidden.jpg",
            bytes: 10,
            minutes_ago: 1,
            group: Some(some_group),
        },
    )
    .await;

    let page = assets::page(&pool, &scoped_to_nothing(), Order::Newest, 0, 100)
        .await
        .expect("page");
    assert!(page.items.is_empty());
    assert_eq!(
        page.total, 0,
        "zero, not the library size — the count is the leak"
    );

    assert!(
        assets::detail(&pool, &scoped_to_nothing(), hidden)
            .await
            .expect("detail")
            .is_none(),
        "and the asset is absent rather than forbidden, so the caller learns nothing about it"
    );
}

#[tokio::test]
async fn an_asset_in_another_group_is_absent_rather_than_forbidden() {
    // `None` covers "does not exist" and "not yours" alike, so the handler answers both with a 404. A 403
    // would confirm the asset exists, which is what the group scoping was for.
    let (_pg, pool) = db().await;
    let mine = group(&pool, "mine").await;
    let theirs = group(&pool, "theirs").await;
    let ours = asset(
        &pool,
        &Spec {
            filename: "ours.jpg",
            bytes: 10,
            minutes_ago: 1,
            group: Some(mine),
        },
    )
    .await;
    let not_ours = asset(
        &pool,
        &Spec {
            filename: "not-ours.jpg",
            bytes: 10,
            minutes_ago: 2,
            group: Some(theirs),
        },
    )
    .await;

    let predicate = access(Some(&[mine]));
    assert!(
        assets::detail(&pool, &predicate, ours)
            .await
            .expect("detail")
            .is_some()
    );
    assert!(
        assets::detail(&pool, &predicate, not_ours)
            .await
            .expect("detail")
            .is_none()
    );
    assert!(
        assets::detail(&pool, &predicate, Uuid::new_v4())
            .await
            .expect("detail")
            .is_none(),
        "an id that never existed is indistinguishable from one the caller may not see"
    );
}

#[tokio::test]
async fn an_ungrouped_asset_is_invisible_to_a_scoped_caller_and_visible_to_an_administrator() {
    // Deliberate, and documented in `access.rs`: an asset in no group has no scope, so nobody scoped to
    // groups can see it — while an administrator's `all_asset_groups` skips the clause entirely, which is
    // how a mis-grouped upload stays reachable by the person who can fix it.
    let (_pg, pool) = db().await;
    let mine = group(&pool, "mine").await;
    let orphan = asset(
        &pool,
        &Spec {
            filename: "orphan.jpg",
            bytes: 10,
            minutes_ago: 1,
            group: None,
        },
    )
    .await;

    let scoped = assets::page(&pool, &access(Some(&[mine])), Order::Newest, 0, 100)
        .await
        .expect("page");
    assert_eq!(scoped.total, 0);

    let admin = assets::page(&pool, &access(None), Order::Newest, 0, 100)
        .await
        .expect("page");
    assert_eq!(admin.items.len(), 1);
    assert_eq!(admin.items[0].id, orphan);
}

#[tokio::test]
async fn a_soft_deleted_asset_is_gone_from_the_list_and_the_detail() {
    let (_pg, pool) = db().await;
    let kept = asset(
        &pool,
        &Spec {
            filename: "kept.jpg",
            bytes: 10,
            minutes_ago: 1,
            group: None,
        },
    )
    .await;
    let removed = asset(
        &pool,
        &Spec {
            filename: "removed.jpg",
            bytes: 10,
            minutes_ago: 2,
            group: None,
        },
    )
    .await;
    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(removed)
        .execute(&pool)
        .await
        .expect("soft delete");

    let page = assets::page(&pool, &access(None), Order::Newest, 0, 100)
        .await
        .expect("page");
    assert_eq!(
        page.items.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![kept]
    );
    assert_eq!(page.total, 1, "and it is not counted either");
    assert!(
        assets::detail(&pool, &access(None), removed)
            .await
            .expect("detail")
            .is_none()
    );
}

// ─── pagination ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_asset_appears_exactly_once_across_the_pages() {
    // The tie-break in the ORDER BY is what makes this true. `created_at` is not unique, and an offset walk
    // over a non-total order skips and repeats rows between pages — a virtualised grid scrolling back over a
    // page it has already drawn would show different assets, which reads as data corruption.
    //
    // Six hundred rows sharing one timestamp, walked in windows of fifty. The scale matters: over twenty rows
    // Postgres returns a stable physical order and the test passes with no tie-break at all, which is a test
    // that proves nothing. Past the point where the planner switches to a top-N heapsort per window, an order
    // that is not total does drop and repeat rows.
    let (_pg, pool) = db().await;
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id, created_at) \
         SELECT gen_random_uuid(), md5(n::text) || md5(n::text), 'tied-' || n || '.jpg', \
                'image/jpeg', 10, gen_random_uuid(), timestamptz '2026-08-18 09:00:00Z' \
         FROM generate_series(1, 600) AS n",
    )
    .execute(&pool)
    .await
    .expect("insert");

    let expected: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM assets ORDER BY id")
        .fetch_all(&pool)
        .await
        .expect("ids");
    assert_eq!(expected.len(), 600);

    let mut seen = Vec::new();
    for window in 0..12 {
        let got = assets::page(&pool, &access(None), Order::Newest, window * 50, 50)
            .await
            .expect("page");
        assert_eq!(got.total, 600, "the total is stable across pages");
        assert_eq!(got.offset, window * 50);
        assert_eq!(got.items.len(), 50);
        seen.extend(got.items.into_iter().map(|a| a.id));
    }

    seen.sort_unstable();
    assert_eq!(
        seen, expected,
        "six hundred assets sharing one timestamp must page without a repeat or a gap"
    );
}

#[tokio::test]
async fn the_orders_actually_order() {
    let (_pg, pool) = db().await;
    let oldest_small = asset(
        &pool,
        &Spec {
            filename: "aaa.jpg",
            bytes: 100,
            minutes_ago: 30,
            group: None,
        },
    )
    .await;
    let middle_large = asset(
        &pool,
        &Spec {
            filename: "mmm.jpg",
            bytes: 9_000,
            minutes_ago: 20,
            group: None,
        },
    )
    .await;
    let newest_medium = asset(
        &pool,
        &Spec {
            filename: "zzz.jpg",
            bytes: 500,
            minutes_ago: 1,
            group: None,
        },
    )
    .await;

    let predicate = access(None);
    for (order, expected) in [
        (
            Order::Newest,
            vec![newest_medium, middle_large, oldest_small],
        ),
        (
            Order::Oldest,
            vec![oldest_small, middle_large, newest_medium],
        ),
        (
            Order::FilenameAsc,
            vec![oldest_small, middle_large, newest_medium],
        ),
        (
            Order::FilenameDesc,
            vec![newest_medium, middle_large, oldest_small],
        ),
        (
            Order::LargestFirst,
            vec![middle_large, newest_medium, oldest_small],
        ),
    ] {
        let got: Vec<Uuid> = assets::page(&pool, &predicate, order, 0, 10)
            .await
            .expect("page")
            .items
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(got, expected, "{order:?}");
    }
}

#[tokio::test]
async fn an_absurd_limit_is_clamped_and_a_negative_offset_is_not_an_error() {
    // Both arrive from a query string, so neither can be trusted and neither should be a 500. The clamp is
    // what stops a caller asking for the whole library in one statement; the export path is a bulk
    // operation with its own budget.
    let (_pg, pool) = db().await;
    for n in 0..3 {
        asset(
            &pool,
            &Spec {
                filename: &format!("clamped-{n}.jpg"),
                bytes: 10,
                minutes_ago: n,
                group: None,
            },
        )
        .await;
    }

    // Enough rows that the clamp is observable: with three assets, any limit at all returns three and the
    // clamp could be missing entirely.
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         SELECT gen_random_uuid(), md5(n::text) || md5(n::text), 'bulk-' || n || '.jpg', \
                'image/jpeg', 10, gen_random_uuid() \
         FROM generate_series(1, 600) AS n",
    )
    .execute(&pool)
    .await
    .expect("bulk insert");

    let huge = assets::page(&pool, &access(None), Order::Newest, 0, 10_000_000)
        .await
        .expect("page");
    assert_eq!(
        i64::try_from(huge.items.len()).expect("small"),
        assets::MAX_LIMIT,
        "a caller asking for ten million gets one page, because one statement must not be asked to \
         materialise the whole library"
    );
    assert_eq!(
        huge.total, 603,
        "and the total still reports everything that matched"
    );

    let negative = assets::page(&pool, &access(None), Order::Newest, -5, 10)
        .await
        .expect("page");
    assert_eq!(
        negative.offset, 0,
        "a negative offset reads as the first page"
    );
    assert_eq!(negative.items.len(), 10);

    let zero_limit = assets::page(&pool, &access(None), Order::Newest, 0, 0)
        .await
        .expect("page");
    assert_eq!(
        zero_limit.items.len(),
        1,
        "a limit of zero becomes one rather than an empty page, because Postgres would accept it and the \
         caller would see an empty library"
    );
}

// ─── the derived tier ───────────────────────────────────────────────────────

#[tokio::test]
async fn the_tier_comes_from_both_columns_and_an_expired_restore_is_archived_again() {
    // The trap the schema warns about twice. Conflating `expired` with `available` leaves a download button
    // enabled until the day somebody presses it.
    let (_pg, pool) = db().await;

    let cases = [
        ("STANDARD", "none", AssetTier::Hot),
        ("STANDARD_IA", "none", AssetTier::Cool),
        ("GLACIER_IR", "none", AssetTier::Cool),
        ("GLACIER", "none", AssetTier::Archive),
        ("DEEP_ARCHIVE", "requested", AssetTier::Restoring),
        ("GLACIER", "ongoing", AssetTier::Restoring),
        ("GLACIER", "available", AssetTier::Restored),
        ("GLACIER", "expired", AssetTier::Archive),
        // A stale restore_state on an object since transitioned back to Standard: hot, not thawing.
        ("STANDARD", "available", AssetTier::Hot),
    ];

    for (index, (class, restore, expected)) in cases.iter().enumerate() {
        let id = asset(
            &pool,
            &Spec {
                filename: &format!("tiered-{index}.jpg"),
                bytes: 10,
                minutes_ago: i64::try_from(index).expect("small"),
                group: None,
            },
        )
        .await;
        place(&pool, id, class, restore).await;

        let detail = assets::detail(&pool, &access(None), id)
            .await
            .expect("detail")
            .expect("present");
        assert_eq!(
            detail.summary.tier, *expected,
            "{class}/{restore} must be {expected:?}"
        );
    }
}

#[tokio::test]
async fn an_asset_with_no_placement_yet_is_still_in_the_library() {
    // A freshly finalised upload has no placement row for a moment. Dropping it from the page would hide it
    // from the person who just uploaded it — and if the join were inside the count, the total would be
    // wrong too.
    let (_pg, pool) = db().await;
    let brand_new = asset(
        &pool,
        &Spec {
            filename: "just-uploaded.jpg",
            bytes: 10,
            minutes_ago: 0,
            group: None,
        },
    )
    .await;

    let page = assets::page(&pool, &access(None), Order::Newest, 0, 10)
        .await
        .expect("page");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].id, brand_new);
    assert_eq!(
        page.items[0].tier,
        AssetTier::Hot,
        "no placement means nothing is archived, so nothing needs a restore"
    );
}

#[tokio::test]
async fn the_display_states_come_from_the_columns_rather_than_a_default() {
    let (_pg, pool) = db().await;
    let id = asset(
        &pool,
        &Spec {
            filename: "stateful.jpg",
            bytes: 10,
            minutes_ago: 1,
            group: None,
        },
    )
    .await;

    // The defaults first: unevaluated rights are `unknown`, which the UI must not style like `allowed`.
    let fresh = assets::detail(&pool, &access(None), id)
        .await
        .expect("detail")
        .expect("present");
    assert_eq!(fresh.summary.rights_state, RightsState::Unknown);
    assert_eq!(fresh.summary.provenance_state, ProvenanceState::None);

    sqlx::query(
        "UPDATE assets SET rights_state = 'expiring', provenance_state = 'untrusted' WHERE id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .expect("update");

    let updated = assets::detail(&pool, &access(None), id)
        .await
        .expect("detail")
        .expect("present");
    assert_eq!(updated.summary.rights_state, RightsState::Expiring);
    assert_eq!(updated.summary.provenance_state, ProvenanceState::Untrusted);

    let page = assets::page(&pool, &access(None), Order::Newest, 0, 10)
        .await
        .expect("page");
    assert_eq!(
        page.items[0].rights_state,
        RightsState::Expiring,
        "the list and the detail must agree, or a badge changes when a panel opens"
    );
}

// ─── hydration ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn hydration_keeps_the_ranking_and_drops_what_the_caller_may_not_see() {
    // Tantivy ranks, Postgres authorises. The index is *permissive* while it is stale, so an id it returns
    // may name an asset the caller cannot see — this is the step that makes that harmless, and the order it
    // returns is the ranking, which is why it is preserved rather than sorted.
    let (_pg, pool) = db().await;
    let mine = group(&pool, "mine").await;
    let theirs = group(&pool, "theirs").await;

    let first = asset(
        &pool,
        &Spec {
            filename: "rank-1.jpg",
            bytes: 10,
            minutes_ago: 3,
            group: Some(mine),
        },
    )
    .await;
    let hidden = asset(
        &pool,
        &Spec {
            filename: "rank-2.jpg",
            bytes: 10,
            minutes_ago: 2,
            group: Some(theirs),
        },
    )
    .await;
    let third = asset(
        &pool,
        &Spec {
            filename: "rank-3.jpg",
            bytes: 10,
            minutes_ago: 1,
            group: Some(mine),
        },
    )
    .await;

    // Deliberately not in id or date order: this is a ranking.
    let ranked = vec![third, hidden, first];
    let visible = assets::visible_among(&pool, &access(Some(&[mine])), &ranked)
        .await
        .expect("hydrate");
    assert_eq!(
        visible,
        vec![third, first],
        "the ranking survives and the inaccessible id is dropped"
    );

    // A deleted asset the index has not caught up with is dropped by the same step.
    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(third)
        .execute(&pool)
        .await
        .expect("soft delete");
    assert_eq!(
        assets::visible_among(&pool, &access(Some(&[mine])), &ranked)
            .await
            .expect("hydrate"),
        vec![first]
    );

    assert!(
        assets::visible_among(&pool, &access(None), &[])
            .await
            .expect("hydrate")
            .is_empty(),
        "an empty candidate list is an empty result, not an unfiltered query"
    );

    // The other half of what a bulk preview needs. Scope decides what a caller may know about; a hold is a
    // fact about an asset they can already see, and the delete executor refuses one. `bulk::preview` counted
    // only the first and promised a number the second would not deliver.
    assert_eq!(
        assets::held_among(&pool, &ranked).await.expect("held"),
        0,
        "nothing is held to begin with"
    );
    sqlx::query("UPDATE assets SET legal_hold = true WHERE id = $1")
        .bind(first)
        .execute(&pool)
        .await
        .expect("place a hold");
    assert_eq!(assets::held_among(&pool, &ranked).await.expect("held"), 1);
    // `third` was soft-deleted above, so a hold on it does not count: it is already gone, and reporting it as
    // a reason the delete will be refused would explain a skip that has a different cause.
    sqlx::query("UPDATE assets SET legal_hold = true WHERE id = $1")
        .bind(third)
        .execute(&pool)
        .await
        .expect("hold a deleted asset");
    assert_eq!(
        assets::held_among(&pool, &ranked).await.expect("held"),
        1,
        "an already-deleted asset is not a pending refusal"
    );
    assert_eq!(assets::held_among(&pool, &[]).await.expect("held"), 0);
}

#[tokio::test]
async fn an_asset_replicated_across_pools_appears_once_and_reports_its_warmest_copy() {
    // `object_placements` is keyed `(object_key, pool_id)`, so a replicated asset has several rows. A plain
    // LEFT JOIN returns it once per row — twice in the grid, and twice in the window count, so the scrollbar
    // is wrong too. And the tier has to come from the *warmest* copy: a Deep Archive replica of an object
    // that is also in Standard must not disable a download that would have worked.
    let (_pg, pool) = db().await;
    let replicated = asset(
        &pool,
        &Spec {
            filename: "replicated.jpg",
            bytes: 10,
            minutes_ago: 1,
            group: None,
        },
    )
    .await;
    place_in_pool(&pool, replicated, "cold-pool", "DEEP_ARCHIVE", "none").await;
    place_in_pool(&pool, replicated, "hot-pool", "STANDARD", "none").await;

    let page = assets::page(&pool, &access(None), Order::Newest, 0, 10)
        .await
        .expect("page");
    assert_eq!(page.items.len(), 1, "two placements, one asset");
    assert_eq!(page.total, 1, "and counted once, or the scrollbar lies");
    assert_eq!(
        page.items[0].tier,
        AssetTier::Hot,
        "the warmest present copy decides the tier"
    );

    // The detail must agree, or the badge changes when the panel opens.
    let detail = assets::detail(&pool, &access(None), replicated)
        .await
        .expect("detail")
        .expect("present");
    assert_eq!(detail.summary.tier, AssetTier::Hot);

    // A restored archive copy beats an unrestored one, which is the other half of "warmest".
    let archived = asset(
        &pool,
        &Spec {
            filename: "two-archives.jpg",
            bytes: 10,
            minutes_ago: 2,
            group: None,
        },
    )
    .await;
    place_in_pool(&pool, archived, "a-pool", "GLACIER", "none").await;
    place_in_pool(&pool, archived, "b-pool", "GLACIER", "available").await;
    assert_eq!(
        assets::detail(&pool, &access(None), archived)
            .await
            .expect("detail")
            .expect("present")
            .summary
            .tier,
        AssetTier::Restored
    );
}

#[tokio::test]
async fn a_placement_that_is_not_present_does_not_decide_the_tier() {
    // A `missing` or `corrupt` placement is not a copy anybody can fetch. Letting one supply the tier would
    // report an asset as hot on the strength of bytes the scrub has already flagged as gone.
    let (_pg, pool) = db().await;
    let id = asset(
        &pool,
        &Spec {
            filename: "half-lost.jpg",
            bytes: 10,
            minutes_ago: 1,
            group: None,
        },
    )
    .await;
    place_in_pool(&pool, id, "gone-pool", "STANDARD", "none").await;
    sqlx::query("UPDATE object_placements SET state = 'missing' WHERE asset_id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("mark missing");
    place_in_pool(&pool, id, "cold-pool", "GLACIER", "none").await;

    assert_eq!(
        assets::detail(&pool, &access(None), id)
            .await
            .expect("detail")
            .expect("present")
            .summary
            .tier,
        AssetTier::Archive,
        "the missing Standard copy must not make this look downloadable"
    );
}
