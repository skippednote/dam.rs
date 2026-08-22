//! The download half of the consumption ledger (Q.12a).
//!
//! `rights_usage` has existed since migration 0005 with a comment naming download events as one of its three
//! sources, and nothing wrote them. What that cost is the point of this suite: `license_scopes.max_downloads`
//! was decoration, exactly as the same comment warned `max_impressions` would be.
//!
//! - **A recorded download makes a cap bite.** The evaluator already sums this ledger against `max_downloads`;
//!   writing it is what closes the loop, and the case here drives the cap to exhaustion through the ledger.
//! - **A declaration is distinguishable from a default.** An audit that cannot tell "somebody said print" from
//!   "nobody asked and the API said internal" is not an audit, and the database refuses to let a non-download
//!   claim one.
//! - **The ledger is read under the caller's predicate**, so a row cannot disclose an asset.
//! - **The vocabulary is what the licences reference**, exclusions included: "worldwide except China" makes `CN`
//!   a territory worth declaring, and the honest answer to declaring it is a refusal with a reason.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::rights::RightsState;
use dam_core::rights_eval::Usage;
use dam_db::usage::{self, NewDownload};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

macro_rules! c {
    ($pool:expr) => {
        &mut *$pool.acquire().await.expect("connection")
    };
}

fn everything() -> policy::AccessPredicate {
    policy::compile(
        &Grants::from(vec![Grant {
            permissions: vec!["asset:read".to_owned(), "asset:download".to_owned()],
            asset_group_ids: vec![],
            all_asset_groups: true,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        }]),
        Action::Download,
        Utc::now(),
    )
}

fn scoped(group: Uuid) -> policy::AccessPredicate {
    policy::compile(
        &Grants::from(vec![Grant {
            permissions: vec!["asset:read".to_owned(), "asset:download".to_owned()],
            asset_group_ids: vec![group],
            all_asset_groups: false,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        }]),
        Action::Download,
        Utc::now(),
    )
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

/// A licence with one scope, optionally capped, returning the scope id.
async fn licence(pool: &PgPool, asset_id: Uuid, cap: Option<i64>) -> Uuid {
    let license_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO licenses (id, name, license_type, perpetual) \
         VALUES ($1, 'worldwide', 'royalty_free', true)",
    )
    .bind(license_id)
    .execute(pool)
    .await
    .expect("licence");
    let scope_id: Uuid = sqlx::query_scalar(
        "INSERT INTO license_scopes (id, license_id, territories, max_downloads) \
         VALUES (gen_random_uuid(), $1, '{WORLD}', $2) RETURNING id",
    )
    .bind(license_id)
    .bind(cap)
    .fetch_one(pool)
    .await
    .expect("scope");
    sqlx::query("INSERT INTO asset_licenses (asset_id, license_id) VALUES ($1, $2)")
        .bind(asset_id)
        .bind(license_id)
        .execute(pool)
        .await
        .expect("attach");
    scope_id
}

fn web() -> Usage {
    Usage {
        channel: "web".to_owned(),
        territory: "WORLD".to_owned(),
    }
}

#[tokio::test]
async fn the_ledger_behaves() {
    let (_pg, pool) = db().await;

    a_recorded_download_exhausts_a_cap(&pool).await;
    a_declaration_is_distinguishable_from_a_default(&pool).await;
    the_ledger_is_read_under_the_predicate(&pool).await;
    a_non_download_cannot_claim_a_declaration(&pool).await;
    the_vocabulary_is_what_the_licences_reference(&pool).await;
}

async fn a_recorded_download_exhausts_a_cap(pool: &PgPool) {
    // The whole point. `max_downloads` was summed against this ledger from the day it was written and nothing
    // ever wrote a download, so a cap of two permitted an unlimited number.
    let id = asset(pool, "capped").await;
    let scope = licence(pool, id, Some(2)).await;

    let first = dam_db::rights::evaluate(pool, id, &web(), Utc::now())
        .await
        .expect("evaluate");
    assert_eq!(first.verdict, RightsState::Allowed);
    assert_eq!(first.downloads_remaining, Some(2));
    assert_eq!(
        first.consuming_scope,
        Some(scope),
        "the download would be attributed to nothing, so the cap would never move"
    );

    for taken in 1..=2 {
        usage::record_download(
            c!(pool),
            &NewDownload {
                asset_id: id,
                channel: "web".to_owned(),
                territory: "WORLD".to_owned(),
                license_scope_id: first.consuming_scope,
                declared: true,
                recorded_by: Some(Uuid::new_v4()),
            },
        )
        .await
        .expect("record");

        // Re-evaluated, not cached: `evaluate` recomputes and refreshes, which is what the download path does
        // when it needs the scope.
        let after = dam_db::rights::evaluate(pool, id, &web(), Utc::now())
            .await
            .expect("evaluate");
        if taken < 2 {
            assert_eq!(after.downloads_remaining, Some(2 - taken), "{after:?}");
            assert_eq!(after.verdict, RightsState::Allowed);
        } else {
            // Exhausted. The scope stops covering the usage, and with no other scope the licence permits
            // nothing — which is a cap that finally refuses.
            assert_eq!(after.verdict, RightsState::Denied, "{after:?}");
            assert!(
                after
                    .reasons
                    .iter()
                    .any(|reason| reason.code == "downloads_exhausted"),
                "{after:?}"
            );
            assert_eq!(after.consuming_scope, None);
        }
    }
}

