//! Rendering the access predicate into SQL (0.10, second half).
//!
//! §7 is explicit that the predicate is applied **at query time, never as a post-filter**, because
//! pagination counts alone disclose the existence of assets a caller cannot see. That is the property
//! this suite exists to check, and it cannot be checked without a real database: a post-filter and an
//! in-query filter return the same *rows*, and differ only in the count.
//!
//! So there is a test here that fetches a count and compares it to the row set. It looks redundant. It
//! is the entire point.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_core::policy::{self, Action, Grant, Grants};
use dam_db::{access, migrate, testing::PostgresHarness};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0)
        .single()
        .expect("timestamp")
}

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

/// Creates a group and returns its id.
async fn make_group(pool: &PgPool, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .expect("insert group")
}

/// Creates an asset, optionally in a group, and returns its id.
async fn make_asset(
    pool: &PgPool,
    filename: &str,
    group: Option<Uuid>,
    release_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO assets (id, version_group_id, content_hash, filename, mime, bytes, \
                             release_at, expires_at) \
         VALUES (gen_random_uuid(), gen_random_uuid(), repeat('a', 64), $1, 'image/jpeg', 100, \
                 $2, $3) \
         RETURNING id",
    )
    .bind(filename)
    .bind(release_at)
    .bind(expires_at)
    .fetch_one(pool)
    .await
    .expect("insert asset");

    if let Some(group) = group {
        sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
            .bind(group)
            .bind(id)
            .execute(pool)
            .await
            .expect("add to group");
    }
    id
}

/// Runs `SELECT id FROM assets WHERE <predicate>` and returns the filenames, sorted.
async fn visible(pool: &PgPool, predicate: &policy::AccessPredicate) -> Vec<String> {
    let mut builder: QueryBuilder<Postgres> =
        QueryBuilder::new("SELECT filename FROM assets WHERE ");
    access::push_asset_filter(&mut builder, predicate).expect("render");
    builder.push(" ORDER BY filename");
    let mut names: Vec<String> = builder
        .build()
        .fetch_all(pool)
        .await
        .expect("query")
        .iter()
        .map(|row| row.get::<String, _>("filename"))
        .collect();
    names.sort();
    names
}

/// Runs `SELECT count(*) FROM assets WHERE <predicate>`.
async fn visible_count(pool: &PgPool, predicate: &policy::AccessPredicate) -> i64 {
    let mut builder: QueryBuilder<Postgres> =
        QueryBuilder::new("SELECT count(*) FROM assets WHERE ");
    access::push_asset_filter(&mut builder, predicate).expect("render");
    builder
        .build_query_scalar()
        .fetch_one(pool)
        .await
        .expect("count")
}

fn grant(permissions: &[&str], groups: &[Uuid]) -> Grant {
    Grant {
        permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
        asset_group_ids: groups.to_vec(),
        all_asset_groups: false,
        valid_from: None,
        valid_until: None,
        requires_eula: false,
        eula_accepted: false,
    }
}

#[tokio::test]
async fn a_caller_sees_only_assets_in_their_granted_groups() {
    let (_pg, pool) = db().await;
    let marketing = make_group(&pool, "marketing").await;
    let legal = make_group(&pool, "legal").await;

    make_asset(&pool, "brochure.jpg", Some(marketing), None, None).await;
    make_asset(&pool, "contract.pdf", Some(legal), None, None).await;
    make_asset(&pool, "orphan.jpg", None, None, None).await;

    let predicate = policy::compile(
        &Grants::from(vec![grant(&["asset:read"], &[marketing])]),
        Action::Read,
        now(),
    );
    assert_eq!(visible(&pool, &predicate).await, vec!["brochure.jpg"]);
}

#[tokio::test]
async fn the_count_matches_the_row_set_so_pagination_cannot_leak() {
    // §7's leak, asserted rather than assumed. A post-filter returns the same rows as an in-query
    // filter and differs only here: the count would be 3 while the visible rows were 1, and a client
    // paginating would see "3 results" with one row on the page. That difference is a disclosure of
    // assets the caller cannot see, and it is invisible unless something compares the two.
    let (_pg, pool) = db().await;
    let mine = make_group(&pool, "mine").await;
    let theirs = make_group(&pool, "theirs").await;

    make_asset(&pool, "a.jpg", Some(mine), None, None).await;
    for name in ["b.jpg", "c.jpg", "d.jpg"] {
        make_asset(&pool, name, Some(theirs), None, None).await;
    }

    let predicate = policy::compile(
        &Grants::from(vec![grant(&["asset:read"], &[mine])]),
        Action::Read,
        now(),
    );
    let rows = visible(&pool, &predicate).await;
    let count = visible_count(&pool, &predicate).await;
    assert_eq!(
        rows.len() as i64,
        count,
        "rows {rows:?} against count {count}"
    );
    assert_eq!(count, 1);
}

