//! The derivative cache (3.2).
//!
//! One property carries this: the cache is keyed on the **recipe**, not the profile name. `op_hash` covers
//! size, format, quality, fit, background, colour profile and rendering intent (§18.1), so a profile that
//! has been redefined has a different hash and misses. A name lookup would serve the bytes rendered under
//! the old definition forever — no error, nothing in a log, and a customer seeing yesterday's quality
//! setting indefinitely. 3.1 shipped with exactly that lookup.
//!
//! One container; the cases are functions over a borrowed pool.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_db::derivatives::{self, NewDerivative};
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
    .bind(blake3::hash(label.as_bytes()).to_hex().to_string())
    .bind(format!("{label}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

fn new_derivative<'a>(asset_id: Uuid, op_hash: &'a str, key: &'a str) -> NewDerivative<'a> {
    NewDerivative {
        asset_id,
        role: "proxy",
        profile: "web-2048",
        op_hash,
        object_key: key,
        mime: "image/jpeg",
        bytes: 4096,
        width: Some(2048),
        height: Some(1365),
        regen_cost_ms: Some(120),
    }
}

// ─── the recipe key ─────────────────────────────────────────────────────────

async fn a_derivative_is_found_by_its_recipe_not_its_name(pool: &PgPool) {
    let id = asset(pool, "recipe").await;
    let recorded = derivatives::record(pool, &new_derivative(id, "hash-a", "acme/p/a"))
        .await
        .expect("record");
    assert_eq!(recorded.op_hash, "hash-a");

    assert_eq!(
        derivatives::by_op_hash(pool, id, "hash-a")
            .await
            .expect("lookup")
            .map(|d| d.object_key),
        Some("acme/p/a".to_owned())
    );

    // The same profile name under a different recipe is a different derivative, and must not be found.
    assert!(
        derivatives::by_op_hash(pool, id, "hash-b")
            .await
            .expect("lookup")
            .is_none(),
        "a redefined profile must miss, or the cache serves stale bytes forever"
    );
}

async fn two_recipes_of_a_non_unique_role_coexist(pool: &PgPool) {
    // The same thumbnail profile before and after a redefinition. Both rows are valid: a URL already issued
    // against the old recipe still resolves, which is why a redefinition evicts nothing by itself.
    let id = asset(pool, "coexist").await;
    for (hash, key) in [("thumb-v1", "acme/t/v1"), ("thumb-v2", "acme/t/v2")] {
        derivatives::record(
            pool,
            &NewDerivative {
                role: "thumbnail",
                profile: "thumb-256",
                ..new_derivative(id, hash, key)
            },
        )
        .await
        .expect("record");
    }
    for hash in ["thumb-v1", "thumb-v2"] {
        assert!(
            derivatives::by_op_hash(pool, id, hash)
                .await
                .expect("lookup")
                .is_some(),
            "{hash} must survive alongside the other"
        );
    }
}

async fn a_second_master_proxy_is_refused_rather_than_silently_replacing(pool: &PgPool) {
    // `derivatives_proxy_idx` is `UNIQUE (asset_id) WHERE role = 'proxy'`: an asset has **one** master proxy,
    // because D5 makes it the search-and-AI substrate rather than one rendition among many. So a redefined
    // proxy cannot coexist with the old one the way a thumbnail can.
    //
    // An upsert here would look tidier and would orphan an object on every proxy redefinition, with nothing
    // recording that the old key still exists. This found the constraint the hard way — the first version of
    // the test above used `role = 'proxy'` twice and hit the index.
    let id = asset(pool, "one-proxy").await;
    derivatives::record(pool, &new_derivative(id, "proxy-v1", "acme/p/v1"))
        .await
        .expect("the first proxy");

    let refused = derivatives::record(pool, &new_derivative(id, "proxy-v2", "acme/p/v2")).await;
    assert!(
        refused.is_err(),
        "a second master proxy must be refused, naming `replace_proxy` as the way through"
    );

    // The original is untouched, which is the point of refusing rather than replacing halfway.
    assert_eq!(
        derivatives::current_proxy(pool, id)
            .await
            .expect("current")
            .map(|d| d.object_key),
        Some("acme/p/v1".to_owned())
    );
}

async fn replacing_the_proxy_reports_the_object_it_orphaned(pool: &PgPool) {
    // The row and the object live in different systems. Deleting the object here would leave a placement
    // pointing at nothing if the transaction then failed, so the key is handed back and the caller reclaims
    // after committing.
    let id = asset(pool, "replace-proxy").await;
    let (first, orphaned) = derivatives::replace_proxy(pool, &new_derivative(id, "p1", "acme/p/1"))
        .await
        .expect("first proxy");
    assert!(orphaned.is_none(), "the first proxy supersedes nothing");
    assert_eq!(first.object_key, "acme/p/1");

    let (second, orphaned) =
        derivatives::replace_proxy(pool, &new_derivative(id, "p2", "acme/p/2"))
            .await
            .expect("replacement");
    assert_eq!(second.object_key, "acme/p/2");
    assert_eq!(
        orphaned.as_deref(),
        Some("acme/p/1"),
        "the superseded object must be reported so it can be reclaimed"
    );

    // Still exactly one proxy row.
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM derivatives WHERE asset_id = $1 AND role = 'proxy'",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("count");
    assert_eq!(count, 1);
}

async fn replacing_the_proxy_with_the_same_recipe_orphans_nothing(pool: &PgPool) {
    // A retried render must not report an object for deletion that is still in use — which would delete the
    // bytes the row points at.
    let id = asset(pool, "idempotent-proxy").await;
    derivatives::replace_proxy(pool, &new_derivative(id, "same", "acme/p/same"))
        .await
        .expect("first");
    let (_, orphaned) =
        derivatives::replace_proxy(pool, &new_derivative(id, "same", "acme/p/same"))
            .await
            .expect("retry");
    assert!(
        orphaned.is_none(),
        "a retry must not report the live object as orphaned"
    );
}

async fn recording_the_same_recipe_twice_keeps_the_first_object(pool: &PgPool) {
    // Two workers can render the same derivative concurrently. They produce byte-identical output for the
    // same recipe, so the loser's row is redundant — but overwriting `object_key` would repoint the row at a
    // second identical object and orphan the first, which the reaper has no way to find.
    let id = asset(pool, "concurrent").await;
    let first = derivatives::record(pool, &new_derivative(id, "same", "acme/p/first"))
        .await
        .expect("first");
    let second = derivatives::record(pool, &new_derivative(id, "same", "acme/p/second"))
        .await
        .expect("second");

    assert_eq!(first.id, second.id, "the same recipe must be one row");
    assert_eq!(
        second.object_key, "acme/p/first",
        "the second render must not repoint the row and orphan the first object"
    );

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM derivatives WHERE asset_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("count");
    assert_eq!(count, 1);
}

async fn a_deleted_assets_derivative_is_not_found(pool: &PgPool) {
    // The join on `assets` is what makes this true, and it matters because a delivery URL outlives a delete.
    let id = asset(pool, "deleted").await;
    derivatives::record(pool, &new_derivative(id, "gone", "acme/p/gone"))
        .await
        .expect("record");
    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("delete");

    assert!(
        derivatives::by_op_hash(pool, id, "gone")
            .await
            .expect("lookup")
            .is_none()
    );
}

// ─── serve accounting ───────────────────────────────────────────────────────

async fn a_serve_is_written_at_most_once_per_window(pool: &PgPool) {
    // The lifecycle engine reads `last_served_at`. Writing it per delivery turns the hottest read path into
    // a write and costs a row of WAL per download.
    let id = asset(pool, "throttled").await;
    let d = derivatives::record(pool, &new_derivative(id, "throttle", "acme/p/t"))
        .await
        .expect("record");

    assert!(
        derivatives::mark_served(pool, d.id, now())
            .await
            .expect("mark"),
        "the first serve writes"
    );
    assert!(
        !derivatives::mark_served(pool, d.id, now() + Duration::minutes(30))
            .await
            .expect("mark"),
        "a serve inside the window does not"
    );
    assert!(
        derivatives::mark_served(
            pool,
            d.id,
            now() + derivatives::SERVED_RESOLUTION + Duration::seconds(1)
        )
        .await
        .expect("mark"),
        "and one past it does"
    );

    let served: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT last_served_at FROM derivatives WHERE id = $1")
            .bind(d.id)
            .fetch_one(pool)
            .await
            .expect("read");
    assert_eq!(
        served,
        Some(now() + derivatives::SERVED_RESOLUTION + Duration::seconds(1))
    );
}

async fn marking_a_derivative_that_does_not_exist_is_not_an_error(pool: &PgPool) {
    // It races a deletion, which is ordinary. Failing here would fail a delivery whose bytes were already
    // authorised and sent.
    assert!(
        !derivatives::mark_served(pool, Uuid::new_v4(), now())
            .await
            .expect("mark")
    );
}

// ─── eviction candidates ────────────────────────────────────────────────────

async fn superseded_lists_recipes_no_current_profile_produces(pool: &PgPool) {
    // The eviction list after a redefinition. Coldest first, because that is the order worth reclaiming in.
    //
    // Thumbnails rather than proxies: two proxy rows for one asset cannot exist (D5), so the coexisting-old-
    // and-new case only arises for the roles that allow it.
    let id = asset(pool, "superseded").await;
    let thumb = |hash, key| NewDerivative {
        role: "thumbnail",
        profile: "thumb-256",
        ..new_derivative(id, hash, key)
    };
    derivatives::record(pool, &thumb("current", "acme/t/cur"))
        .await
        .expect("record");
    let old = derivatives::record(pool, &thumb("old", "acme/t/old"))
        .await
        .expect("record");

    // Filtered to this asset. `superseded` is tenant-wide by design — the caller is a worker walking the
    // whole cache — so earlier cases in this driver contribute rows, and an unfiltered count would be
    // asserting about them rather than about this one.
    let stale: Vec<_> = derivatives::superseded(pool, &["current".to_owned()], 100)
        .await
        .expect("superseded")
        .into_iter()
        .filter(|d| d.asset_id == id)
        .collect();
    assert_eq!(stale.len(), 1, "got {stale:?}");
    assert_eq!(stale[0].id, old.id);
}

async fn an_empty_profile_set_is_refused_rather_than_proposing_to_evict_everything(pool: &PgPool) {
    // `<> ALL('{}')` is true for every row, so an empty current set would propose deleting the whole cache.
    // That is a configuration failure — "no profiles are defined" — not an eviction plan, and the difference
    // is every derivative in the tenant.
    let id = asset(pool, "empty-set").await;
    derivatives::record(
        pool,
        &NewDerivative {
            role: "thumbnail",
            profile: "thumb-256",
            ..new_derivative(id, "something", "acme/t/s")
        },
    )
    .await
    .expect("record");

    assert!(
        derivatives::superseded(pool, &[], 100).await.is_err(),
        "an empty profile set must be refused, not treated as an instruction to evict everything"
    );
}

async fn the_built_in_profiles_are_all_current(pool: &PgPool) {
    // A live check that the registry and the cache agree: recording each built-in profile's own hash and
    // then asking what is superseded must return nothing. If a profile's hash were unstable between calls,
    // every derivative would look superseded immediately after being written.
    let id = asset(pool, "built-in").await;
    let hashes: Vec<String> = dam_media::profiles::ALL
        .iter()
        .map(dam_media::profiles::Profile::op_hash)
        .collect();
    for (index, profile) in dam_media::profiles::ALL.iter().enumerate() {
        let key = format!("acme/p/{}", profile.name);
        let new = NewDerivative {
            asset_id: id,
            role: profile.role,
            profile: profile.name,
            op_hash: &hashes[index],
            object_key: &key,
            mime: "image/webp",
            bytes: 1024,
            width: None,
            height: None,
            regen_cost_ms: None,
        };
        // The proxy is unique per asset, so it takes the replacing path; everything else is recorded.
        if profile.role == "proxy" {
            derivatives::replace_proxy(pool, &new)
                .await
                .expect("replace the proxy");
        } else {
            derivatives::record(pool, &new).await.expect("record");
        }
    }

    let stale = derivatives::superseded(pool, &hashes, 100)
        .await
        .expect("superseded");
    assert!(
        stale.iter().all(|d| d.asset_id != id),
        "a derivative written from a current profile must not be superseded: {stale:?}"
    );
}

#[tokio::test]
async fn the_derivative_cache_invariants_hold() {
    let (_pg, pool) = db().await;

    a_derivative_is_found_by_its_recipe_not_its_name(&pool).await;
    two_recipes_of_a_non_unique_role_coexist(&pool).await;
    a_second_master_proxy_is_refused_rather_than_silently_replacing(&pool).await;
    replacing_the_proxy_reports_the_object_it_orphaned(&pool).await;
    replacing_the_proxy_with_the_same_recipe_orphans_nothing(&pool).await;
    recording_the_same_recipe_twice_keeps_the_first_object(&pool).await;
    a_deleted_assets_derivative_is_not_found(&pool).await;

    a_serve_is_written_at_most_once_per_window(&pool).await;
    marking_a_derivative_that_does_not_exist_is_not_an_error(&pool).await;

    superseded_lists_recipes_no_current_profile_produces(&pool).await;
    an_empty_profile_set_is_refused_rather_than_proposing_to_evict_everything(&pool).await;
    the_built_in_profiles_are_all_current(&pool).await;
}
