//! Loading and caching effective rights (2.8), against a real database.
//!
//! The calculation is tested purely in `dam-core`. What needs a database is the part that makes the D12
//! chokepoint affordable: loading a five-table input set, caching the verdict, and — the property that
//! actually matters — **never serving a stale `allowed`**.
//!
//! One container; the cases are functions over a borrowed pool.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_core::rights::RightsState;
use dam_core::rights_eval::Usage;
use dam_db::rights;
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap()
}

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

async fn asset(pool: &PgPool, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(format!("blake3:{label}"))
    .bind(format!("{label}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

/// A perpetual licence with one scope, attached to `asset_id`.
async fn licence(
    pool: &PgPool,
    asset_id: Uuid,
    name: &str,
    territories: &[&str],
    excluded: &[&str],
    channels: &[&str],
    ends_at: Option<DateTime<Utc>>,
) -> (Uuid, Uuid) {
    let license_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO licenses (id, name, license_type, perpetual, ends_at) \
         VALUES ($1, $2, 'rights_managed', $3, $4)",
    )
    .bind(license_id)
    .bind(name)
    .bind(ends_at.is_none())
    .bind(ends_at)
    .execute(pool)
    .await
    .expect("licence");

    let scope_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO license_scopes \
         (id, license_id, territories, excluded_territories, channels) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(scope_id)
    .bind(license_id)
    .bind(
        territories
            .iter()
            .map(|t| (*t).to_owned())
            .collect::<Vec<String>>(),
    )
    .bind(
        excluded
            .iter()
            .map(|t| (*t).to_owned())
            .collect::<Vec<String>>(),
    )
    .bind(
        channels
            .iter()
            .map(|c| (*c).to_owned())
            .collect::<Vec<String>>(),
    )
    .execute(pool)
    .await
    .expect("scope");

    sqlx::query("INSERT INTO asset_licenses (asset_id, license_id) VALUES ($1, $2)")
        .bind(asset_id)
        .bind(license_id)
        .execute(pool)
        .await
        .expect("attach");
    (license_id, scope_id)
}

fn web(territory: &str) -> Usage {
    Usage {
        channel: "web".to_owned(),
        territory: territory.to_owned(),
    }
}

// ─── loading ────────────────────────────────────────────────────────────────

async fn a_five_table_input_set_loads_into_one_evaluation(pool: &PgPool) {
    // The point of `inputs_for`: the licence, its scopes, the releases and the usage ledger arrive as one
    // value the pure calculation can take. Four queries rather than one join, because joining unrelated
    // shapes returns the cross product — three scopes and four releases become twelve rows to de-duplicate.
    let id = asset(pool, "loaded").await;
    licence(pool, id, "worldwide", &["WORLD"], &[], &[], None).await;

    let inputs = rights::inputs_for(pool, id).await.expect("inputs");
    assert_eq!(inputs.licenses.len(), 1);
    assert_eq!(inputs.licenses[0].scopes.len(), 1);
    assert!(!inputs.legal_hold);

    let outcome = dam_core::rights_eval::evaluate(&inputs, &web("GB"), now());
    assert_eq!(outcome.verdict, RightsState::Allowed);
}

async fn a_scope_belongs_only_to_its_own_licence(pool: &PgPool) {
    // Two licences on one asset, each with its own scope. Grouping the scopes by asset instead of by
    // licence would give each licence both scopes — and since scopes within a licence are alternatives,
    // that would silently widen the restrictive one to cover the permissive one's territory.
    let id = asset(pool, "two-licences").await;
    licence(pool, id, "gb only", &["GB"], &[], &[], None).await;
    licence(pool, id, "us only", &["US"], &[], &[], None).await;

    let inputs = rights::inputs_for(pool, id).await.expect("inputs");
    assert_eq!(inputs.licenses.len(), 2);
    for license in &inputs.licenses {
        assert_eq!(
            license.scopes.len(),
            1,
            "each licence must carry only its own scope, or the intersection is not one"
        );
    }

    // And the intersection denies both territories, since neither licence covers the other's.
    for territory in ["GB", "US"] {
        assert_eq!(
            dam_core::rights_eval::evaluate(&inputs, &web(territory), now()).verdict,
            RightsState::Denied,
            "{territory} is outside one of the two licences"
        );
    }
}

async fn usage_is_summed_per_scope(pool: &PgPool) {
    // The ledger is append-only by design — a counter cannot be audited or corrected — so the totals are
    // computed here. Summing across scopes instead of per scope would let consumption on one contract
    // exhaust another's cap.
    let id = asset(pool, "used").await;
    let (_, scope) = licence(pool, id, "capped", &["WORLD"], &[], &[], None).await;
    for impressions in [100_i64, 250] {
        sqlx::query(
            "INSERT INTO rights_usage (id, asset_id, license_scope_id, impressions, source) \
             VALUES (gen_random_uuid(), $1, $2, $3, 'manual')",
        )
        .bind(id)
        .bind(scope)
        .bind(impressions)
        .execute(pool)
        .await
        .expect("usage");
    }

    let inputs = rights::inputs_for(pool, id).await.expect("inputs");
    let consumed = inputs
        .consumed
        .iter()
        .find(|(s, _)| *s == scope)
        .map(|(_, c)| *c)
        .expect("consumption for the scope");
    assert_eq!(consumed.impressions, 350);
}

async fn an_unknown_asset_is_not_found_rather_than_unlicensed(pool: &PgPool) {
    // The distinction matters: "no such asset" is a 404 and "no licence" is a verdict. Returning an empty
    // input set for a missing asset would report it as `unknown` — which reads as an asset awaiting
    // review rather than one that does not exist.
    assert!(rights::inputs_for(pool, Uuid::new_v4()).await.is_err());
}

// ─── the cache ──────────────────────────────────────────────────────────────

async fn a_verdict_is_cached_and_read_back(pool: &PgPool) {
    let id = asset(pool, "cached").await;
    licence(pool, id, "worldwide", &["WORLD"], &[], &[], None).await;

    let computed = rights::evaluate(pool, id, &web("GB"), now())
        .await
        .expect("evaluate");
    assert_eq!(computed.verdict, RightsState::Allowed);

    let hit = rights::cached(pool, id, &web("GB"), now())
        .await
        .expect("cached")
        .expect("a verdict must be cached after evaluation");
    assert_eq!(hit.verdict, RightsState::Allowed);
}

async fn an_expired_cache_row_is_not_served(pool: &PgPool) {
    // The property that makes the cache safe. `expires_at` is the earliest instant the verdict could
    // change on its own, so serving a row past it means serving an `allowed` for a licence that has ended.
    let id = asset(pool, "expiring-cache").await;
    licence(
        pool,
        id,
        "ends in ten days",
        &["WORLD"],
        &[],
        &[],
        Some(now() + Duration::days(10)),
    )
    .await;

    let computed = rights::evaluate(pool, id, &web("GB"), now())
        .await
        .expect("evaluate");
    assert_eq!(computed.verdict, RightsState::Expiring);
    assert_eq!(computed.expires_at, Some(now() + Duration::days(10)));

    // Fresh now.
    assert!(
        rights::cached(pool, id, &web("GB"), now())
            .await
            .expect("cached")
            .is_some()
    );
    // Stale once the licence has ended.
    assert!(
        rights::cached(pool, id, &web("GB"), now() + Duration::days(11))
            .await
            .expect("cached")
            .is_none(),
        "a row past its expiry must not be served — that is exactly the stale allowed"
    );

    // And `effective` recomputes rather than denying, giving the correct post-expiry verdict.
    assert_eq!(
        rights::effective(pool, id, &web("GB"), now() + Duration::days(11))
            .await
            .expect("effective"),
        RightsState::Denied
    );
}

async fn a_cold_cache_recomputes_rather_than_denying(pool: &PgPool) {
    // Failing closed on a miss would make the first download of the day fail for every asset, and people
    // would learn to retry instead of read the error.
    let id = asset(pool, "cold").await;
    licence(pool, id, "worldwide", &["WORLD"], &[], &[], None).await;

    assert!(
        rights::cached(pool, id, &web("GB"), now())
            .await
            .expect("cached")
            .is_none(),
        "nothing cached yet"
    );
    assert_eq!(
        rights::effective(pool, id, &web("GB"), now())
            .await
            .expect("effective"),
        RightsState::Allowed
    );
}

async fn each_channel_and_territory_is_cached_separately(pool: &PgPool) {
    // A verdict is per (asset, channel, territory). One cache entry per asset would make the last
    // evaluated usage answer for all of them — so a permitted UK download would authorise a Chinese one.
    let id = asset(pool, "per-usage").await;
    licence(pool, id, "not china", &["WORLD"], &["CN"], &[], None).await;

    assert_eq!(
        rights::effective(pool, id, &web("GB"), now())
            .await
            .expect("gb"),
        RightsState::Allowed
    );
    assert_eq!(
        rights::effective(pool, id, &web("CN"), now())
            .await
            .expect("cn"),
        RightsState::Denied,
        "the GB verdict must not authorise CN"
    );
    // And the GB entry is still allowed afterwards, so the CN evaluation did not overwrite it.
    assert_eq!(
        rights::cached(pool, id, &web("GB"), now())
            .await
            .expect("cached")
            .expect("present")
            .verdict,
        RightsState::Allowed
    );
}

async fn the_reasons_survive_the_round_trip(pool: &PgPool) {
    // A refusal without a reason is a support ticket. The codes are what let a UI explain the denial and
    // an operator fix it.
    let id = asset(pool, "denied").await;
    // No licence attached at all.
    let outcome = rights::evaluate(pool, id, &web("GB"), now())
        .await
        .expect("evaluate");
    assert_eq!(outcome.verdict, RightsState::Unknown);

    let hit = rights::cached(pool, id, &web("GB"), now())
        .await
        .expect("cached")
        .expect("present");
    let codes: Vec<&str> = hit
        .reasons
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|r| r["code"].as_str())
        .collect();
    assert_eq!(codes, vec!["no_license"]);
}

// ─── the denormalised mirror ────────────────────────────────────────────────

async fn only_the_default_usage_is_mirrored_onto_the_asset(pool: &PgPool) {
    // `assets.rights_state` exists so a list endpoint avoids a five-table join per row. Mirroring every
    // channel would make the last evaluated one win, and the badge would flip depending on which download
    // happened most recently.
    let id = asset(pool, "mirrored").await;
    licence(pool, id, "not china", &["WORLD"], &["CN"], &[], None).await;

    // The default usage: web / WORLD. WORLD is unsatisfiable here because CN is carved out, which is the
    // deliberately strict reading — the badge errs toward warning.
    rights::evaluate(
        pool,
        id,
        &Usage {
            channel: rights::DEFAULT_CHANNEL.to_owned(),
            territory: rights::DEFAULT_TERRITORY.to_owned(),
        },
        now(),
    )
    .await
    .expect("evaluate default");

    let (state, evaluated): (String, Option<DateTime<Utc>>) =
        sqlx::query_as("SELECT rights_state, rights_evaluated_at FROM assets WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("asset state");
    assert_eq!(state, "denied");
    assert!(evaluated.is_some());

    // A non-default usage must not touch the mirror, even though its verdict differs.
    rights::evaluate(pool, id, &web("GB"), now())
        .await
        .expect("evaluate gb");
    let after: String = sqlx::query_scalar("SELECT rights_state FROM assets WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("asset state");
    assert_eq!(
        after, "denied",
        "an allowed GB verdict must not overwrite the default-usage badge"
    );
}

async fn invalidation_clears_the_cache_and_resets_the_badge(pool: &PgPool) {
    // Called when a licence is edited or a release withdrawn. Leaving the badge at its old value would
    // show `allowed` after a revocation, which is worse than showing "not yet evaluated".
    let id = asset(pool, "invalidated").await;
    licence(pool, id, "worldwide", &["WORLD"], &[], &[], None).await;
    rights::evaluate(
        pool,
        id,
        &Usage {
            channel: rights::DEFAULT_CHANNEL.to_owned(),
            territory: rights::DEFAULT_TERRITORY.to_owned(),
        },
        now(),
    )
    .await
    .expect("evaluate");

    let dropped = rights::invalidate(pool, id).await.expect("invalidate");
    assert_eq!(dropped, 1);

    let state: String = sqlx::query_scalar("SELECT rights_state FROM assets WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("state");
    assert_eq!(state, "unknown");
    assert!(
        rights::cached(
            pool,
            id,
            &Usage {
                channel: rights::DEFAULT_CHANNEL.to_owned(),
                territory: rights::DEFAULT_TERRITORY.to_owned(),
            },
            now()
        )
        .await
        .expect("cached")
        .is_none()
    );
}

async fn the_stale_list_finds_rows_whose_verdict_could_have_changed(pool: &PgPool) {
    // What the worker walks. Without it, a verdict that lapses overnight stays `allowed` in the cache
    // until somebody happens to request it — and the request that recomputes it is the one that should
    // have been refused.
    let id = asset(pool, "stale").await;
    licence(
        pool,
        id,
        "ends soon",
        &["WORLD"],
        &[],
        &[],
        Some(now() + Duration::days(5)),
    )
    .await;
    rights::evaluate(pool, id, &web("GB"), now())
        .await
        .expect("evaluate");

    assert!(
        rights::stale(pool, now(), 100)
            .await
            .expect("stale")
            .is_empty(),
        "nothing is stale yet"
    );
    let overdue = rights::stale(pool, now() + Duration::days(6), 100)
        .await
        .expect("stale");
    assert_eq!(overdue.len(), 1);
    assert_eq!(overdue[0].0, id);
}

async fn a_perpetual_licence_leaves_no_expiry_to_poll(pool: &PgPool) {
    // A null `expires_at` means the verdict cannot change on its own, so the worker must not keep
    // revisiting it. Storing a far-future date instead would put every asset in the tenant on the stale
    // queue forever.
    let id = asset(pool, "perpetual").await;
    licence(pool, id, "forever", &["WORLD"], &[], &[], None).await;
    let outcome = rights::evaluate(pool, id, &web("GB"), now())
        .await
        .expect("evaluate");
    assert_eq!(outcome.expires_at, None);

    let far_future = now() + Duration::days(10_000);
    assert!(
        rights::stale(pool, far_future, 100)
            .await
            .expect("stale")
            .iter()
            .all(|(a, _)| *a != id),
        "a perpetual licence must never appear on the stale queue"
    );
}

async fn a_legal_hold_denies_through_the_loader(pool: &PgPool) {
    // The hold lives on the asset, so this checks the loader carries it — a calculation that never sees
    // the flag would report the licence's verdict and permit the download.
    let id = asset(pool, "held").await;
    licence(pool, id, "worldwide", &["WORLD"], &[], &[], None).await;
    sqlx::query("UPDATE assets SET legal_hold = true WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("hold");

    let outcome = rights::evaluate(pool, id, &web("GB"), now())
        .await
        .expect("evaluate");
    assert_eq!(outcome.verdict, RightsState::Denied);
    assert_eq!(outcome.reasons.first().map(|r| r.code), Some("legal_hold"));
}

#[tokio::test]
async fn the_rights_invariants_hold() {
    let (_pg, pool) = db().await;

    a_five_table_input_set_loads_into_one_evaluation(&pool).await;
    a_scope_belongs_only_to_its_own_licence(&pool).await;
    usage_is_summed_per_scope(&pool).await;
    an_unknown_asset_is_not_found_rather_than_unlicensed(&pool).await;

    a_verdict_is_cached_and_read_back(&pool).await;
    an_expired_cache_row_is_not_served(&pool).await;
    a_cold_cache_recomputes_rather_than_denying(&pool).await;
    each_channel_and_territory_is_cached_separately(&pool).await;
    the_reasons_survive_the_round_trip(&pool).await;

    only_the_default_usage_is_mirrored_onto_the_asset(&pool).await;
    invalidation_clears_the_cache_and_resets_the_badge(&pool).await;
    the_stale_list_finds_rows_whose_verdict_could_have_changed(&pool).await;
    a_perpetual_licence_leaves_no_expiry_to_poll(&pool).await;
    a_legal_hold_denies_through_the_loader(&pool).await;
}
