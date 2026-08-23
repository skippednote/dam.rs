//! The admin worklists (Q.20, Q.2c·3).
//!
//! Each worklist is one SQL condition, and the risk is entirely in the conditions. Three of them are the kind
//! that look right and are not:
//!
//! **"Missing required metadata" resolves a type per asset.** The required flag lives on the *field*, but
//! whether the field applies depends on the asset's metadata type — its own, else the tenant default, else
//! every field. So the same empty field is a gap for one asset and irrelevant for another, and the case that
//! catches a naive query is a required field that the asset's type does not include.
//!
//! **A key present with an empty value is still missing.** `{"caption": ""}` and `{"caption": []}` are what a
//! form posts when somebody tabs through it, and a `?` existence check calls both of them filled.
//!
//! **Expiry has two lists, and they are about different things.** Past-and-still-active is an exposure;
//! upcoming is a task. An asset that expired and was archived is on neither, because somebody dealt with it.
//!
//! And one property that is not about SQL at all: **every count runs through the caller's predicate**, so a
//! scoped reader's worklist is their own. A worklist that counted work its reader cannot see would send them
//! looking for an asset that 404s — §7's disclosure rule, arriving as a usability bug.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::policy::{self, Action, Grant, Grants};
use dam_db::worklists::{self, Worklist};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use std::collections::HashMap;
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

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

async fn asset(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 4096, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

/// A category tree with one term in it. Returns `(taxonomy, term)`.
async fn category(pool: &PgPool, taxonomy_key: &str, slug: &str) -> (Uuid, Uuid) {
    let taxonomy: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomies (id, key, label, kind) \
         VALUES (gen_random_uuid(), $1, $1, 'category') RETURNING id",
    )
    .bind(taxonomy_key)
    .fetch_one(pool)
    .await
    .expect("taxonomy");
    let term: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomy_terms (id, taxonomy_id, slug, label, path) \
         VALUES (gen_random_uuid(), $1, $2, $2, $2::extensions.ltree) RETURNING id",
    )
    .bind(taxonomy)
    .bind(slug)
    .fetch_one(pool)
    .await
    .expect("term");
    (taxonomy, term)
}

/// The counts, keyed, so a case asserts the one it is about and its neighbours stay readable.
async fn counted(pool: &PgPool, predicate: &policy::AccessPredicate) -> HashMap<Worklist, i64> {
    worklists::counts(pool, predicate)
        .await
        .expect("counts")
        .into_iter()
        .collect()
}

async fn ids_on(pool: &PgPool, worklist: Worklist) -> Vec<Uuid> {
    worklists::page(
        pool,
        &access(None),
        worklist,
        dam_db::assets::Order::Oldest,
        0,
        50,
    )
    .await
    .expect("page")
    .items
    .into_iter()
    .map(|item| item.id)
    .collect()
}

// ─── expiry ─────────────────────────────────────────────────────────────────

async fn expiry_splits_into_exposure_and_task(pool: &PgPool) {
    let lapsed = asset(pool, "lapsed").await;
    let soon = asset(pool, "soon").await;
    let later = asset(pool, "later").await;
    let dealt_with = asset(pool, "dealt-with").await;

    sqlx::query("UPDATE assets SET expires_at = now() - interval '1 day' WHERE id = $1")
        .bind(lapsed)
        .execute(pool)
        .await
        .expect("lapsed");
    sqlx::query("UPDATE assets SET expires_at = now() + interval '10 days' WHERE id = $1")
        .bind(soon)
        .execute(pool)
        .await
        .expect("soon");
    // Outside the 30-day horizon: a real date, and not yet anybody's problem.
    sqlx::query("UPDATE assets SET expires_at = now() + interval '200 days' WHERE id = $1")
        .bind(later)
        .execute(pool)
        .await
        .expect("later");
    sqlx::query(
        "UPDATE assets SET expires_at = now() - interval '9 days', status = 'archived' WHERE id = $1",
    )
    .bind(dealt_with)
    .execute(pool)
    .await
    .expect("dealt with");

    assert_eq!(ids_on(pool, Worklist::Expired).await, vec![lapsed]);
    assert_eq!(
        ids_on(pool, Worklist::ExpiringSoon).await,
        vec![soon],
        "the 200-day one is a date, not a task"
    );

    sqlx::query("UPDATE assets SET expires_at = NULL, status = 'active'")
        .execute(pool)
        .await
        .expect("reset");
}