async fn a_declaration_is_distinguishable_from_a_default(pool: &PgPool) {
    let id = asset(pool, "declared").await;
    licence(pool, id, None).await;

    usage::record_download(
        c!(pool),
        &NewDownload {
            asset_id: id,
            channel: "print".to_owned(),
            territory: "GB".to_owned(),
            license_scope_id: None,
            declared: true,
            recorded_by: Some(Uuid::new_v4()),
        },
    )
    .await
    .expect("record");
    usage::record_download(
        c!(pool),
        &NewDownload {
            asset_id: id,
            channel: "internal".to_owned(),
            territory: "WORLD".to_owned(),
            license_scope_id: None,
            declared: false,
            recorded_by: None,
        },
    )
    .await
    .expect("record");

    let rows = usage::for_asset(c!(pool), id, &everything(), 50)
        .await
        .expect("read");
    assert_eq!(rows.len(), 2, "{rows:?}");
    // Newest first, and the two are told apart. An audit that could not distinguish them would let "we asked
    // everybody" be claimed on the strength of rows nobody ever saw.
    let declared = rows
        .iter()
        .find(|row| row.declared)
        .expect("the stated one");
    assert_eq!(declared.channel.as_deref(), Some("print"));
    assert_eq!(declared.territory.as_deref(), Some("GB"));
    assert!(declared.recorded_by.is_some());
    let defaulted = rows
        .iter()
        .find(|row| !row.declared)
        .expect("the defaulted one");
    assert_eq!(defaulted.channel.as_deref(), Some("internal"));
    assert_eq!(defaulted.downloads, 1);
}

async fn the_ledger_is_read_under_the_predicate(pool: &PgPool) {
    // The ledger names an asset, so reading it has to be scoped like anything else that does: an asset outside
    // the caller's scope has no ledger rather than an empty one.
    let id = asset(pool, "elsewhere").await;
    usage::record_download(
        c!(pool),
        &NewDownload {
            asset_id: id,
            channel: "web".to_owned(),
            territory: "WORLD".to_owned(),
            license_scope_id: None,
            declared: true,
            recorded_by: None,
        },
    )
    .await
    .expect("record");

    assert_eq!(
        usage::for_asset(c!(pool), id, &everything(), 50)
            .await
            .expect("read")
            .len(),
        1
    );
    let group: Uuid = sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), 'other', 'Other') \
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("group");
    let hidden = usage::for_asset(c!(pool), id, &scoped(group), 50)
        .await
        .expect("read");
    assert!(
        hidden.is_empty(),
        "a ledger leaked past the scope: {hidden:?}"
    );
}

async fn a_non_download_cannot_claim_a_declaration(pool: &PgPool) {
    // A connector report and a manual print-run entry record something that happened elsewhere, with no person
    // at a dialog. `declared` on one of those would be a claim about an event nobody witnessed, so the database
    // refuses it rather than the comment discouraging it.
    let id = asset(pool, "connector").await;
    let refused = sqlx::query(
        "INSERT INTO rights_usage (id, asset_id, channel, downloads, source, declared) \
         VALUES (gen_random_uuid(), $1, 'web', 1, 'connector', true)",
    )
    .bind(id)
    .execute(pool)
    .await;
    assert!(
        refused.is_err(),
        "a connector row claimed somebody declared its use"
    );

    // The same row without the claim is fine, and does not appear in the download ledger.
    sqlx::query(
        "INSERT INTO rights_usage (id, asset_id, channel, impressions, source) \
         VALUES (gen_random_uuid(), $1, 'web', 500, 'connector')",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("a connector report is ordinary");
    let rows = usage::for_asset(c!(pool), id, &everything(), 50)
        .await
        .expect("read");
    assert!(
        rows.is_empty(),
        "a connector report is in a list about people's stated intentions: {rows:?}"
    );
}

async fn the_vocabulary_is_what_the_licences_reference(pool: &PgPool) {
    // Derived rather than configured, so every option is one that can change a rights answer.
    let id = asset(pool, "vocab").await;
    let license_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO licenses (id, name, license_type, perpetual) \
         VALUES ($1, 'except china', 'rights_managed', true)",
    )
    .bind(license_id)
    .execute(pool)
    .await
    .expect("licence");
    sqlx::query(
        "INSERT INTO license_scopes \
         (id, license_id, territories, excluded_territories, channels, excluded_channels) \
         VALUES (gen_random_uuid(), $1, '{WORLD}', '{CN}', '{web,social}', '{ooh}')",
    )
    .bind(license_id)
    .execute(pool)
    .await
    .expect("scope");
    sqlx::query("INSERT INTO asset_licenses (asset_id, license_id) VALUES ($1, $2)")
        .bind(id)
        .bind(license_id)
        .execute(pool)
        .await
        .expect("attach");

    let (channels, territories) = usage::vocabulary(c!(pool)).await.expect("vocabulary");
    assert!(channels.contains(&"web".to_owned()), "{channels:?}");
    assert!(channels.contains(&"social".to_owned()), "{channels:?}");
    // Exclusions too: a channel a licence *forbids* is one somebody may want to declare, and the honest answer
    // is a refusal naming the exclusion rather than the option being missing.
    assert!(
        channels.contains(&"ooh".to_owned()),
        "an excluded channel is not offerable, so declaring it is impossible: {channels:?}"
    );
    assert!(
        territories.contains(&"CN".to_owned()),
        "an excluded territory is not offerable: {territories:?}"
    );
    assert!(territories.contains(&"WORLD".to_owned()), "{territories:?}");
    // Sorted and deduplicated: several licences naming `web` is one option.
    let mut sorted = channels.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(channels, sorted, "{channels:?}");
}
