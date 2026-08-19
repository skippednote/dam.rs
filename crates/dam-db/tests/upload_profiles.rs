//! Upload profiles: what an upload arrives already knowing (Q.3).
//!
//! A profile answers three questions that are asked at three different times by three different pieces of the
//! system — which is the whole reason it is a row rather than a parameter:
//!
//! - the *uploader*, before any bytes move, needs the form and whether to insist on required fields;
//! - *finalise*, writing the asset, needs the defaults and the metadata type;
//! - *enrichment*, in a worker long afterwards, needs to know whether machine tagging was permitted at all.
//!
//! So the interesting cases here are about the defaults being real metadata rather than a blob — validated
//! against the tenant's schema when saved *and* when applied, because a field definition can change in between
//! and a default that has quietly become invalid must fail where somebody can see it.
//!
//! One container; cases are functions over a borrowed pool. See the note in `provenance.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::fields::{self, NewField};
use dam_db::upload_profiles::{self, NewProfile, ProfileRefusal};
use dam_db::{migrate, testing::PostgresHarness};
use serde_json::json;
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

fn text(key: &str) -> NewField {
    NewField {
        key: key.to_owned(),
        label: key.to_owned(),
        kind: "text".to_owned(),
        taxonomy_id: None,
        multivalued: false,
        required: false,
        read_only: false,
        searchable: true,
        facetable: false,
        ai_writable: false,
        search_alias: None,
        validation: json!({}),
    }
}

#[tokio::test]
async fn the_upload_profile_contract_holds() {
    let (_pg, pool) = db().await;

    fields::define(&pool, text("credit")).await.expect("credit");
    fields::define(
        &pool,
        NewField {
            kind: "int".to_owned(),
            ..text("shoot_year")
        },
    )
    .await
    .expect("shoot_year");

    a_profile_is_created_with_defaults(&pool).await;
    a_default_that_is_not_a_field_is_refused(&pool).await;
    a_default_of_the_wrong_kind_is_refused(&pool).await;
    a_read_only_field_cannot_be_defaulted(&pool).await;
    only_one_profile_is_the_fallback(&pool).await;
    the_fallback_is_what_an_unnamed_upload_gets(&pool).await;
    applying_defaults_fills_only_absent_keys(&pool).await;
    a_default_that_became_invalid_fails_where_it_can_be_seen(&pool).await;
    removing_a_profile_leaves_its_assets_alone(&pool).await;
}

async fn a_profile_is_created_with_defaults(pool: &PgPool) {
    let created = upload_profiles::create(
        pool,
        NewProfile {
            key: "press".to_owned(),
            label: "Press delivery".to_owned(),
            metadata_type_id: None,
            defaults: json!({ "credit": "Acme Press Office" }),
            require_complete: true,
            ai_tags_enabled: false,
            is_default: false,
        },
    )
    .await
    .expect("create");

    assert_eq!(created.key, "press");
    assert!(created.require_complete);
    // Off, and that matters beyond a checkbox: some deliveries arrive already described, and some must not be
    // machine-tagged at all. Enrichment reads this long after the upload session is gone.
    assert!(!created.ai_tags_enabled);
    assert_eq!(created.defaults["credit"], "Acme Press Office");

    // A duplicate key is refused by name rather than as a constraint violation: this reaches an administrator
    // naming an intake, and "press is already a profile" is the only version they can act on.
    let refusal = upload_profiles::create(
        pool,
        NewProfile {
            key: "press".to_owned(),
            label: "Press again".to_owned(),
            metadata_type_id: None,
            defaults: json!({}),
            require_complete: false,
            ai_tags_enabled: true,
            is_default: false,
        },
    )
    .await
    .expect_err("duplicate");
    assert!(matches!(&refusal, ProfileRefusal::DuplicateKey(key) if key == "press"));
}