async fn an_embargo_is_a_future_release_date(pool: &PgPool) {
    let held = asset(pool, "held").await;
    let out = asset(pool, "out").await;
    sqlx::query("UPDATE assets SET release_at = now() + interval '5 days' WHERE id = $1")
        .bind(held)
        .execute(pool)
        .await
        .expect("held");
    // A release date in the past is not an embargo; it is history.
    sqlx::query("UPDATE assets SET release_at = now() - interval '5 days' WHERE id = $1")
        .bind(out)
        .execute(pool)
        .await
        .expect("out");

    assert_eq!(ids_on(pool, Worklist::Embargoed).await, vec![held]);
    sqlx::query("UPDATE assets SET release_at = NULL")
        .execute(pool)
        .await
        .expect("reset");
}

// ─── the absences ───────────────────────────────────────────────────────────

async fn a_licence_and_a_category_are_absences_of_rows(pool: &PgPool) {
    let bare = asset(pool, "bare").await;
    let licensed = asset(pool, "licensed").await;
    let filed = asset(pool, "filed").await;

    let licence: Uuid = sqlx::query_scalar(
        "INSERT INTO licenses (id, name, license_type) \
         VALUES (gen_random_uuid(), 'Stock', 'royalty_free') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("licence");
    sqlx::query("INSERT INTO asset_licenses (asset_id, license_id) VALUES ($1, $2)")
        .bind(licensed)
        .bind(licence)
        .execute(pool)
        .await
        .expect("applied");

    let (subject, harbour) = category(pool, "subject", "harbour").await;
    sqlx::query(
        "INSERT INTO asset_tags (asset_id, term_id, state, source) \
         VALUES ($1, $2, 'confirmed', 'human')",
    )
    .bind(filed)
    .bind(harbour)
    .execute(pool)
    .await
    .expect("filed");
    let _ = subject;

    let no_licence = ids_on(pool, Worklist::NoLicence).await;
    assert!(no_licence.contains(&bare) && no_licence.contains(&filed));
    assert!(!no_licence.contains(&licensed));

    let uncategorised = ids_on(pool, Worklist::Uncategorised).await;
    assert!(uncategorised.contains(&bare) && uncategorised.contains(&licensed));
    assert!(
        !uncategorised.contains(&filed),
        "one term is enough; this list is about none at all"
    );
}

async fn a_suggested_tag_is_not_filing_and_a_vocabulary_is_not_a_category(pool: &PgPool) {
    // The two ways an asset can look filed and not be. A *suggested* tag is a machine's guess waiting for
    // review — counting it as filed would empty this worklist the moment enrichment ran, which is precisely
    // when the reviewing has not happened. And a vocabulary is a label set for tagging rather than a filing
    // tree, which is why `categories::uncategorised` refuses a non-tree taxonomy.
    let guessed = asset(pool, "guessed").await;
    let labelled = asset(pool, "labelled").await;
    let (_, harbour) = category(pool, "second-tree", "quay").await;
    sqlx::query(
        "INSERT INTO asset_tags (asset_id, term_id, state, source, confidence) \
         VALUES ($1, $2, 'suggested', 'zero_shot', 0.4)",
    )
    .bind(guessed)
    .bind(harbour)
    .execute(pool)
    .await
    .expect("suggested");

    let vocabulary: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomies (id, key, label, kind) \
         VALUES (gen_random_uuid(), 'moods', 'Moods', 'vocabulary') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("vocabulary");
    let mood: Uuid = sqlx::query_scalar(
        "INSERT INTO taxonomy_terms (id, taxonomy_id, slug, label, path) \
         VALUES (gen_random_uuid(), $1, 'calm', 'Calm', 'calm'::extensions.ltree) RETURNING id",
    )
    .bind(vocabulary)
    .fetch_one(pool)
    .await
    .expect("term");
    sqlx::query(
        "INSERT INTO asset_tags (asset_id, term_id, state, source) \
         VALUES ($1, $2, 'confirmed', 'human')",
    )
    .bind(labelled)
    .bind(mood)
    .execute(pool)
    .await
    .expect("confirmed vocabulary tag");

    let uncategorised = ids_on(pool, Worklist::Uncategorised).await;
    assert!(
        uncategorised.contains(&guessed),
        "a suggestion is not filing"
    );
    assert!(
        uncategorised.contains(&labelled),
        "a confirmed vocabulary tag is a label, not a category"
    );
}

async fn a_thumbnail_is_wanted_by_role_not_by_recipe(pool: &PgPool) {
    let drawn = asset(pool, "drawn").await;
    let grey = asset(pool, "grey").await;
    // A thumbnail from an *older* profile still draws, so the list must not demand the current op hash —
    // otherwise every asset predating a profile change appears as work nobody needs to do.
    sqlx::query(
        "INSERT INTO derivatives (id, asset_id, role, profile, op_hash, object_key, mime, bytes) \
         VALUES (gen_random_uuid(), $1, 'thumbnail', 'thumb-256', 'an-older-hash', 'k', 'image/webp', 1)",
    )
    .bind(drawn)
    .execute(pool)
    .await
    .expect("derivative");
    // A preview is not a thumbnail: the grid draws the thumbnail, so this asset is still a grey square.
    sqlx::query(
        "INSERT INTO derivatives (id, asset_id, role, profile, op_hash, object_key, mime, bytes) \
         VALUES (gen_random_uuid(), $1, 'preview', 'preview-1024', 'h2', 'k2', 'image/webp', 1)",
    )
    .bind(grey)
    .execute(pool)
    .await
    .expect("derivative");

    let missing = ids_on(pool, Worklist::NoThumbnail).await;
    assert!(!missing.contains(&drawn), "a stale thumbnail still draws");
    assert!(
        missing.contains(&grey),
        "a preview is not what the grid draws"
    );
}

async fn enrichment_failure_is_not_the_pending_queue(pool: &PgPool) {
    let failed = asset(pool, "failed").await;
    let waiting = asset(pool, "waiting").await;
    let reviewing = asset(pool, "reviewing").await;
    for (id, state) in [
        (failed, "failed"),
        (waiting, "pending"),
        (reviewing, "needs_review"),
    ] {
        sqlx::query("UPDATE assets SET enrichment_state = $2 WHERE id = $1")
            .bind(id)
            .bind(state)
            .execute(pool)
            .await
            .expect("state");
    }

    // Only the one that stopped. `pending` is a queue that will drain on its own, and `needs_review` has the
    // review screen — a worklist row for it would be a second front door that cannot do the work.
    assert_eq!(ids_on(pool, Worklist::EnrichmentFailed).await, vec![failed]);
}

// ─── required metadata ──────────────────────────────────────────────────────

async fn required_is_resolved_through_the_assets_own_type(pool: &PgPool) {
    // Two required fields, and two types that include one each. So the same empty `caption` is a gap for the
    // photo type's asset and nothing at all for the document type's — which is the case a query that joined
    // `field_defs` on `required` alone would get wrong.
    for key in ["caption", "author"] {
        sqlx::query(
            "INSERT INTO field_defs (id, key, label, kind, required) \
             VALUES (gen_random_uuid(), $1, $1, 'text', true)",
        )
        .bind(key)
        .execute(pool)
        .await
        .expect("field");
    }
    let photo: Uuid = sqlx::query_scalar(
        "INSERT INTO metadata_types (id, key, label) \
         VALUES (gen_random_uuid(), 'photo', 'Photo') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("type");
    let paper: Uuid = sqlx::query_scalar(
        "INSERT INTO metadata_types (id, key, label) \
         VALUES (gen_random_uuid(), 'paper', 'Paper') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("type");
    sqlx::query(
        "INSERT INTO metadata_type_fields (metadata_type_id, field_key) VALUES ($1, 'caption')",
    )
    .bind(photo)
    .execute(pool)
    .await
    .expect("membership");
    sqlx::query(
        "INSERT INTO metadata_type_fields (metadata_type_id, field_key) VALUES ($1, 'author')",
    )
    .bind(paper)
    .execute(pool)
    .await
    .expect("membership");

    let photo_gap = asset(pool, "photo-gap").await;
    let photo_done = asset(pool, "photo-done").await;
    let paper_asset = asset(pool, "paper").await;
    sqlx::query("UPDATE assets SET metadata_type_id = $2 WHERE id = $1")
        .bind(photo_gap)
        .bind(photo)
        .execute(pool)
        .await
        .expect("typed");
    sqlx::query("UPDATE assets SET metadata_type_id = $2 WHERE id = $1")
        .bind(photo_done)
        .bind(photo)
        .execute(pool)
        .await
        .expect("typed");
    sqlx::query("UPDATE assets SET metadata_type_id = $2 WHERE id = $1")
        .bind(paper_asset)
        .bind(paper)
        .execute(pool)
        .await
        .expect("typed");

    sqlx::query(
        "INSERT INTO asset_metadata (asset_id, values) VALUES ($1, '{\"caption\": \"A harbour\"}'::jsonb)",
    )
    .bind(photo_done)
    .execute(pool)
    .await
    .expect("values");
    // The paper asset has a caption and no author: its own type does not include `caption`, so filling it
    // changes nothing and the missing `author` is what counts.
    sqlx::query(
        "INSERT INTO asset_metadata (asset_id, values) VALUES ($1, '{\"caption\": \"A memo\"}'::jsonb)",
    )
    .bind(paper_asset)
    .execute(pool)
    .await
    .expect("values");

    let missing = ids_on(pool, Worklist::MissingRequired).await;
    assert!(
        missing.contains(&photo_gap),
        "an empty required field of its own type"
    );
    assert!(
        !missing.contains(&photo_done),
        "filled, and the other required field belongs to a type this asset does not have"
    );
    assert!(
        missing.contains(&paper_asset),
        "its own type's required field is the one that is empty"
    );
}

async fn an_empty_string_or_list_is_still_missing(pool: &PgPool) {
    // What a form posts when somebody tabs through it. A `?` existence check calls both of these filled.
    let blank = asset(pool, "blank").await;
    let emptied = asset(pool, "emptied").await;
    let nulled = asset(pool, "nulled").await;
    let photo: Uuid = sqlx::query_scalar("SELECT id FROM metadata_types WHERE key = 'photo'")
        .fetch_one(pool)
        .await
        .expect("type");
    for (id, values) in [
        (blank, "{\"caption\": \"\"}"),
        (emptied, "{\"caption\": []}"),
        (nulled, "{\"caption\": null}"),
    ] {
        sqlx::query("UPDATE assets SET metadata_type_id = $2 WHERE id = $1")
            .bind(id)
            .bind(photo)
            .execute(pool)
            .await
            .expect("typed");
        sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2::jsonb)")
            .bind(id)
            .bind(values)
            .execute(pool)
            .await
            .expect("values");
    }

    let missing = ids_on(pool, Worklist::MissingRequired).await;
    for (id, what) in [(blank, "\"\""), (emptied, "[]"), (nulled, "null")] {
        assert!(
            missing.contains(&id),
            "{what} is not a filled required field"
        );
    }
}

async fn a_tenant_with_no_types_applies_every_required_field(pool: &PgPool) {
    // The last link in the resolution chain: no type on the asset and no default for the tenant means every
    // required field applies, which is what `metadata_types` says resolution does.
    let (_pg, fresh) = db().await;
    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind, required) \
         VALUES (gen_random_uuid(), 'caption', 'Caption', 'text', true)",
    )
    .execute(&fresh)
    .await
    .expect("field");
    let bare = asset(&fresh, "typeless").await;

    assert_eq!(ids_on(&fresh, Worklist::MissingRequired).await, vec![bare]);
    // And nothing else changed about the tenant: the other lists are computed from the same rows.
    let counts = counted(&fresh, &access(None)).await;
    assert_eq!(counts[&Worklist::Uncategorised], 1);
    assert_eq!(counts[&Worklist::Expired], 0);
    let _ = pool;
}