#[tokio::test]
async fn paginating_the_filtered_set_never_exposes_a_gap() {
    // The other half of the same leak: if the filter were applied after `LIMIT`, page one of two rows
    // would come back short and the client would infer that something had been removed.
    let (_pg, pool) = db().await;
    let mine = make_group(&pool, "mine").await;
    let theirs = make_group(&pool, "theirs").await;
    for name in ["a.jpg", "b.jpg", "c.jpg"] {
        make_asset(&pool, name, Some(mine), None, None).await;
    }
    for name in ["x.jpg", "y.jpg"] {
        make_asset(&pool, name, Some(theirs), None, None).await;
    }

    let predicate = policy::compile(
        &Grants::from(vec![grant(&["asset:read"], &[mine])]),
        Action::Read,
        now(),
    );

    let page = |limit: i64, offset: i64| {
        let predicate = predicate.clone();
        let pool = pool.clone();
        async move {
            let mut builder: QueryBuilder<Postgres> =
                QueryBuilder::new("SELECT filename FROM assets WHERE ");
            access::push_asset_filter(&mut builder, &predicate).expect("render");
            builder.push(" ORDER BY filename LIMIT ");
            builder.push_bind(limit);
            builder.push(" OFFSET ");
            builder.push_bind(offset);
            builder
                .build()
                .fetch_all(&pool)
                .await
                .expect("page")
                .iter()
                .map(|r| r.get::<String, _>("filename"))
                .collect::<Vec<String>>()
        }
    };

    assert_eq!(page(2, 0).await, vec!["a.jpg", "b.jpg"]);
    assert_eq!(page(2, 2).await, vec!["c.jpg"]);
    assert_eq!(page(2, 4).await, Vec::<String>::new());
}

#[tokio::test]
async fn a_caller_with_no_roles_sees_nothing_rather_than_everything() {
    // The direction of the failure. A predicate that matched nothing must render as a false condition,
    // not as an omitted filter — an omitted filter is a full scan of the tenant's library, and it is a
    // one-character mistake away.
    let (_pg, pool) = db().await;
    let group = make_group(&pool, "marketing").await;
    make_asset(&pool, "brochure.jpg", Some(group), None, None).await;

    let predicate = policy::compile(&Grants::from(vec![]), Action::Read, now());
    assert!(visible(&pool, &predicate).await.is_empty());
    assert_eq!(visible_count(&pool, &predicate).await, 0);
}

#[tokio::test]
async fn a_caller_with_the_verb_but_no_groups_also_sees_nothing() {
    let (_pg, pool) = db().await;
    let group = make_group(&pool, "marketing").await;
    make_asset(&pool, "brochure.jpg", Some(group), None, None).await;

    let predicate = policy::compile(
        &Grants::from(vec![grant(&["asset:read"], &[])]),
        Action::Read,
        now(),
    );
    assert_eq!(visible_count(&pool, &predicate).await, 0);
}

#[tokio::test]
async fn an_administrator_sees_every_asset_including_ungrouped_ones() {
    // An asset in no group at all is the case an `= ANY(groups)` join silently drops. An administrator
    // has to see it, or a mis-grouped upload becomes invisible to the only person who could fix it.
    let (_pg, pool) = db().await;
    let group = make_group(&pool, "marketing").await;
    make_asset(&pool, "brochure.jpg", Some(group), None, None).await;
    make_asset(&pool, "orphan.jpg", None, None, None).await;

    let mut admin = grant(&["asset:read"], &[]);
    admin.all_asset_groups = true;
    let predicate = policy::compile(&Grants::from(vec![admin]), Action::Read, now());
    assert_eq!(
        visible(&pool, &predicate).await,
        vec!["brochure.jpg", "orphan.jpg"]
    );
}

