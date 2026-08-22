//! Metadata types: which fields apply to which kind of asset (Q.1).
//!
//! The feature is a *selection* over the tenant's field vocabulary, not a second vocabulary — `field_defs`
//! stays the one place a key has a kind and a validation rule, because that invariant is what the schema-admin
//! refusals protect. So the interesting cases here are the resolution rules: what an asset's field list is
//! when it has a type, when it has none, when its tenant has no types at all, and when the type it named was
//! removed underneath it. Each of those has to leave already-stored metadata visible, because a field list is
//! what the detail panel enumerates — a resolution bug does not lose data, it *hides* it, which is worse
//! because nothing alarms.
//!
//! One container; cases are functions over a borrowed pool. See the note in `provenance.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::fields::{self, NewField};
use dam_db::metadata_types::{self, NewType};
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

async fn asset(pool: &PgPool, label: &str, mime: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, $4, 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(label.as_bytes()).to_hex().to_string())
    .bind(format!("{label}.bin"))
    .bind(mime)
    .execute(pool)
    .await
    .expect("asset");
    id
}

#[tokio::test]
async fn the_metadata_type_contract_holds() {
    let (_pg, pool) = db().await;

    // The tenant's vocabulary, defined once and shared by every type below — which is the whole point.
    for key in [
        "description",
        "alt_text",
        "print_dpi",
        "duration_note",
        "archive_manifest",
    ] {
        fields::define(&pool, text(key)).await.expect("define");
    }

    with_no_types_every_field_applies(&pool).await;
    a_type_selects_its_own_fields_in_its_own_order(&pool).await;
    a_field_is_shared_not_copied(&pool).await;
    ingest_picks_a_type_by_media_class(&pool).await;
    an_unmatched_class_falls_back_to_the_default(&pool).await;
    an_asset_with_no_type_falls_back_too(&pool).await;
    removing_a_type_leaves_its_assets_readable(&pool).await;
    removing_a_field_removes_it_from_every_type(&pool).await;
    only_one_type_can_be_the_default(&pool).await;
    a_type_cannot_include_a_field_that_does_not_exist(&pool).await;
    a_write_is_scoped_to_the_asset_s_own_form(&pool).await;
}

async fn with_no_types_every_field_applies(pool: &PgPool) {
    // The migration state, and the one that must not change behaviour: a tenant that has not opted in sees
    // every field on every asset, exactly as before types existed. Narrowing silently here would hide
    // stored metadata on every asset in the library at once.
    let id = asset(pool, "pre-types", "image/jpeg").await;
    let applicable = metadata_types::fields_for(pool, id).await.expect("fields");
    let keys: Vec<&str> = applicable.iter().map(|def| def.key.as_str()).collect();
    assert_eq!(
        keys.len(),
        5,
        "with no types defined, the whole vocabulary applies: {keys:?}"
    );
}

async fn a_type_selects_its_own_fields_in_its_own_order(pool: &PgPool) {
    let image = metadata_types::define(
        pool,
        NewType {
            key: "image".to_owned(),
            label: "Image".to_owned(),
            applies_to: vec!["image".to_owned()],
            is_default: true,
            // Order is per type, not per tenant: `print_dpi` first for photographers, and the same field
            // sits elsewhere in another type without either type winning.
            field_keys: vec![
                "print_dpi".to_owned(),
                "description".to_owned(),
                "alt_text".to_owned(),
            ],
        },
    )
    .await
    .expect("define image type");
    assert_eq!(image.field_keys.len(), 3);

    let id = asset(pool, "a-photo", "image/jpeg").await;
    metadata_types::assign(pool, id, Some(image.id))
        .await
        .expect("assign");

    let keys: Vec<String> = metadata_types::fields_for(pool, id)
        .await
        .expect("fields")
        .into_iter()
        .map(|def| def.key)
        .collect();
    assert_eq!(keys, ["print_dpi", "description", "alt_text"]);
}

async fn a_field_is_shared_not_copied(pool: &PgPool) {
    // `description` belongs to two types. It is one definition with one kind and one validation rule, which
    // is what keeps a value written under the video type readable under the image type. A per-type copy of
    // the definition is exactly the divergence `field_defs`' unique key exists to prevent.
    let video = metadata_types::define(
        pool,
        NewType {
            key: "video".to_owned(),
            label: "Video".to_owned(),
            applies_to: vec!["video".to_owned()],
            is_default: false,
            field_keys: vec!["description".to_owned(), "duration_note".to_owned()],
        },
    )
    .await
    .expect("define video type");

    let id = asset(pool, "a-video", "video/mp4").await;
    metadata_types::assign(pool, id, Some(video.id))
        .await
        .expect("assign");
    let keys: Vec<String> = metadata_types::fields_for(pool, id)
        .await
        .expect("fields")
        .into_iter()
        .map(|def| def.key)
        .collect();
    assert_eq!(keys, ["description", "duration_note"]);

    // Same key, same definition object, reached through a different type.
    let all = fields::load(pool).await.expect("load");
    assert_eq!(
        all.iter().filter(|def| def.key == "description").count(),
        1,
        "one definition, however many types include it"
    );
}