// ─── scope, versions, and the counts themselves ─────────────────────────────

async fn every_count_is_the_callers_own(pool: &PgPool) {
    let (_pg, fresh) = db().await;
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) \
         VALUES (gen_random_uuid(), 'mine', 'Mine') RETURNING id",
    )
    .fetch_one(&fresh)
    .await
    .expect("group");
    let mine = asset(&fresh, "mine").await;
    asset(&fresh, "theirs").await;
    sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
        .bind(group)
        .bind(mine)
        .execute(&fresh)
        .await
        .expect("member");

    let wide = counted(&fresh, &access(None)).await;
    let narrow = counted(&fresh, &access(Some(&[group]))).await;
    let nothing = counted(&fresh, &access(Some(&[]))).await;

    assert_eq!(wide[&Worklist::Uncategorised], 2);
    assert_eq!(
        narrow[&Worklist::Uncategorised],
        1,
        "a scoped reader's worklist is their own work, not the library's"
    );
    assert_eq!(nothing[&Worklist::Uncategorised], 0);
    // The page agrees with the count it sits under, which is the §7 property: a total computed over a wider
    // set than the rows would disclose how much is out of reach.
    let page = worklists::page(
        &fresh,
        &access(Some(&[group])),
        Worklist::Uncategorised,
        dam_db::assets::Order::Oldest,
        0,
        50,
    )
    .await
    .expect("page");
    assert_eq!(page.total, 1);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, mine);
    let _ = pool;
}