#[tokio::test]
async fn an_unreleased_or_expired_asset_still_appears_in_a_read_query() {
    // Decision 2, at the SQL layer. Filtering release and expiry out of the *visibility* query is the
    // obvious implementation and the wrong one: an asset that vanishes on expiry is one nobody renews.
    let (_pg, pool) = db().await;
    let group = make_group(&pool, "marketing").await;
    make_asset(&pool, "current.jpg", Some(group), None, None).await;
    make_asset(
        &pool,
        "embargoed.jpg",
        Some(group),
        Some(now() + Duration::days(7)),
        None,
    )
    .await;
    make_asset(
        &pool,
        "lapsed.jpg",
        Some(group),
        None,
        Some(now() - Duration::days(7)),
    )
    .await;

    let predicate = policy::compile(
        &Grants::from(vec![grant(&["asset:read"], &[group])]),
        Action::Read,
        now(),
    );
    assert_eq!(
        visible(&pool, &predicate).await,
        vec!["current.jpg", "embargoed.jpg", "lapsed.jpg"]
    );
}

#[tokio::test]
async fn a_soft_deleted_asset_is_filtered_out_of_every_query() {
    let (_pg, pool) = db().await;
    let group = make_group(&pool, "marketing").await;
    let doomed = make_asset(&pool, "deleted.jpg", Some(group), None, None).await;
    make_asset(&pool, "kept.jpg", Some(group), None, None).await;
    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(doomed)
        .execute(&pool)
        .await
        .expect("soft delete");

    let mut admin = grant(&["asset:read"], &[]);
    admin.all_asset_groups = true;
    for predicate in [
        policy::compile(
            &Grants::from(vec![grant(&["asset:read"], &[group])]),
            Action::Read,
            now(),
        ),
        policy::compile(&Grants::from(vec![admin]), Action::Read, now()),
    ] {
        assert_eq!(visible(&pool, &predicate).await, vec!["kept.jpg"]);
    }
}

#[tokio::test]
async fn an_asset_in_several_groups_is_visible_through_any_of_them() {
    let (_pg, pool) = db().await;
    let a = make_group(&pool, "a").await;
    let b = make_group(&pool, "b").await;
    let asset = make_asset(&pool, "shared.jpg", Some(a), None, None).await;
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(b)
        .bind(asset)
        .execute(&pool)
        .await
        .expect("second group");

    let predicate = policy::compile(
        &Grants::from(vec![grant(&["asset:read"], &[b])]),
        Action::Read,
        now(),
    );
    assert_eq!(visible(&pool, &predicate).await, vec!["shared.jpg"]);
}

#[tokio::test]
async fn an_asset_visible_through_two_granted_groups_appears_once() {
    // An `IN (SELECT ...)` is a set membership test, but a naive join would return the asset once per
    // matching group — inflating counts and breaking pagination in a way that only shows up when
    // somebody grants overlapping groups.
    let (_pg, pool) = db().await;
    let a = make_group(&pool, "a").await;
    let b = make_group(&pool, "b").await;
    let asset = make_asset(&pool, "shared.jpg", Some(a), None, None).await;
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(b)
        .bind(asset)
        .execute(&pool)
        .await
        .expect("second group");

    let predicate = policy::compile(
        &Grants::from(vec![grant(&["asset:read"], &[a, b])]),
        Action::Read,
        now(),
    );
    assert_eq!(visible_count(&pool, &predicate).await, 1);
}

#[tokio::test]
async fn a_rule_based_group_is_refused_rather_than_silently_ignored() {
    // Decision 4 says rule-based groups are evaluated live, and the language they are written in is the
    // query IR — which is task 2.4 and does not exist yet. Ignoring the predicate would grant *less*
    // access than the administrator configured: fail-closed, but silently, so nobody would find out
    // until an asset that should have been visible was not. Refusing names the gap.
    let (_pg, pool) = db().await;
    let rule_based: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label, predicate) \
         VALUES (gen_random_uuid(), 'recent', 'Recent', '{\"field\":\"created_at\"}'::jsonb) \
         RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("insert rule-based group");

    let predicate = policy::compile(
        &Grants::from(vec![grant(&["asset:read"], &[rule_based])]),
        Action::Read,
        now(),
    );
    let err = access::check_groups_are_renderable(&pool, &predicate)
        .await
        .expect_err("a rule-based group must be refused until the IR exists");
    assert!(
        format!("{err}").contains("predicate"),
        "the error must name what is unsupported: {err}"
    );
}

#[tokio::test]
async fn explicit_membership_groups_pass_the_renderability_check() {
    let (_pg, pool) = db().await;
    let plain = make_group(&pool, "marketing").await;
    let predicate = policy::compile(
        &Grants::from(vec![grant(&["asset:read"], &[plain])]),
        Action::Read,
        now(),
    );
    access::check_groups_are_renderable(&pool, &predicate)
        .await
        .expect("an explicit-membership group renders");
}
