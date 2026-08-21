//! Field validation against a real database (2.1).
//!
//! One check needs a row and cannot be decided from the payload: whether a taxonomy term belongs to the
//! taxonomy its field is bound to. TASKS.md names it as this task's test, and it matters for a reason
//! beyond tidiness — a term from the wrong vocabulary would index and facet under that vocabulary, so
//! "everything in Outdoor" would quietly return assets nobody put there.
//!
//! One container for the suite; the cases are functions over a borrowed pool. See the note in
//! `provenance.rs` for why.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::fields::{Mode, Writer};
use dam_db::fields::{self, ValidationOutcome};
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

async fn taxonomy(pool: &PgPool, key: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO taxonomies (id, key, label) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(key)
        .execute(pool)
        .await
        .expect("taxonomy");
    id
}

async fn term(pool: &PgPool, taxonomy_id: Uuid, slug: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO taxonomy_terms (id, taxonomy_id, path, slug, label) \
         VALUES ($1, $2, text2ltree($3), $3, $3)",
    )
    .bind(id)
    .bind(taxonomy_id)
    .bind(slug)
    .execute(pool)
    .await
    .expect("term");
    id
}

async fn field(pool: &PgPool, key: &str, kind: &str, taxonomy_id: Option<Uuid>, multivalued: bool) {
    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind, taxonomy_id, multivalued) \
         VALUES (gen_random_uuid(), $1, $1, $2, $3, $4)",
    )
    .bind(key)
    .bind(kind)
    .bind(taxonomy_id)
    .bind(multivalued)
    .execute(pool)
    .await
    .expect("field def");
}

async fn validate(pool: &PgPool, payload: serde_json::Value) -> Result<(), Vec<(String, String)>> {
    let object = payload.as_object().expect("an object").clone();
    match fields::validate(pool, &object, Mode::Patch, Writer::Human).await {
        Ok(_) => Ok(()),
        Err(ValidationOutcome::Rejected(rejections)) => Err(rejections
            .into_iter()
            .map(|r| (r.key, r.code.to_owned()))
            .collect()),
        Err(ValidationOutcome::Failed(error)) => panic!("validation failed: {error}"),
    }
}

/// The wiring `validate_for_on` exists for (Q.19b).
///
/// The rule itself is `dam_core`'s and tested there. What this covers is that the asset-scoped entry point
/// *loads* what the asset already carries: an edit that fills in a child field without restating its parent is
/// the ordinary shape of an edit, and a validator judging the payload alone would refuse it.
async fn a_dependent_field_is_judged_against_what_the_asset_already_carries(pool: &PgPool) {
    field(pool, "has_people", "bool", None, false).await;
    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind, validation) \
         VALUES (gen_random_uuid(), 'release_reference', 'Release reference', 'text', \
                 '{\"depends_on\": {\"key\": \"has_people\", \"values\": [\"true\"]}}'::jsonb)",
    )
    .execute(pool)
    .await
    .expect("dependent field");

    // Two assets: one whose stored metadata satisfies the condition, one whose does not.
    let with_people = Uuid::new_v4();
    let without = Uuid::new_v4();
    for (id, has_people) in [(with_people, true), (without, false)] {
        sqlx::query(
            "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
             VALUES ($1, $2, 'shoot.jpg', 'image/jpeg', 10, $1)",
        )
        .bind(id)
        .bind(format!("{id}"))
        .execute(pool)
        .await
        .expect("asset");
        sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2)")
            .bind(id)
            .bind(serde_json::json!({"has_people": has_people}))
            .execute(pool)
            .await
            .expect("metadata");
    }

    let patch = serde_json::json!({"release_reference": "MR-9"});
    let object = patch.as_object().expect("object").clone();

    let mut conn = pool.acquire().await.expect("conn");
    let accepted = fields::validate_for_on(
        &mut conn,
        Some(with_people),
        &object,
        Mode::Patch,
        Writer::Human,
    )
    .await;
    assert!(
        accepted.is_ok(),
        "the stored parent satisfies the condition: {:?}",
        accepted.err().map(|e| e.to_string())
    );

    let refused = fields::validate_for_on(
        &mut conn,
        Some(without),
        &object,
        Mode::Patch,
        Writer::Human,
    )
    .await;
    match refused {
        Err(ValidationOutcome::Rejected(rejections)) => {
            assert_eq!(rejections[0].key, "release_reference");
            assert_eq!(rejections[0].code, "not_applicable");
        }
        other => panic!("an inapplicable field must be refused: {other:?}"),
    }
}

// ─── the case the task names ────────────────────────────────────────────────

async fn a_taxonomy_ref_refuses_a_term_from_the_wrong_taxonomy(pool: &PgPool) {
    let categories = taxonomy(pool, "categories").await;
    let colours = taxonomy(pool, "colours").await;
    field(pool, "category", "taxonomy_ref", Some(categories), false).await;

    let outdoor = term(pool, categories, "outdoor").await;
    let red = term(pool, colours, "red").await;

    validate(pool, json!({"category": outdoor.to_string()}))
        .await
        .expect("a term from the field's own taxonomy");

    let rejected = validate(pool, json!({"category": red.to_string()}))
        .await
        .expect_err("a term from another taxonomy must be refused");
    assert_eq!(
        rejected,
        vec![("category".to_owned(), "wrong_taxonomy".to_owned())],
        "a valid-looking UUID from the wrong vocabulary is the case that would otherwise pass"
    );
}