async fn an_old_version_is_not_a_second_job(pool: &PgPool) {
    let (_pg, fresh) = db().await;
    let current = asset(&fresh, "current").await;
    // A superseded version of the same asset, and an attachment hanging off it. Neither is a row the library
    // shows, so neither is work: a worklist that counted them would report three jobs for one photograph.
    // The current row becomes v2 first: `assets_version_idx` is unique on (group, version_no), so the
    // superseded row needs the number the current one was born with.
    sqlx::query("UPDATE assets SET version_no = 2 WHERE id = $1")
        .bind(current)
        .execute(&fresh)
        .await
        .expect("promote");
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id, \
                             version_no, is_current) \
         VALUES (gen_random_uuid(), 'older', 'current.jpg', 'image/jpeg', 4096, \
                 (SELECT version_group_id FROM assets WHERE id = $1), 1, false)",
    )
    .bind(current)
    .execute(&fresh)
    .await
    .expect("older version");
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id, \
                             attached_to, attachment_kind) \
         VALUES (gen_random_uuid(), 'release-form', 'release.pdf', 'application/pdf', 100, \
                 gen_random_uuid(), $1, 'release')",
    )
    .bind(current)
    .execute(&fresh)
    .await
    .expect("attachment");

    let counts = counted(&fresh, &access(None)).await;
    assert_eq!(
        counts[&Worklist::Uncategorised],
        1,
        "one photograph is one job, whatever else hangs off it"
    );
    let _ = pool;
}