async fn ingest_picks_a_type_by_media_class(pool: &PgPool) {
    // A document type exists before these assertions, and that is load-bearing rather than scenery: with only
    // an image type and an image default, *every* mime resolves to the image type and the assertions below
    // pass whatever `media_class` returns. Mutation testing caught it — classing `image/svg+xml` as a
    // document still "passed". A second type gives a wrong class somewhere else to land.
    metadata_types::define(
        pool,
        NewType {
            key: "document".to_owned(),
            label: "Document".to_owned(),
            applies_to: vec!["document".to_owned()],
            is_default: false,
            field_keys: vec!["description".to_owned()],
        },
    )
    .await
    .expect("define document type");

    // Ingest should not have to be told: a video/mp4 lands on the type that claims the video class. Being
    // told is still possible (see `assign`), but the default path is automatic or nobody sets it at all.
    for (mime, expected) in [
        ("image/png", "image"),
        ("video/quicktime", "video"),
        ("image/svg+xml", "image"),
        ("application/pdf", "document"),
    ] {
        let chosen = metadata_types::for_mime(pool, mime)
            .await
            .expect("choose")
            .expect("a type matches");
        assert_eq!(chosen.key, expected, "{mime} should land on {expected}");
    }
}

async fn an_unmatched_class_falls_back_to_the_default(pool: &PgPool) {
    // An audio file, with no audio type defined. It gets the default rather than nothing: a new media class
    // arriving in the library must not produce assets with no metadata form at all.
    let chosen = metadata_types::for_mime(pool, "audio/mpeg")
        .await
        .expect("choose")
        .expect("the default catches it");
    assert_eq!(chosen.key, "image", "image is the tenant's default here");

    // A zip classes as `archive` and there is no archive type yet, so it takes the same route. Worth pinning
    // separately from the audio case: audio has no type because nobody made one, while archive is a class the
    // vocabulary knows about — and both must still end somewhere.
    let chosen = metadata_types::for_mime(pool, "application/zip")
        .await
        .expect("choose")
        .expect("the default catches it");
    assert_eq!(chosen.key, "image");
}

async fn an_asset_with_no_type_falls_back_too(pool: &PgPool) {
    let id = asset(pool, "typeless", "application/zip").await;
    let keys: Vec<String> = metadata_types::fields_for(pool, id)
        .await
        .expect("fields")
        .into_iter()
        .map(|def| def.key)
        .collect();
    // The default type's list, not the empty list and not the whole vocabulary.
    assert_eq!(keys, ["print_dpi", "description", "alt_text"]);
}

