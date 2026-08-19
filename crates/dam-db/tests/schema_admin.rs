//! Editing the tenant's field definitions, against a real database (F.11b·2).
//!
//! The schema is what every other subsystem reads: the validator refuses writes against it, the search
//! renderer decides textual-ness from it, the facet counter enumerates it, and the metadata form is drawn
//! from it. So the interesting cases here are not "does an INSERT insert" — they are the ones where an edit
//! would put stored data and the definition that describes it out of step, which is a state no other layer
//! can detect because validation only happens on write.
//!
//! One container per case group; cases are functions over a borrowed pool. See the note in `provenance.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::fields::{self, Amendment, NewField, SchemaRefusal};
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

/// An asset carrying `values` for its metadata.
async fn asset_with(pool: &PgPool, label: &str, values: serde_json::Value) -> Uuid {
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
    sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2)")
        .bind(id)
        .bind(values)
        .execute(pool)
        .await
        .expect("metadata");
    id
}

/// An asset with no `asset_metadata` row at all.
async fn bare_asset(pool: &PgPool, label: &str) -> Uuid {
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

#[tokio::test]
async fn the_schema_edit_contract_holds() {
    let (_pg, pool) = db().await;

    a_definition_round_trips(&pool).await;
    a_duplicate_key_is_refused_by_name(&pool).await;
    a_duplicate_alias_is_refused_by_name(&pool).await;
    a_malformed_key_never_reaches_the_table(&pool).await;
    a_term_field_needs_its_taxonomy(&pool).await;
    an_amendment_changes_presentation_freely(&pool).await;
    the_key_cannot_be_amended(&pool).await;
    the_kind_is_locked_once_values_exist(&pool).await;
    a_soft_deleted_asset_still_locks_the_kind(&pool).await;
    a_cleared_value_is_not_a_value(&pool).await;
    the_kind_changes_freely_while_unused(&pool).await;
    requiring_a_field_reports_what_it_would_break(&pool).await;
    an_unrelated_edit_reports_no_new_breakage(&pool).await;
    removal_keeps_the_values_and_says_how_many(&pool).await;
    a_removed_definition_can_come_back(&pool).await;
    editing_something_absent_says_so(&pool).await;
    reordering_moves_the_form_not_the_data(&pool).await;
}

async fn a_definition_round_trips(pool: &PgPool) {
    let defined = fields::define(
        pool,
        NewField {
            label: "Brand name".to_owned(),
            facetable: true,
            search_alias: Some("bra".to_owned()),
            validation: json!({ "max_length": 40 }),
            ..text("brand")
        },
    )
    .await
    .expect("define");
    assert_eq!(defined.key, "brand");
    assert_eq!(defined.label, "Brand name");
    assert!(defined.facetable);

    // Visible to the *validator's* view too, not only the catalogue — the two read the same row, and a
    // definition that forms can draw but writes cannot validate against would accept anything.
    let loaded = fields::load(pool).await.expect("load");
    let brand = loaded.iter().find(|def| def.key == "brand").expect("brand");
    assert_eq!(brand.constraints.max_length, Some(40));

    // And the search alias is live, because that is a separate index and a separate query.
    let aliases = fields::aliases(pool).await.expect("aliases");
    assert_eq!(aliases.get("bra").map(String::as_str), Some("brand"));
}

async fn a_duplicate_key_is_refused_by_name(pool: &PgPool) {
    // Not a unique-violation from the driver: the caller is an administrator in a form, and "brand is
    // already defined" is the only version of this they can act on.
    let refusal = fields::define(pool, text("brand")).await.expect_err("dupe");
    assert!(
        matches!(&refusal, SchemaRefusal::DuplicateKey(key) if key == "brand"),
        "expected a named duplicate, got {refusal:?}"
    );
}

async fn a_duplicate_alias_is_refused_by_name(pool: &PgPool) {
    let refusal = fields::define(
        pool,
        NewField {
            search_alias: Some("bra".to_owned()),
            ..text("brandish")
        },
    )
    .await
    .expect_err("dupe alias");
    assert!(
        matches!(&refusal, SchemaRefusal::DuplicateAlias(alias) if alias == "bra"),
        "expected a named alias clash, got {refusal:?}"
    );
    // And nothing landed: a refused definition must not leave the field behind without its alias.
    assert!(
        fields::load(pool)
            .await
            .expect("load")
            .iter()
            .all(|def| def.key != "brandish")
    );
}

async fn a_malformed_key_never_reaches_the_table(pool: &PgPool) {
    // The key is a JSONB member name, a shorthand-search token, and part of a generated SQL path
    // expression. Every one of those has an escaping story; a key that needs escaping in any of them is
    // refused at the door instead, which is the only place the rule can be enforced once.
    for bad in [
        "Brand",      // upper case — the search shorthand is case-folded, so two keys would collide
        "brand name", // a space cannot be typed as a search selector
        "brand-name", // the shorthand's own operator character
        "1brand",     // digits first: not distinguishable from a value in shorthand
        "",           // nothing
        "values",     // the metadata column's own name, which the SQL renderer generates against
        "asset_id",   // an index field name; a document with two would be ambiguous
    ] {
        let refusal = fields::define(pool, text(bad)).await.expect_err(bad);
        assert!(
            matches!(
                refusal,
                SchemaRefusal::BadKey { .. } | SchemaRefusal::ReservedKey(_)
            ),
            "{bad:?} should be refused as a key, got {refusal:?}"
        );
    }
}

async fn a_term_field_needs_its_taxonomy(pool: &PgPool) {
    let refusal = fields::define(
        pool,
        NewField {
            kind: "taxonomy_ref".to_owned(),
            ..text("category")
        },
    )
    .await
    .expect_err("no taxonomy");
    assert!(matches!(refusal, SchemaRefusal::TaxonomyRequired));

    // A taxonomy that does not exist is the same class of mistake and gets its own refusal, because the
    // fix is different: pick a real one rather than supply one.
    let refusal = fields::define(
        pool,
        NewField {
            kind: "taxonomy_ref".to_owned(),
            taxonomy_id: Some(Uuid::new_v4()),
            ..text("category")
        },
    )
    .await
    .expect_err("unknown taxonomy");
    assert!(matches!(refusal, SchemaRefusal::UnknownTaxonomy(_)));

    let taxonomy_id = Uuid::new_v4();
    sqlx::query("INSERT INTO taxonomies (id, key, label) VALUES ($1, 'category', 'Category')")
        .bind(taxonomy_id)
        .execute(pool)
        .await
        .expect("taxonomy");
    let defined = fields::define(
        pool,
        NewField {
            kind: "taxonomy_ref".to_owned(),
            taxonomy_id: Some(taxonomy_id),
            ..text("category")
        },
    )
    .await
    .expect("define term field");
    assert_eq!(defined.taxonomy_id, Some(taxonomy_id));
}

async fn an_amendment_changes_presentation_freely(pool: &PgPool) {
    // Values already exist for `brand`, and none of this touches them.
    asset_with(pool, "branded", json!({ "brand": "acme" })).await;

    let amended = fields::amend(
        pool,
        "brand",
        Amendment {
            label: Some("Brand".to_owned()),
            facetable: Some(false),
            search_alias: Some(Some("brnd".to_owned())),
            validation: Some(json!({ "max_length": 80 })),
            ..Amendment::default()
        },
    )
    .await
    .expect("amend");
    assert_eq!(amended.field.label, "Brand");
    assert!(!amended.field.facetable);
    // Facetable changed, so what the index must hold changed with it — the caller is told, rather than
    // left to discover that facets are stale.
    assert!(amended.reindex_required, "a facet change needs a reindex");

    // The old alias is gone rather than kept alongside: an alias is a name for one field, and two names
    // that resolve differently between builds is the kind of thing nobody debugs twice.
    let aliases = fields::aliases(pool).await.expect("aliases");
    assert_eq!(aliases.get("brnd").map(String::as_str), Some("brand"));
    assert!(!aliases.contains_key("bra"));

    // A label-only edit does not: nothing about the document changed.
    let amended = fields::amend(
        pool,
        "brand",
        Amendment {
            label: Some("Brand name".to_owned()),
            ..Amendment::default()
        },
    )
    .await
    .expect("amend label");
    assert!(!amended.reindex_required, "a label is not indexed");
}

async fn the_key_cannot_be_amended(pool: &PgPool) {
    // There is no `key` on `Amendment` at all — this asserts the *consequence*, which is that the only
    // way to change a key is define-new + backfill + remove-old. A rename in place would leave every
    // stored value under the old member name, invisible and unvalidatable.
    let before = fields::catalog(pool).await.expect("catalog");
    assert!(before.iter().any(|def| def.key == "brand"));
    let refusal = fields::define(pool, text("brand")).await.expect_err("dupe");
    assert!(matches!(refusal, SchemaRefusal::DuplicateKey(_)));
}

async fn the_kind_is_locked_once_values_exist(pool: &PgPool) {
    // `brand` has a value on one asset from the amendment case above. Text → int would make that value
    // invalid, and nothing would ever notice: validation runs on write, so the row would sit there
    // describing itself with a definition it does not satisfy.
    let refusal = fields::amend(
        pool,
        "brand",
        Amendment {
            kind: Some("int".to_owned()),
            ..Amendment::default()
        },
    )
    .await
    .expect_err("kind locked");
    let SchemaRefusal::KindLockedByValues { key, assets } = &refusal else {
        panic!("expected a value-count refusal, got {refusal:?}");
    };
    assert_eq!(key, "brand");
    assert_eq!(*assets, 1, "the count is what makes the refusal actionable");
}

async fn a_soft_deleted_asset_still_locks_the_kind(pool: &PgPool) {
    // A soft delete does not touch `asset_metadata`, so the value is still there waiting for a restore.
    // Changing the kind while it is only *hidden* would mean the restore brings back a value that was
    // never validated against the kind it now claims to have — and nothing re-validates on restore.
    fields::define(pool, text("stylist")).await.expect("define");
    let asset = asset_with(pool, "binned", json!({ "stylist": "rivera" })).await;
    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(asset)
        .execute(pool)
        .await
        .expect("soft delete");

    // The administrator's own view says nobody is using it — that is the live count, and it is honest.
    assert_eq!(fields::usage(pool, "stylist").await.expect("usage"), 0);

    // The lock disagrees, and it is the one that matters.
    let refusal = fields::amend(
        pool,
        "stylist",
        Amendment {
            kind: Some("int".to_owned()),
            ..Amendment::default()
        },
    )
    .await
    .expect_err("locked by a deleted asset");
    assert!(
        matches!(&refusal, SchemaRefusal::KindLockedByValues { assets, .. } if *assets == 1),
        "a recoverable value must still lock the kind, got {refusal:?}"
    );
}

async fn a_cleared_value_is_not_a_value(pool: &PgPool) {
    // Clearing a field stores an explicit JSON `null` rather than dropping the member — that is how the
    // validator distinguishes "emptied on purpose" from "never set". So every count here has to read the
    // value, not just test for the key: an asset whose only mention of a field is a `null` is an asset
    // with nothing stored, and it must neither inflate the usage report nor lock the kind.
    fields::define(pool, text("retoucher"))
        .await
        .expect("define");
    asset_with(pool, "cleared", json!({ "retoucher": null })).await;

    assert_eq!(
        fields::usage(pool, "retoucher").await.expect("usage"),
        0,
        "a cleared field is not in use"
    );
    let amended = fields::amend(
        pool,
        "retoucher",
        Amendment {
            kind: Some("int".to_owned()),
            ..Amendment::default()
        },
    )
    .await
    .expect("a null does not lock the kind");
    assert_eq!(amended.field.kind, "int");
}

async fn the_kind_changes_freely_while_unused(pool: &PgPool) {
    fields::define(pool, text("shoot_notes"))
        .await
        .expect("define");
    let amended = fields::amend(
        pool,
        "shoot_notes",
        Amendment {
            kind: Some("long_text".to_owned()),
            ..Amendment::default()
        },
    )
    .await
    .expect("amend kind");
    assert_eq!(amended.field.kind, "long_text");
    // Textual-ness feeds the index's text blob, so this one does need a reindex.
    assert!(amended.reindex_required);

    // An unknown kind is refused by name rather than stored: `FieldKind::parse` is what every other layer
    // calls, and a row it cannot parse takes out `load` for the whole tenant.
    let refusal = fields::amend(
        pool,
        "shoot_notes",
        Amendment {
            kind: Some("colour".to_owned()),
            ..Amendment::default()
        },
    )
    .await
    .expect_err("unknown kind");
    assert!(matches!(&refusal, SchemaRefusal::UnknownKind(kind) if kind == "colour"));
}

async fn requiring_a_field_reports_what_it_would_break(pool: &PgPool) {
    asset_with(pool, "no-campaign", json!({ "brand": "globex" })).await;
    // And one with no metadata row at all — the normal state of an asset between ingest and its first
    // edit. Counting only assets that *have* a row would report zero breakage on a freshly imported
    // library, which is precisely the library where this number matters most.
    bare_asset(pool, "never-edited").await;
    fields::define(pool, text("campaign"))
        .await
        .expect("define");

    // Turning `required` on is allowed — it is a forward-looking rule, and refusing it would make a
    // schema unfixable on a library that predates the rule. But the assets that would now fail their next
    // metadata write are counted and reported, because "you have just made 40,000 assets unsaveable" is
    // not something to discover one 422 at a time.
    let amended = fields::amend(
        pool,
        "campaign",
        Amendment {
            required: Some(true),
            ..Amendment::default()
        },
    )
    .await
    .expect("amend required");
    assert!(amended.field.required);
    // Compared against the live library rather than a literal: `campaign` was defined a line ago, so
    // *every* live asset lacks it — including the one with no metadata row and excluding the soft-deleted
    // one. A literal here would also have to be rewritten by every case that happens to add an asset,
    // which is how a count assertion quietly becomes a count of whatever the code produced.
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM assets WHERE deleted_at IS NULL")
        .fetch_one(pool)
        .await
        .expect("count");
    assert!(
        live > 2,
        "the fixture needs more than a couple of assets to be worth counting"
    );
    assert_eq!(
        amended.assets_now_incomplete, live,
        "every live asset lacks campaign, whether or not it has a metadata row at all"
    );
    assert!(!amended.reindex_required, "requiredness is not indexed");
}

async fn an_unrelated_edit_reports_no_new_breakage(pool: &PgPool) {
    // `campaign` is already required and still unset on every asset. Editing its label must not re-report
    // the same count: a number that reappears on an unrelated save reads as a consequence of that save,
    // and an administrator renaming a label would think they had just broken two assets.
    let amended = fields::amend(
        pool,
        "campaign",
        Amendment {
            label: Some("Campaign name".to_owned()),
            ..Amendment::default()
        },
    )
    .await
    .expect("amend label");
    assert!(amended.field.required, "still required");
    assert_eq!(
        amended.assets_now_incomplete, 0,
        "the count belongs to the edit that introduced the rule, not to every later edit"
    );
}

async fn removal_keeps_the_values_and_says_how_many(pool: &PgPool) {
    let removed = fields::remove(pool, "brand").await.expect("remove");
    assert_eq!(removed.key, "brand");
    assert_eq!(removed.assets_with_values, 2, "both branded assets counted");
    // A removal takes the field out of forms, facets and search, so the index must be rebuilt without it.
    assert!(removed.reindex_required);

    // The definition is gone from every reader.
    assert!(
        fields::catalog(pool)
            .await
            .expect("catalog")
            .iter()
            .all(|def| def.key != "brand")
    );
    assert!(
        !fields::aliases(pool)
            .await
            .expect("aliases")
            .contains_key("brnd")
    );

    // The *values* are not: the JSONB members stay exactly where they were. This is what makes a
    // mis-clicked delete on a large tenant a recoverable mistake rather than a data-loss event.
    let still: i64 =
        sqlx::query_scalar("SELECT count(*) FROM asset_metadata WHERE values ? 'brand'")
            .fetch_one(pool)
            .await
            .expect("count");
    assert_eq!(
        still, 2,
        "removing a definition must not touch stored values"
    );
}

async fn a_removed_definition_can_come_back(pool: &PgPool) {
    // The reason the values are kept: re-defining the same key with the same kind makes them visible
    // again, unedited. Anything else would make "undo" a restore-from-backup.
    let defined = fields::define(pool, text("brand")).await.expect("redefine");
    assert_eq!(defined.key, "brand");

    let usage = fields::usage(pool, "brand").await.expect("usage");
    assert_eq!(usage, 2, "the old values belong to the new definition");
}

async fn editing_something_absent_says_so(pool: &PgPool) {
    let refusal = fields::amend(pool, "nonesuch", Amendment::default())
        .await
        .expect_err("absent");
    assert!(matches!(&refusal, SchemaRefusal::UnknownField(key) if key == "nonesuch"));

    let refusal = fields::remove(pool, "nonesuch").await.expect_err("absent");
    assert!(matches!(&refusal, SchemaRefusal::UnknownField(key) if key == "nonesuch"));
}

async fn reordering_moves_the_form_not_the_data(pool: &PgPool) {
    let keys: Vec<String> = fields::catalog(pool)
        .await
        .expect("catalog")
        .into_iter()
        .map(|def| def.key)
        .collect();
    let mut reversed = keys.clone();
    reversed.reverse();

    fields::reorder(pool, &reversed).await.expect("reorder");
    let after: Vec<String> = fields::catalog(pool)
        .await
        .expect("catalog")
        .into_iter()
        .map(|def| def.key)
        .collect();
    assert_eq!(after, reversed);

    // A partial list is refused rather than silently leaving the rest wherever they were: display order is
    // a total order, and a client that sent half the fields has a stale schema — reordering against it
    // would move fields it never showed the user.
    let refusal = fields::reorder(pool, &reversed[..1])
        .await
        .expect_err("partial");
    assert!(matches!(refusal, SchemaRefusal::IncompleteOrder { .. }));

    // And an unknown key in the list, same reasoning.
    let refusal = fields::reorder(pool, &["nonesuch".to_owned()])
        .await
        .expect_err("unknown");
    assert!(matches!(
        refusal,
        SchemaRefusal::IncompleteOrder { .. } | SchemaRefusal::UnknownField(_)
    ));
}