async fn a_default_that_is_not_a_field_is_refused(pool: &PgPool) {
    // The whole point of validating at save time: a default naming a field nobody defined would sit in the
    // profile until an upload used it, and then either fail every upload or be silently dropped. Both are
    // worse than refusing here, where the person who typed it is still looking.
    let refusal = upload_profiles::create(
        pool,
        NewProfile {
            key: "typo".to_owned(),
            label: "Typo".to_owned(),
            metadata_type_id: None,
            defaults: json!({ "creditt": "Acme" }),
            require_complete: false,
            ai_tags_enabled: true,
            is_default: false,
        },
    )
    .await
    .expect_err("unknown field");
    let ProfileRefusal::InvalidDefaults(problems) = &refusal else {
        panic!("expected field problems, got {refusal:?}");
    };
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].key, "creditt");
    assert_eq!(problems[0].code, "unknown_field");
}

async fn a_default_of_the_wrong_kind_is_refused(pool: &PgPool) {
    // `shoot_year` is an int. A default of "last summer" would be stored as metadata that the validator would
    // reject if a human typed it — so the profile must not be able to write it either.
    let refusal = upload_profiles::create(
        pool,
        NewProfile {
            key: "loose".to_owned(),
            label: "Loose".to_owned(),
            metadata_type_id: None,
            defaults: json!({ "shoot_year": "last summer" }),
            require_complete: false,
            ai_tags_enabled: true,
            is_default: false,
        },
    )
    .await
    .expect_err("wrong kind");
    assert!(matches!(refusal, ProfileRefusal::InvalidDefaults(_)));
}

async fn a_read_only_field_cannot_be_defaulted(pool: &PgPool) {
    // A read-only field is maintained by the system — it describes the file rather than the tenant's intent.
    // A profile writing one would make the metadata disagree with the bytes, which is the exact reason the
    // validator refuses a human doing it.
    fields::define(
        pool,
        NewField {
            read_only: true,
            ..text("ingested_by")
        },
    )
    .await
    .expect("read-only field");

    let refusal = upload_profiles::create(
        pool,
        NewProfile {
            key: "sneaky".to_owned(),
            label: "Sneaky".to_owned(),
            metadata_type_id: None,
            defaults: json!({ "ingested_by": "a robot" }),
            require_complete: false,
            ai_tags_enabled: true,
            is_default: false,
        },
    )
    .await
    .expect_err("read-only");
    let ProfileRefusal::InvalidDefaults(problems) = &refusal else {
        panic!("expected field problems, got {refusal:?}");
    };
    assert_eq!(problems[0].code, "read_only");
}

async fn only_one_profile_is_the_fallback(pool: &PgPool) {
    let first = upload_profiles::create(
        pool,
        NewProfile {
            key: "studio".to_owned(),
            label: "Studio".to_owned(),
            metadata_type_id: None,
            defaults: json!({}),
            require_complete: false,
            ai_tags_enabled: true,
            is_default: true,
        },
    )
    .await
    .expect("studio");
    let second = upload_profiles::create(
        pool,
        NewProfile {
            key: "partner".to_owned(),
            label: "Partner".to_owned(),
            metadata_type_id: None,
            defaults: json!({}),
            require_complete: false,
            ai_tags_enabled: true,
            is_default: true,
        },
    )
    .await
    .expect("partner");

    // Claiming the fallback moves it rather than being refused: "make this the default" is the intent either
    // way, and two rows holding it would make an upload's treatment depend on row order.
    let defaults: Vec<String> =
        sqlx::query_scalar("SELECT key FROM upload_profiles WHERE is_default ORDER BY key")
            .fetch_all(pool)
            .await
            .expect("query");
    assert_eq!(defaults, ["partner"]);

    let _ = (first, second);
}

