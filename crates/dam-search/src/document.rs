//! Building index documents (2.6).
//!
//! One document per asset. What goes in is decided here rather than at each call site, so the SQL and
//! Tantivy back ends are fed from one description of what an asset *is* for search purposes.
//!
//! ## The group ids are the thing to be careful about
//!
//! An asset's group membership is written into its document so the access predicate can be rendered into
//! the Tantivy query. That filter is a **narrowing optimisation, not the authority**: membership changes
//! in Postgres the instant an administrator saves, and the index catches up when the asset is reindexed.
//! Between those two moments the index is stale, and a stale *permissive* index is a leak.
//!
//! So results from Tantivy are hydrated through Postgres with the same predicate applied there — see
//! [`crate::query`]. Tantivy ranks; Postgres authorises. Anything else makes an eventually-consistent
//! index the gate on a governed library.

use crate::schema::IndexSchema;
use dam_core::fields::FieldKind;
use tantivy::TantivyDocument;
use uuid::Uuid;

/// Everything the index needs about one asset.
#[derive(Debug, Clone)]
pub struct AssetDocument {
    pub asset_id: Uuid,
    pub filename: String,
    pub deleted: bool,
    /// The asset groups this asset belongs to. See the module docs.
    pub group_ids: Vec<Uuid>,
    /// The validated metadata, as stored in `asset_metadata.values`.
    pub values: serde_json::Map<String, serde_json::Value>,
}

impl AssetDocument {
    /// Renders this asset into a Tantivy document.
    ///
    /// The text blob is assembled here rather than left as N searchable fields, because the field set is
    /// per tenant: a query over a varying field list cannot be compared between back ends, and the
    /// differential test needs it to be.
    pub fn to_tantivy(&self, schema: &IndexSchema) -> TantivyDocument {
        let mut doc = TantivyDocument::new();
        doc.add_text(schema.asset_id(), self.asset_id.to_string());
        doc.add_bool(schema.deleted(), self.deleted);
        for group in &self.group_ids {
            doc.add_text(schema.group_ids(), group.to_string());
        }

        // The filename first, then every textual field's values. The same set the SQL renderer searches,
        // in the same order, from the same `is_textual` rule.
        let mut blob = String::from(&self.filename);
        for key in schema.text_keys() {
            for value in text_values(self.values.get(key)) {
                blob.push(' ');
                blob.push_str(&value);
            }
        }
        doc.add_text(schema.text(), blob);

        // The metadata verbatim into the JSON field, so a field query can reach a typed value without the
        // schema having to know the key existed.
        //
        // Filtered to the *defined* fields: a stale key left in `values` by a removed definition would
        // otherwise stay searchable, and an administrator who deleted a field would reasonably expect it
        // to stop being searchable.
        let mut indexed = serde_json::Map::new();
        for def in schema.fields() {
            if let Some(value) = self.values.get(&def.key) {
                indexed.insert(def.key.clone(), normalise(def.kind, value));
            }
        }
        // Through Tantivy's own value type. `OwnedValue::from` on a `serde_json::Value` is the
        // conversion Tantivy defines, so the numeric and string mappings are its own rather than ours —
        // which matters because the query renderer builds terms with the same mapping.
        let object: std::collections::BTreeMap<String, tantivy::schema::OwnedValue> = indexed
            .into_iter()
            .map(|(key, value)| (key, tantivy::schema::OwnedValue::from(value)))
            .collect();
        doc.add_object(schema.metadata(), object);
        doc
    }
}

/// A value's text, flattened over arrays.
///
/// Without the array branch a free-text search silently misses every value in a multivalued field, which
/// is the same trap the SQL renderer had — and there it produced "search does not find my tags" with no
/// error anywhere.
fn text_values(value: Option<&serde_json::Value>) -> Vec<String> {
    match value {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        Some(serde_json::Value::String(text)) => vec![text.clone()],
        Some(other) => vec![other.to_string()],
    }
}

/// Coerces a stored value into the JSON shape the query renderer will look for.
///
/// Dates and timestamps stay strings, matching what the validator normalised and what the SQL renderer
/// compares against. Converting them to Tantivy dates here would make the two back ends compare different
/// things, and the difference would only show at a boundary.
fn normalise(kind: FieldKind, value: &serde_json::Value) -> serde_json::Value {
    match kind {
        // An integral float and an integer are the same number, and the validator already stored an
        // integer — this guards a value that arrived another way.
        FieldKind::Int => match value.as_f64() {
            Some(number) if number.fract() == 0.0 => serde_json::json!(number as i64),
            _ => value.clone(),
        },
        _ => value.clone(),
    }
}