async fn the_rights_lists_read_the_stored_verdict(pool: &PgPool) {
    // Not recomputed here. `rights_eval` needs the licences, the releases, the intended usage and each
    // licence's own notice window — a second implementation in SQL would be a second rights engine, and the
    // two would disagree the day either changed. So these lists read the column the engine wrote, which is
    // also the column the grid badge renders.
    let (_pg, fresh) = db().await;
    let ending = asset(&fresh, "ending").await;
    let forbidden = asset(&fresh, "forbidden").await;
    let fine = asset(&fresh, "fine").await;
    for (id, state) in [
        (ending, "expiring"),
        (forbidden, "denied"),
        (fine, "allowed"),
    ] {
        sqlx::query("UPDATE assets SET rights_state = $2 WHERE id = $1")
            .bind(id)
            .bind(state)
            .execute(&fresh)
            .await
            .expect("state");
    }

    assert_eq!(ids_on(&fresh, Worklist::RightsExpiring).await, vec![ending]);
    assert_eq!(
        ids_on(&fresh, Worklist::RightsDenied).await,
        vec![forbidden]
    );
    // And neither is the scheduled-expiry list, which asks a different column entirely: a contract term
    // ending is not a retention date arriving.
    assert!(ids_on(&fresh, Worklist::ExpiringSoon).await.is_empty());
    assert!(ids_on(&fresh, Worklist::Expired).await.is_empty());
    let _ = pool;
}

async fn a_key_round_trips_and_a_typo_does_not(_pool: &PgPool) {
    for worklist in Worklist::all() {
        assert_eq!(Worklist::from_key(worklist.key()), Some(worklist));
    }
    // A 404 rather than a default: a mistyped URL that quietly showed a *different* list would have somebody
    // working through the wrong backlog.
    assert_eq!(Worklist::from_key("uncategorized"), None);
    assert_eq!(Worklist::from_key(""), None);
}

#[tokio::test]
async fn the_worklists_are_the_libraries_own_gaps() {
    let (_pg, pool) = db().await;

    expiry_splits_into_exposure_and_task(&pool).await;
    an_embargo_is_a_future_release_date(&pool).await;
    a_licence_and_a_category_are_absences_of_rows(&pool).await;
    a_suggested_tag_is_not_filing_and_a_vocabulary_is_not_a_category(&pool).await;
    a_thumbnail_is_wanted_by_role_not_by_recipe(&pool).await;
    enrichment_failure_is_not_the_pending_queue(&pool).await;
    required_is_resolved_through_the_assets_own_type(&pool).await;
    an_empty_string_or_list_is_still_missing(&pool).await;
    a_tenant_with_no_types_applies_every_required_field(&pool).await;
    every_count_is_the_callers_own(&pool).await;
    an_old_version_is_not_a_second_job(&pool).await;
    the_rights_lists_read_the_stored_verdict(&pool).await;
    a_key_round_trips_and_a_typo_does_not(&pool).await;
}