async fn removing_a_type_leaves_its_assets_readable(pool: &PgPool) {
    let doomed = metadata_types::define(
        pool,
        NewType {
            key: "archive".to_owned(),
            label: "Archive".to_owned(),
            applies_to: vec!["archive".to_owned()],
            is_default: false,
            field_keys: vec!["archive_manifest".to_owned()],
        },
    )
    .await
    .expect("define archive type");
    let id = asset(pool, "a-zip", "application/zip").await;
    metadata_types::assign(pool, id, Some(doomed.id))
        .await
        .expect("assign");
    assert_eq!(
        metadata_types::fields_for(pool, id)
            .await
            .expect("fields")
            .len(),
        1
    );

    metadata_types::remove(pool, doomed.id)
        .await
        .expect("remove");

    // The asset survives with its type cleared, and falls back — rather than the removal being blocked, or
    // the asset being left pointing at a row that no longer exists.
    let keys: Vec<String> = metadata_types::fields_for(pool, id)
        .await
        .expect("fields")
        .into_iter()
        .map(|def| def.key)
        .collect();
    assert_eq!(
        keys,
        ["print_dpi", "description", "alt_text"],
        "fell back to the default"
    );
    let still_there: Option<Uuid> =
        sqlx::query_scalar("SELECT metadata_type_id FROM assets WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("row");
    assert!(
        still_there.is_none(),
        "the dangling reference was cleared, not left"
    );
}

async fn removing_a_field_removes_it_from_every_type(pool: &PgPool) {
    // A definition's removal keeps its *values* (that is `fields::remove`'s contract) and drops its
    // membership. Leaving the membership would make the type list a set of keys some of which no longer
    // resolve, which is a form the UI cannot render.
    fields::remove(pool, "alt_text")
        .await
        .expect("remove field");

    let image = metadata_types::by_key(pool, "image")
        .await
        .expect("load")
        .expect("image type");
    assert_eq!(
        image.field_keys,
        ["print_dpi", "description"],
        "the removed field is gone from the type too"
    );
}

async fn only_one_type_can_be_the_default(pool: &PgPool) {
    // Two rows claiming the fallback would make an asset's field list depend on row order. Setting a new
    // default moves it rather than refusing, because "make this the default" is the intent either way.
    let video = metadata_types::by_key(pool, "video")
        .await
        .expect("load")
        .expect("video type");
    metadata_types::set_default(pool, video.id)
        .await
        .expect("set default");

    let defaults: Vec<String> =
        sqlx::query_scalar("SELECT key FROM metadata_types WHERE is_default ORDER BY key")
            .fetch_all(pool)
            .await
            .expect("query");
    assert_eq!(defaults, ["video"], "exactly one, and it moved");

    // And the fallback followed it.
    let chosen = metadata_types::for_mime(pool, "audio/mpeg")
        .await
        .expect("choose")
        .expect("default");
    assert_eq!(chosen.key, "video");

    // Put it back so later assertions read against the original arrangement.
    let image = metadata_types::by_key(pool, "image")
        .await
        .expect("load")
        .expect("image type");
    metadata_types::set_default(pool, image.id)
        .await
        .expect("restore default");
}

async fn a_type_cannot_include_a_field_that_does_not_exist(pool: &PgPool) {
    // Named rather than a foreign-key violation: this reaches an administrator building a form, and
    // "nonesuch is not a field" is the only version they can act on.
    let refusal = metadata_types::define(
        pool,
        NewType {
            key: "bogus".to_owned(),
            label: "Bogus".to_owned(),
            applies_to: vec![],
            is_default: false,
            field_keys: vec!["nonesuch".to_owned()],
        },
    )
    .await
    .expect_err("unknown field");
    assert!(
        matches!(&refusal, metadata_types::TypeRefusal::UnknownField(key) if key == "nonesuch"),
        "expected a named unknown field, got {refusal:?}"
    );

    // And a duplicate key is its own refusal, for the same reason.
    let refusal = metadata_types::define(
        pool,
        NewType {
            key: "image".to_owned(),
            label: "Image again".to_owned(),
            applies_to: vec![],
            is_default: false,
            field_keys: vec![],
        },
    )
    .await
    .expect_err("duplicate");
    assert!(matches!(
        refusal,
        metadata_types::TypeRefusal::DuplicateKey(_)
    ));
}

async fn a_write_is_scoped_to_the_asset_s_own_form(pool: &PgPool) {
    // The point of the whole feature, and the only place it is observable: a key the asset's form does not
    // show must not be writable. Otherwise a type is decoration — the value lands in the JSONB and no form
    // ever displays it again, which is the same silent-discard failure `unknown_field` exists to prevent.
    let video = metadata_types::by_key(pool, "video")
        .await
        .expect("load")
        .expect("video type");
    let id = asset(pool, "scoped-video", "video/mp4").await;
    metadata_types::assign(pool, id, Some(video.id))
        .await
        .expect("assign");

    let mut conn = pool.acquire().await.expect("conn");

    // `duration_note` is on the video form: accepted.
    let payload = serde_json::json!({ "duration_note": "two minutes" })
        .as_object()
        .expect("object")
        .clone();
    fields::validate_for_on(
        &mut conn,
        Some(id),
        &payload,
        dam_core::fields::Mode::Patch,
        dam_core::fields::Writer::Human,
    )
    .await
    .expect("a field on this asset's form is writable");

    // `print_dpi` is a real field in the tenant's vocabulary, and it is *not* on the video form: refused.
    // That distinction is the test — an implementation that validated against the whole vocabulary would
    // accept this and look correct.
    let payload = serde_json::json!({ "print_dpi": "300" })
        .as_object()
        .expect("object")
        .clone();
    let outcome = fields::validate_for_on(
        &mut conn,
        Some(id),
        &payload,
        dam_core::fields::Mode::Patch,
        dam_core::fields::Writer::Human,
    )
    .await
    .expect_err("a field outside this asset's form is not writable");
    let fields::ValidationOutcome::Rejected(rejections) = outcome else {
        panic!("expected a rejection, got {outcome:?}");
    };
    assert_eq!(rejections.len(), 1);
    assert_eq!(rejections[0].key, "print_dpi");
    // The same code a typo gets, deliberately: a distinct "not in this type" would disclose the rest of the
    // tenant's schema to a caller holding one asset, and the fix is identical either way.
    assert_eq!(rejections[0].code, "unknown_field");

    // Unscoped, the same payload is fine — which is what proves the scoping did the work rather than the
    // field being broken.
    fields::validate_for_on(
        &mut conn,
        None,
        &payload,
        dam_core::fields::Mode::Patch,
        dam_core::fields::Writer::Human,
    )
    .await
    .expect("the whole vocabulary still accepts it");
}
