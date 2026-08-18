//! The Tantivy schema, derived from `field_defs` (2.6).
//!
//! ## Why the metadata is one JSON field rather than one field per definition
//!
//! A Tantivy schema is fixed when the index is created. `field_defs` is not: adding a metadata field is
//! an ordinary administrative action a customer takes on a Tuesday afternoon. A schema with one Tantivy
//! field per definition would therefore make **every field addition a full reindex** of that tenant —
//! hours for a million-asset library, and a search outage while it runs.
//!
//! So the tenant's values go into a single JSON field. Adding, renaming or removing a definition changes
//! what is written into it and never the schema, so a field addition costs a reindex of nothing. The price
//! is that per-field boosting is not expressible at the schema level; that belongs in the query, where it
//! is a runtime choice rather than a migration.
//!
//! ## The fixed fields, and why each is here
//!
//! Everything the access filter and the result hydration need, and nothing else — a stored field is bytes
//! on disk per document, and a DAM's index is large.
//!
//! - `asset_id` is stored, because it is the join key back to Postgres. Nothing else is stored: the
//!   authoritative row is in the database, and a second copy in the index is a second thing to keep in
//!   step.
//! - `group_ids` carries asset-group membership so the access predicate can be rendered into the query.
//!   See the warning in [`crate::query`] about what that filter is and is not.
//! - `deleted` lets a soft-deleted asset be excluded without a round trip, since almost every query
//!   excludes them.
//! - `text` is the concatenated searchable text. Concatenated at index time rather than searched as N
//!   fields, because the field set is per tenant and a query over a varying field list cannot be compared
//!   between back ends — which the differential test needs.

use dam_core::fields::FieldDef;
use tantivy::schema::{
    FAST, Field, INDEXED, STORED, STRING, Schema, SchemaBuilder, TEXT, TextFieldIndexing,
    TextOptions,
};

/// The fixed field names. Public so a test can assert against them rather than a magic string.
pub const ASSET_ID: &str = "asset_id";
pub const GROUP_IDS: &str = "group_ids";
pub const DELETED: &str = "deleted";
pub const TEXT_BLOB: &str = "text";
pub const METADATA: &str = "metadata";

/// The schema plus the handles a writer and a query renderer need.
///
/// Handles rather than repeated `get_field` lookups: the lookup returns a `Result` and doing it per
/// document per field turns an index write into a string comparison loop.
#[derive(Debug, Clone)]
pub struct IndexSchema {
    schema: Schema,
    asset_id: Field,
    group_ids: Field,
    deleted: Field,
    text: Field,
    metadata: Field,
    /// The definitions this schema was built for, in order.
    ///
    /// Kept so the writer knows which keys are textual — the same `is_textual` the validator and the SQL
    /// renderer use, so all three agree on what a free-text search covers.
    fields: Vec<FieldDef>,
}

impl IndexSchema {
    /// Builds the schema for a tenant.
    ///
    /// The schema itself does not depend on `defs` — that is the point of the JSON field — but the
    /// definitions are retained so the writer can shape documents consistently with the SQL renderer.
    pub fn new(defs: Vec<FieldDef>) -> Self {
        let mut builder: SchemaBuilder = Schema::builder();

        // `STRING` not `TEXT`: an id must match exactly, and a tokenising analyser would split a UUID on
        // its hyphens and match any asset sharing a segment.
        let asset_id = builder.add_text_field(ASSET_ID, STRING | STORED);

        // Multi-valued: one document, many groups. `STRING` for the same exact-match reason, and `FAST`
        // because the access filter touches it on every query.
        let group_ids = builder.add_text_field(GROUP_IDS, STRING | FAST);

        let deleted = builder.add_bool_field(DELETED, INDEXED | FAST);

        // Positions, so a quoted phrase can be a phrase query rather than a bag of words.
        let text = builder.add_text_field(
            TEXT_BLOB,
            TextOptions::default().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("default")
                    .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions),
            ),
        );

        // The dynamic half. Not `expand_dots_enabled`: a field key cannot contain a dot
        // (`field_defs_key_shape` forbids it), so enabling it would only create a way for a value
        // containing a dot to be read as a path.
        let metadata = builder.add_json_field(METADATA, TEXT | FAST);

        Self {
            schema: builder.build(),
            asset_id,
            group_ids,
            deleted,
            text,
            metadata,
            fields: defs,
        }
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn asset_id(&self) -> Field {
        self.asset_id
    }

    pub fn group_ids(&self) -> Field {
        self.group_ids
    }

    pub fn deleted(&self) -> Field {
        self.deleted
    }

    pub fn text(&self) -> Field {
        self.text
    }

    pub fn metadata(&self) -> Field {
        self.metadata
    }

    pub fn fields(&self) -> &[FieldDef] {
        &self.fields
    }

    /// The keys a free-text search covers, in definition order.
    ///
    /// The same rule `dam_core::fields::FieldKind::is_textual` gives the validator and the SQL renderer.
    /// Three consumers agreeing by construction rather than by coincidence is the §12 discipline applied
    /// one level down.
    pub fn text_keys(&self) -> Vec<&str> {
        self.fields
            .iter()
            .filter(|def| def.kind.is_textual())
            .map(|def| def.key.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dam_core::fields::{Constraints, FieldKind};

    fn def(key: &str, kind: FieldKind) -> FieldDef {
        FieldDef {
            key: key.to_owned(),
            kind,
            taxonomy_id: None,
            multivalued: false,
            required: false,
            read_only: false,
            ai_writable: false,
            constraints: Constraints::default(),
        }
    }

    #[test]
    fn the_schema_does_not_change_when_a_field_definition_is_added() {
        // The property the JSON field exists for. If this ever fails, adding a metadata field becomes a
        // full reindex of the tenant — hours for a large library, with search degraded throughout.
        let before = IndexSchema::new(vec![def("brand", FieldKind::Text)]);
        let after = IndexSchema::new(vec![
            def("brand", FieldKind::Text),
            def("year", FieldKind::Int),
            def("colour", FieldKind::Select),
        ]);
        assert_eq!(
            before.schema(),
            after.schema(),
            "adding a field definition must not change the Tantivy schema"
        );
    }

    #[test]
    fn an_empty_tenant_still_has_the_fixed_fields() {
        let schema = IndexSchema::new(vec![]);
        for name in [ASSET_ID, GROUP_IDS, DELETED, TEXT_BLOB, METADATA] {
            assert!(
                schema.schema().get_field(name).is_ok(),
                "{name} must exist even before any field is defined"
            );
        }
    }

    #[test]
    fn text_keys_match_the_validators_notion_of_textual() {
        let schema = IndexSchema::new(vec![
            def("brand", FieldKind::Text),
            def("year", FieldKind::Int),
            def("notes", FieldKind::LongText),
            def("live", FieldKind::Bool),
        ]);
        assert_eq!(schema.text_keys(), vec!["brand", "notes"]);
    }
}
