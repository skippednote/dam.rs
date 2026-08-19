//! Auto-import mappings: embedded metadata into the tenant's own fields (Q.4).
//!
//! The table is small; the *rules* are the substance, and each exists because the obvious alternative loses
//! somebody's work:
//!
//! - **Priority decides between sources.** "Prefer what the editor typed, fall back to what the camera
//!   recorded" is the ordinary requirement, and it cannot be said without an order.
//! - **`overwrite` defaults to false.** Re-running an import over a curated library would otherwise replace
//!   corrections with whatever the file says — invisibly, and for every asset at once.
//! - **A mapping is applied through the validator.** An imported value is metadata like any other, so a caption
//!   that does not fit an `int` field is refused rather than stored.
//!
//! One container; cases are functions over a borrowed pool. See the note in `provenance.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::auto_import::{self, MappingRefusal, NewMapping};
use dam_db::fields::{self, NewField};
use dam_db::{migrate, testing::PostgresHarness};
use serde_json::json;
use sqlx::PgPool;
use std::collections::BTreeMap;

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

fn embedded(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[tokio::test]
async fn the_auto_import_contract_holds() {
    let (_pg, pool) = db().await;

    fields::define(&pool, text("photographer"))
        .await
        .expect("photographer");
    fields::define(&pool, text("caption"))
        .await
        .expect("caption");
    fields::define(
        &pool,
        NewField {
            kind: "int".to_owned(),
            ..text("shot_iso")
        },
    )
    .await
    .expect("shot_iso");
    fields::define(
        &pool,
        NewField {
            read_only: true,
            ..text("ingested_by")
        },
    )
    .await
    .expect("read-only field");
    fields::define(
        &pool,
        NewField {
            kind: "date".to_owned(),
            ..text("shot_on")
        },
    )
    .await
    .expect("shot_on");
    fields::define(
        &pool,
        NewField {
            kind: "datetime".to_owned(),
            ..text("shot_at")
        },
    )
    .await
    .expect("shot_at");

    a_mapping_is_created_and_listed(&pool).await;
    a_mapping_into_an_unknown_field_is_refused(&pool).await;
    a_malformed_source_is_refused(&pool).await;
    a_read_only_field_cannot_be_a_target(&pool).await;
    the_same_source_and_field_twice_is_refused(&pool).await;
    priority_decides_between_two_sources(&pool).await;
    a_disabled_mapping_does_not_fire(&pool).await;
    an_existing_value_survives_unless_overwrite_is_set(&pool).await;
    a_value_that_does_not_fit_its_field_is_reported_not_stored(&pool).await;
    a_timestamp_fills_a_date_field_and_an_unzoned_one_is_refused(&pool).await;
    nothing_matching_produces_no_change_at_all(&pool).await;
    removing_the_field_removes_the_mapping(&pool).await;
}

async fn a_mapping_is_created_and_listed(pool: &PgPool) {
    let created = auto_import::create(
        pool,
        NewMapping {
            source: "exif.artist".to_owned(),
            field_key: "photographer".to_owned(),
            priority: 10,
            overwrite: false,
            enabled: true,
        },
    )
    .await
    .expect("create");
    assert_eq!(created.source, "exif.artist");
    assert_eq!(created.field_key, "photographer");

    let listed = auto_import::list(pool).await.expect("list");
    assert!(listed.iter().any(|m| m.id == created.id));
}

async fn a_mapping_into_an_unknown_field_is_refused(pool: &PgPool) {
    // Named rather than left to the foreign key: this reaches an administrator building an import rule, and
    // "there is no field called photograpber" is the only version they can act on.
    let refusal = auto_import::create(
        pool,
        NewMapping {
            source: "exif.artist".to_owned(),
            field_key: "photograpber".to_owned(),
            priority: 0,
            overwrite: false,
            enabled: true,
        },
    )
    .await
    .expect_err("unknown field");
    assert!(
        matches!(&refusal, MappingRefusal::UnknownField(key) if key == "photograpber"),
        "got {refusal:?}"
    );
}

async fn a_malformed_source_is_refused(pool: &PgPool) {
    // A source is `namespace.name`. A mapping whose left-hand side cannot ever be produced is a rule that
    // silently never fires — the worst kind of configuration, because it looks correct on the screen.
    for bad in [
        "artist",
        "EXIF.Artist",
        "exif..artist",
        "exif.",
        // Both halves are checked, not just one: a namespace is as required as a name.
        ".artist",
        "",
        "exif.artist; drop",
    ] {
        let refusal = auto_import::create(
            pool,
            NewMapping {
                source: bad.to_owned(),
                field_key: "caption".to_owned(),
                priority: 0,
                overwrite: false,
                enabled: true,
            },
        )
        .await
        .expect_err(bad);
        assert!(
            matches!(refusal, MappingRefusal::MalformedSource(_)),
            "{bad:?} should be refused: {refusal:?}"
        );
    }
}

async fn a_read_only_field_cannot_be_a_target(pool: &PgPool) {
    // A read-only field is maintained by the system and describes the file rather than the tenant's intent. An
    // import writing one would make the metadata disagree with the bytes — the exact reason the validator
    // refuses a human doing it, so the rule cannot be allowed to exist either.
    let refusal = auto_import::create(
        pool,
        NewMapping {
            source: "exif.software".to_owned(),
            field_key: "ingested_by".to_owned(),
            priority: 0,
            overwrite: false,
            enabled: true,
        },
    )
    .await
    .expect_err("read-only");
    assert!(
        matches!(refusal, MappingRefusal::ReadOnlyTarget(_)),
        "got {refusal:?}"
    );
}

async fn the_same_source_and_field_twice_is_refused(pool: &PgPool) {
    let refusal = auto_import::create(
        pool,
        NewMapping {
            source: "exif.artist".to_owned(),
            field_key: "photographer".to_owned(),
            priority: 99,
            overwrite: true,
            enabled: true,
        },
    )
    .await
    .expect_err("duplicate");
    assert!(
        matches!(refusal, MappingRefusal::Duplicate { .. }),
        "got {refusal:?}"
    );
}

async fn priority_decides_between_two_sources(pool: &PgPool) {
    // The requirement this table exists for: prefer what an editor typed, fall back to what the camera
    // recorded. Lower priority first, so `xmp.creator` at 0 beats `exif.artist` at 10.
    auto_import::create(
        pool,
        NewMapping {
            source: "xmp.creator".to_owned(),
            field_key: "photographer".to_owned(),
            priority: 0,
            overwrite: false,
            enabled: true,
        },
    )
    .await
    .expect("create");

    let both = embedded(&[
        ("xmp.creator", "Ada Lovelace"),
        ("exif.artist", "Camera Default"),
    ]);
    let plan = auto_import::plan(pool, &both, &serde_json::Map::new())
        .await
        .expect("plan");
    assert_eq!(
        plan.values.get("photographer").and_then(|v| v.as_str()),
        Some("Ada Lovelace"),
        "the higher-priority source wins: {plan:?}"
    );

    // And with only the lower-priority source present, it is used — a preference is not a requirement.
    let camera_only = embedded(&[("exif.artist", "Camera Default")]);
    let plan = auto_import::plan(pool, &camera_only, &serde_json::Map::new())
        .await
        .expect("plan");
    assert_eq!(
        plan.values.get("photographer").and_then(|v| v.as_str()),
        Some("Camera Default")
    );
}

async fn a_disabled_mapping_does_not_fire(pool: &PgPool) {
    let mapping = auto_import::create(
        pool,
        NewMapping {
            source: "xmp.description".to_owned(),
            field_key: "caption".to_owned(),
            priority: 0,
            overwrite: false,
            enabled: false,
        },
    )
    .await
    .expect("create");

    let carrying = embedded(&[("xmp.description", "A harbour at dawn")]);
    let plan = auto_import::plan(pool, &carrying, &serde_json::Map::new())
        .await
        .expect("plan");
    assert!(
        !plan.values.contains_key("caption"),
        "a disabled rule must not fire: {plan:?}"
    );

    // Enabling it makes it fire, which is what makes the switch worth having rather than a delete.
    auto_import::set_enabled(pool, mapping.id, true)
        .await
        .expect("enable");
    let plan = auto_import::plan(pool, &carrying, &serde_json::Map::new())
        .await
        .expect("plan");
    assert_eq!(
        plan.values.get("caption").and_then(|v| v.as_str()),
        Some("A harbour at dawn")
    );
}

async fn an_existing_value_survives_unless_overwrite_is_set(pool: &PgPool) {
    let carrying = embedded(&[("xmp.description", "From the file")]);
    let existing = json!({ "caption": "Written by a person" })
        .as_object()
        .expect("object")
        .clone();

    // The default, and the safe direction: re-running an import over a curated library must not replace
    // somebody's corrections with whatever the camera said.
    let plan = auto_import::plan(pool, &carrying, &existing)
        .await
        .expect("plan");
    assert!(
        !plan.values.contains_key("caption"),
        "an existing value is left alone: {plan:?}"
    );
    assert_eq!(
        plan.skipped,
        vec!["caption".to_owned()],
        "and the skip is reported"
    );

    // Turning it on is a deliberate "the file is the source of truth for this field".
    let mapping = auto_import::list(pool)
        .await
        .expect("list")
        .into_iter()
        .find(|m| m.field_key == "caption")
        .expect("the caption mapping");
    auto_import::set_overwrite(pool, mapping.id, true)
        .await
        .expect("overwrite");

    let plan = auto_import::plan(pool, &carrying, &existing)
        .await
        .expect("plan");
    assert_eq!(
        plan.values.get("caption").and_then(|v| v.as_str()),
        Some("From the file")
    );
    assert!(plan.skipped.is_empty());

    // A field explicitly cleared to null is *not* a held value, so an import may fill it. Checking presence
    // alone would make "somebody cleared this once" a permanent refusal, which no screen would ever explain.
    auto_import::set_overwrite(pool, mapping.id, false)
        .await
        .expect("back to the default");
    let cleared = json!({ "caption": serde_json::Value::Null })
        .as_object()
        .expect("object")
        .clone();
    let plan = auto_import::plan(pool, &carrying, &cleared)
        .await
        .expect("plan");
    assert_eq!(
        plan.values.get("caption").and_then(|v| v.as_str()),
        Some("From the file"),
        "a cleared field is fillable without overwrite: {plan:?}"
    );
    assert!(
        plan.skipped.is_empty(),
        "and is not reported as skipped: {plan:?}"
    );
}

async fn a_value_that_does_not_fit_its_field_is_reported_not_stored(pool: &PgPool) {
    // `shot_iso` is an int and EXIF renders sensitivity as text. An imported value is metadata like any other,
    // so it goes through the validator — and a rejection is reported rather than silently dropped, because a
    // mapping that never produces anything should be visible to whoever configured it.
    auto_import::create(
        pool,
        NewMapping {
            source: "exif.iso".to_owned(),
            field_key: "shot_iso".to_owned(),
            priority: 0,
            overwrite: false,
            enabled: true,
        },
    )
    .await
    .expect("create");

    let odd = embedded(&[("exif.iso", "ISO 400 (approx)")]);
    let plan = auto_import::plan(pool, &odd, &serde_json::Map::new())
        .await
        .expect("plan");
    assert!(
        !plan.values.contains_key("shot_iso"),
        "an invalid value is not stored: {plan:?}"
    );
    assert!(
        plan.rejected.iter().any(|r| r.key == "shot_iso"),
        "and it is reported: {plan:?}"
    );

    // A value that *does* fit is imported, so the field is not simply unusable.
    let plain = embedded(&[("exif.iso", "400")]);
    let plan = auto_import::plan(pool, &plain, &serde_json::Map::new())
        .await
        .expect("plan");
    assert_eq!(
        plan.values.get("shot_iso").and_then(|v| v.as_i64()),
        Some(400)
    );

    // Per-field, not all-or-nothing: a valid sibling lands even though the ISO alongside it is refused. One odd
    // camera writing one odd tag must not make the whole import do nothing.
    auto_import::create(
        pool,
        NewMapping {
            source: "xmp.headline".to_owned(),
            field_key: "photographer".to_owned(),
            priority: 0,
            overwrite: false,
            enabled: true,
        },
    )
    .await
    .expect("create");
    let mixed = embedded(&[
        ("exif.iso", "ISO 400 (approx)"),
        ("xmp.headline", "Ada Lovelace"),
    ]);
    let plan = auto_import::plan(pool, &mixed, &serde_json::Map::new())
        .await
        .expect("plan");
    assert_eq!(
        plan.values.get("photographer").and_then(|v| v.as_str()),
        Some("Ada Lovelace"),
        "the valid sibling survives the rejection: {plan:?}"
    );
    assert!(plan.rejected.iter().any(|r| r.key == "shot_iso"));
}

async fn nothing_matching_produces_no_change_at_all(pool: &PgPool) {
    // The overwhelmingly common case: a file with no embedded metadata, or none any mapping points at. The plan
    // has to be empty rather than a set of nulls, or every upload would clear the fields it did not fill.
    let plan = auto_import::plan(pool, &BTreeMap::new(), &serde_json::Map::new())
        .await
        .expect("plan");
    assert!(plan.values.is_empty(), "{plan:?}");
    assert!(plan.rejected.is_empty());
    assert!(plan.skipped.is_empty());

    let unmapped = embedded(&[("exif.lens", "XF 35mm")]);
    let plan = auto_import::plan(pool, &unmapped, &serde_json::Map::new())
        .await
        .expect("plan");
    assert!(
        plan.values.is_empty(),
        "an unmapped source contributes nothing: {plan:?}"
    );
}

async fn removing_the_field_removes_the_mapping(pool: &PgPool) {
    // A mapping into a field that no longer exists is a rule that can never fire. Keeping it would make the
    // import screen list phantoms, and `ON DELETE CASCADE` is what stops that.
    let before = auto_import::list(pool).await.expect("list").len();
    fields::remove(pool, "caption")
        .await
        .expect("remove the field");
    let after = auto_import::list(pool).await.expect("list");
    assert!(
        after.len() < before,
        "the mapping went with the field: {before} -> {}",
        after.len()
    );
    assert!(after.iter().all(|m| m.field_key != "caption"));
}

async fn a_timestamp_fills_a_date_field_and_an_unzoned_one_is_refused(pool: &PgPool) {
    for field in ["shot_on", "shot_at"] {
        auto_import::create(
            pool,
            NewMapping {
                source: "exif.taken_at".to_owned(),
                field_key: field.to_owned(),
                priority: 0,
                overwrite: false,
                enabled: true,
            },
        )
        .await
        .expect("create");
    }

    // What a camera without EXIF 2.31 writes: a wall-clock reading with no zone.
    let local = embedded(&[("exif.taken_at", "2026-03-14T09:26:53")]);
    let plan = auto_import::plan(pool, &local, &serde_json::Map::new())
        .await
        .expect("plan");
    assert_eq!(
        plan.values.get("shot_on").and_then(|v| v.as_str()),
        Some("2026-03-14"),
        "a date field takes the day: {plan:?}"
    );
    // And a datetime field refuses, because without an offset there is no instant. Appending `Z` would move the
    // photograph by up to a day and store the result as fact.
    assert!(
        plan.rejected.iter().any(|r| r.key == "shot_at"),
        "an unzoned timestamp is not an instant: {plan:?}"
    );

    // With the offset the camera recorded, both fields are satisfiable.
    let zoned = embedded(&[("exif.taken_at", "2026-03-14T09:26:53+05:30")]);
    let plan = auto_import::plan(pool, &zoned, &serde_json::Map::new())
        .await
        .expect("plan");
    assert_eq!(
        plan.values.get("shot_at").and_then(|v| v.as_str()),
        Some("2026-03-14T09:26:53+05:30")
    );
    assert_eq!(
        plan.values.get("shot_on").and_then(|v| v.as_str()),
        Some("2026-03-14"),
        "the day survives the offset being present: {plan:?}"
    );
    assert!(plan.rejected.is_empty(), "{plan:?}");
}