async fn the_fallback_is_what_an_unnamed_upload_gets(pool: &PgPool) {
    let chosen = upload_profiles::for_upload(pool, None)
        .await
        .expect("resolve")
        .expect("a fallback exists");
    assert_eq!(chosen.key, "partner");

    // A named profile wins over the fallback, which is the ordinary case.
    let press = upload_profiles::by_key(pool, "press")
        .await
        .expect("load")
        .expect("press");
    let chosen = upload_profiles::for_upload(pool, Some(press.id))
        .await
        .expect("resolve")
        .expect("named");
    assert_eq!(chosen.key, "press");

    // A named profile that no longer exists resolves to the fallback rather than failing: a session can
    // outlive an administrator's tidy-up, and refusing the upload at that point would strand staged bytes
    // over a configuration change nobody told the uploader about.
    let chosen = upload_profiles::for_upload(pool, Some(Uuid::new_v4()))
        .await
        .expect("resolve")
        .expect("falls back");
    assert_eq!(chosen.key, "partner");
}

async fn applying_defaults_fills_only_absent_keys(pool: &PgPool) {
    let press = upload_profiles::by_key(pool, "press")
        .await
        .expect("load")
        .expect("press");

    // Nothing supplied: the default lands.
    let applied = upload_profiles::apply_defaults(pool, &press, &serde_json::Map::new())
        .await
        .expect("apply");
    assert_eq!(applied["credit"], "Acme Press Office");

    // Something supplied for the same key: the upload's own value wins. A default is a starting point, not an
    // override — a profile that overwrote what the uploader typed would silently discard their work.
    let supplied = json!({ "credit": "Photographer's own" })
        .as_object()
        .expect("object")
        .clone();
    let applied = upload_profiles::apply_defaults(pool, &press, &supplied)
        .await
        .expect("apply");
    assert_eq!(applied["credit"], "Photographer's own");
}

async fn a_default_that_became_invalid_fails_where_it_can_be_seen(pool: &PgPool) {
    // Validated at save *and* at apply. A definition can change between the two — somebody removes the field,
    // or narrows it — and a default that has quietly become invalid must produce a visible failure rather
    // than being dropped from every upload from then on.
    let orphan = upload_profiles::create(
        pool,
        NewProfile {
            key: "will_break".to_owned(),
            label: "Will break".to_owned(),
            metadata_type_id: None,
            defaults: json!({ "credit": "Acme" }),
            require_complete: false,
            ai_tags_enabled: true,
            is_default: false,
        },
    )
    .await
    .expect("valid at save time");

    fields::remove(pool, "credit")
        .await
        .expect("remove the field");

    let profile = upload_profiles::by_key(pool, "will_break")
        .await
        .expect("load")
        .expect("profile");
    let refusal = upload_profiles::apply_defaults(pool, &profile, &serde_json::Map::new())
        .await
        .expect_err("the default no longer validates");
    assert!(
        matches!(refusal, ProfileRefusal::InvalidDefaults(_)),
        "got {refusal:?}"
    );

    // Put the field back so later cases see the schema they expect.
    fields::define(pool, text("credit"))
        .await
        .expect("redefine");
    let _ = orphan;
}

async fn removing_a_profile_leaves_its_assets_alone(pool: &PgPool) {
    let doomed = upload_profiles::create(
        pool,
        NewProfile {
            key: "temporary".to_owned(),
            label: "Temporary".to_owned(),
            metadata_type_id: None,
            defaults: json!({}),
            require_complete: false,
            ai_tags_enabled: true,
            is_default: false,
        },
    )
    .await
    .expect("create");

    let asset = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id, upload_profile_id) \
         VALUES ($1, $2, 'from-a-profile.jpg', 'image/jpeg', 10, $1, $3)",
    )
    .bind(asset)
    .bind(blake3::hash(b"from-a-profile").to_hex().to_string())
    .bind(doomed.id)
    .execute(pool)
    .await
    .expect("asset");

    upload_profiles::remove(pool, doomed.id)
        .await
        .expect("remove");

    // The asset survives with its reference cleared. Removing a profile is an administrative decision about
    // future intakes; it must not be blocked by, or destroy, what already arrived under it.
    let (exists, profile): (bool, Option<Uuid>) =
        sqlx::query_as("SELECT true, upload_profile_id FROM assets WHERE id = $1")
            .bind(asset)
            .fetch_one(pool)
            .await
            .expect("row");
    assert!(exists);
    assert!(
        profile.is_none(),
        "the dangling reference was cleared, not left"
    );
}