async fn a_term_that_does_not_exist_is_distinguished_from_the_wrong_taxonomy(pool: &PgPool) {
    // Different problems with different fixes: one is a stale client, the other is a client using the
    // wrong vocabulary. One code for both would send people looking in the wrong place.
    let categories = taxonomy(pool, "cat2").await;
    field(pool, "category2", "taxonomy_ref", Some(categories), false).await;

    let rejected = validate(pool, json!({"category2": Uuid::new_v4().to_string()}))
        .await
        .expect_err("an unknown term must be refused");
    assert_eq!(
        rejected,
        vec![("category2".to_owned(), "term_not_found".to_owned())]
    );
}

async fn every_bad_term_in_a_multivalued_field_is_reported(pool: &PgPool) {
    // A bulk import fixing one term per attempt is a bulk import nobody finishes.
    let categories = taxonomy(pool, "cat3").await;
    let other = taxonomy(pool, "other3").await;
    field(pool, "tags3", "taxonomy_ref", Some(categories), true).await;

    let good = term(pool, categories, "good3").await;
    let wrong = term(pool, other, "wrong3").await;
    let missing = Uuid::new_v4();

    let rejected = validate(
        pool,
        json!({"tags3": [good.to_string(), wrong.to_string(), missing.to_string()]}),
    )
    .await
    .expect_err("two of the three are bad");
    assert_eq!(rejected.len(), 2, "got {rejected:?}");
    let codes: Vec<&str> = rejected.iter().map(|(_, code)| code.as_str()).collect();
    assert!(codes.contains(&"wrong_taxonomy") && codes.contains(&"term_not_found"));
}

// ─── definitions are read from the database, not trusted from the caller ────

async fn definitions_are_loaded_with_their_constraints(pool: &PgPool) {
    // The `validation` jsonb has to survive the round trip, or every constraint a tenant configures is
    // silently unenforced — which looks exactly like having no constraints, and nobody notices.
    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind, validation, required, ai_writable) \
         VALUES (gen_random_uuid(), 'sku', 'SKU', 'text', \
                 '{\"pattern\": \"[A-Z]{3}-[0-9]{4}\", \"max_length\": 8}'::jsonb, true, true)",
    )
    .execute(pool)
    .await
    .expect("field def");

    let defs = fields::load(pool).await.expect("load");
    let sku = defs
        .iter()
        .find(|d| d.key == "sku")
        .expect("the sku definition");
    assert_eq!(sku.constraints.max_length, Some(8));
    assert_eq!(
        sku.constraints.pattern.as_deref(),
        Some("[A-Z]{3}-[0-9]{4}")
    );
    assert!(sku.required && sku.ai_writable);

    validate(pool, json!({"sku": "ABC-1234"}))
        .await
        .expect("matches the pattern");
    let rejected = validate(pool, json!({"sku": "nope"}))
        .await
        .expect_err("must apply the loaded pattern");
    assert_eq!(rejected, vec![("sku".to_owned(), "pattern".to_owned())]);
}

async fn a_field_kind_this_build_does_not_know_is_an_error_not_a_default(pool: &PgPool) {
    // Defaulting to `text` would silently drop validation for a field whose kind arrived in a newer
    // migration — which is precisely when validation matters. The CHECK constraint means this can only
    // happen across a rollback, and a rollback is the worst time to start accepting anything.
    // Two statements, because Postgres refuses multiple commands in one prepared statement.
    sqlx::query("ALTER TABLE field_defs DROP CONSTRAINT field_defs_kind_check")
        .execute(pool)
        .await
        .expect("drop the kind constraint");
    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind) \
         VALUES (gen_random_uuid(), 'from_the_future', 'Future', 'quantum')",
    )
    .execute(pool)
    .await
    .expect("insert an unknown kind");

    let outcome = fields::load(pool).await;
    assert!(
        outcome.is_err(),
        "an unknown kind must refuse rather than degrade to text"
    );
}

#[tokio::test]
async fn the_field_validation_invariants_hold() {
    let (_pg, pool) = db().await;
    a_taxonomy_ref_refuses_a_term_from_the_wrong_taxonomy(&pool).await;
    a_term_that_does_not_exist_is_distinguished_from_the_wrong_taxonomy(&pool).await;
    every_bad_term_in_a_multivalued_field_is_reported(&pool).await;
    definitions_are_loaded_with_their_constraints(&pool).await;
    a_dependent_field_is_judged_against_what_the_asset_already_carries(&pool).await;
    // Last: it drops a CHECK constraint and inserts a row that makes `load` fail for every
    // subsequent case.
    a_field_kind_this_build_does_not_know_is_an_error_not_a_default(&pool).await;
}
