//! One CSV vocabulary, shared by every export (Q.18).
//!
//! ## Why this is a module and not two similar functions
//!
//! An order's metadata export and a search-results export are the same document about different sets. Written
//! twice they would drift — a different column order, a different way of flattening a multivalued field, a
//! quoting rule fixed in one and not the other — and the person who notices is the one whose re-import fails
//! against a file that opened fine in a spreadsheet.
//!
//! ## The columns are the tenant's own vocabulary
//!
//! Field *keys*, not labels. `FieldDef` carries the validation shape rather than a display name, and a header
//! that matches the API's vocabulary is what somebody re-importing the file needs. The fixed columns come
//! first, in the order a person scans them.
//!
//! ## A cell is flat
//!
//! A multivalued field becomes `a; b` rather than `["a","b"]`. The point of an export is that somebody opens
//! it in a spreadsheet, and JSON in a cell is a thing they then have to undo.
use dam_core::fields::FieldDef;
use uuid::Uuid;

/// One asset as an export reads it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Row {
    pub id: Uuid,
    pub filename: String,
    pub mime: String,
    pub bytes: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub values: serde_json::Value,
}

/// The `SELECT` an export reads, over ids the caller has already been shown to be allowed.
pub const SELECT: &str = "SELECT assets.id, assets.filename, assets.mime, assets.bytes, \
                                 assets.width, assets.height, \
                                 coalesce(m.values, '{}'::jsonb) AS values \
                          FROM assets LEFT JOIN asset_metadata m ON m.asset_id = assets.id \
                          WHERE assets.id = ANY($1) AND assets.deleted_at IS NULL \
                          ORDER BY assets.filename";

/// The whole document: a header row, then one row per asset in `order`.
///
/// `order` decides the row order rather than the query, because an export of a *ranked* search should read in
/// the order the search returned — and a `SELECT ... WHERE id = ANY(...)` has no opinion about that. Ids in
/// `order` with no row are skipped silently: an asset deleted between the search and the export is not an
/// error, it is one fewer row.
#[must_use]
pub fn document(fields: &[FieldDef], rows: &[Row], order: &[Uuid]) -> String {
    let mut csv = String::new();
    csv.push_str("filename,mime,bytes,width,height");
    for field in fields {
        csv.push(',');
        csv.push_str(&cell(&field.key));
    }
    csv.push('\n');

    for id in order {
        let Some(row) = rows.iter().find(|row| row.id == *id) else {
            continue;
        };
        csv.push_str(&cell(&row.filename));
        csv.push(',');
        csv.push_str(&cell(&row.mime));
        csv.push_str(&format!(
            ",{},{},{}",
            row.bytes,
            row.width.map(|w| w.to_string()).unwrap_or_default(),
            row.height.map(|h| h.to_string()).unwrap_or_default()
        ));
        for field in fields {
            csv.push(',');
            csv.push_str(&cell(&flatten(row.values.get(&field.key))));
        }
        csv.push('\n');
    }
    csv
}

/// A CSV cell: quoted when it has to be, and never able to end the record early.
#[must_use]
pub fn cell(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// One stored metadata value as a cell. See the module docs on flatness.
#[must_use]
pub fn flatten(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| flatten(Some(item)))
            .filter(|rendered| !rendered.is_empty())
            .collect::<Vec<_>>()
            .join("; "),
        Some(other) => other.to_string(),
    }
}

/// The `Content-Type` and `Content-Disposition` an export answers with.
///
/// An attachment with a name, because a CSV rendered inline in a browser tab is a wall of commas rather than a
/// file somebody has.
#[must_use]
pub fn headers(filename: &str) -> [(axum::http::HeaderName, String); 2] {
    [
        (
            axum::http::header::CONTENT_TYPE,
            "text/csv; charset=utf-8".to_owned(),
        ),
        (
            axum::http::header::CONTENT_DISPOSITION,
            // The filename is built from a reference or a fixed string, never from caller input — a quote or a
            // newline in this header is a response-splitting bug rather than a formatting one.
            format!("attachment; filename=\"{}\"", cell(filename)),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cell_that_could_end_the_record_early_is_quoted() {
        assert_eq!(cell("plain"), "plain");
        assert_eq!(cell("a,b"), "\"a,b\"");
        assert_eq!(cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(cell("two\nlines"), "\"two\nlines\"");
    }

    #[test]
    fn a_multivalued_field_is_a_semicolon_list_rather_than_json() {
        let values = serde_json::json!({"colours": ["blue", "red"], "year": 2026, "empty": []});
        assert_eq!(flatten(values.get("colours")), "blue; red");
        assert_eq!(flatten(values.get("year")), "2026");
        assert_eq!(flatten(values.get("empty")), "");
        assert_eq!(flatten(values.get("absent")), "");
    }
}
