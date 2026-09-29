//! Reading an S3 Inventory report into [`InventoryEntry`]s.
//!
//! S3 Inventory writes one report per configured schedule (daily, here). Each run lands under a
//! date-stamped prefix as a `manifest.json` plus one or more gzipped CSV data files:
//!
//! ```text
//! <dest-prefix>/<source-bucket>/<config-id>/2026-09-29T01-00Z/manifest.json
//! <dest-prefix>/<source-bucket>/<config-id>/data/<uuid>.csv.gz
//! ```
//!
//! The manifest names the data files and, crucially, the *column order* — S3 Inventory has no fixed
//! schema; every field beyond `Bucket` and `Key` is opt-in and appears only if the inventory
//! configuration asked for it. So the reader resolves column positions from the manifest's declared
//! `fileSchema` rather than assuming an order, and refuses a report that omits `Size` (which the scrub
//! needs) with a message that names the fix.
//!
//! Everything here is pure: bytes in, entries out. The S3 round-trips that fetch those bytes live on
//! [`crate::S3Store`], which cannot be exercised without a bucket; the parsing — the part with the
//! error-prone edges (gzip, CSV quoting, URL-encoded keys, absent columns) — is unit-tested below.

use crate::{Error, InventoryEntry, Result};
use dam_core::StorageClass;
use serde::Deserialize;
use std::io::Read;
use std::str::FromStr;

/// The subset of a `manifest.json` the reader uses.
#[derive(Debug, Deserialize)]
pub(crate) struct Manifest {
    /// A comma-separated list of column names, e.g. `"Bucket, Key, Size, StorageClass"`. This is the
    /// authority on the CSV's column order — the data files carry no header row.
    #[serde(rename = "fileSchema")]
    pub file_schema: String,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ManifestFile {
    /// The object key of one gzipped CSV data file, relative to the destination bucket.
    pub key: String,
}

pub(crate) fn parse_manifest(bytes: &[u8]) -> Result<Manifest> {
    serde_json::from_slice(bytes)
        .map_err(|e| Error::Backend(format!("S3 inventory manifest parse: {e}")))
}

/// The columns the scrub needs, resolved to indices from the manifest's declared schema.
struct Columns {
    key: usize,
    size: usize,
    storage_class: Option<usize>,
}

impl Columns {
    fn from_schema(schema: &str) -> Result<Self> {
        let cols: Vec<&str> = schema.split(',').map(str::trim).collect();
        let find = |name: &str| cols.iter().position(|c| c.eq_ignore_ascii_case(name));
        let key = find("Key")
            .ok_or_else(|| Error::Backend("S3 inventory schema has no Key column".to_owned()))?;
        // Size is the one field the scrub cannot work without, and it is optional in S3 Inventory. A
        // report configured without it would otherwise parse fine and then compare every object against a
        // size of zero — so refuse it here, naming the field to add.
        let size = find("Size").ok_or_else(|| {
            Error::Backend(
                "S3 inventory schema has no Size column; add the Size optional field to the inventory \
                 configuration"
                    .to_owned(),
            )
        })?;
        Ok(Self {
            key,
            size,
            storage_class: find("StorageClass"),
        })
    }
}

/// Inflate one gzipped CSV data file and turn its rows into entries, using the manifest's column order.
pub(crate) fn entries_from_gzipped_csv(schema: &str, gz: &[u8]) -> Result<Vec<InventoryEntry>> {
    let cols = Columns::from_schema(schema)?;
    let mut text = String::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_string(&mut text)
        .map_err(|e| Error::Backend(format!("S3 inventory data gunzip: {e}")))?;

    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let fields = split_csv_line(line);
        let key = fields
            .get(cols.key)
            .map(|k| percent_decode(k))
            .unwrap_or_default();
        // A row with no key is unusable and never something S3 emits; skip it rather than record a
        // placement lookup that can never match.
        if key.is_empty() {
            continue;
        }
        let size = fields
            .get(cols.size)
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let storage_class = cols
            .storage_class
            .and_then(|i| fields.get(i))
            // An unrecognised class (S3 adds them over time, e.g. REDUCED_REDUNDANCY) falls back to
            // Standard rather than failing the whole run: the scrub compares size and probes the first
            // byte, neither of which depends on the class being one dam models.
            .and_then(|s| StorageClass::from_str(s).ok())
            .unwrap_or(StorageClass::Standard);
        out.push(InventoryEntry {
            key,
            size,
            storage_class,
            // The inventory's ETag is an MD5 (or a multipart composite), not the blake3 content hash dam
            // records in `object_placements.remote_checksum`; comparing the two would report every object
            // corrupt. So the S3 inventory carries no comparable checksum, and the scrub falls back to
            // size + the first-byte probe — exactly what the per-object S3 path does, since S3 claims no
            // server checksum either.
            checksum: None,
        });
    }
    Ok(out)
}

/// Split one RFC-4180 CSV line into unquoted field values.
///
/// A dozen lines rather than the `csv` crate: S3 Inventory emits every field double-quoted, the only
/// escape is a doubled `""`, and there are no embedded newlines within a field for the columns dam
/// requests. Pulling a crate (and its transitive tree) for that is more dependency-review surface than
/// the format warrants.
fn split_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            '"' => in_quotes = true,
            ',' if !in_quotes => fields.push(std::mem::take(&mut cur)),
            other => cur.push(other),
        }
    }
    fields.push(cur);
    fields
}

/// Decode `%XX` escapes in an S3 Inventory object key.
///
/// S3 Inventory URL-encodes keys. dam's own keys are content-addressed hex under a fixed prefix, so
/// this is a no-op for them in practice — but a store also holds imported objects with arbitrary names,
/// and a key that fails to decode back to what a placement recorded would be reported missing.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    fn gzip(s: &str) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(s.as_bytes()).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn a_manifest_yields_its_schema_and_data_files() {
        let json = br#"{
            "sourceBucket": "damrs-dev",
            "destinationBucket": "arn:aws:s3:::damrs-dev",
            "fileFormat": "CSV",
            "fileSchema": "Bucket, Key, Size, StorageClass",
            "files": [
                { "key": "inventory/damrs-dev/daily/data/a.csv.gz", "size": 42, "MD5checksum": "x" }
            ]
        }"#;
        let m = parse_manifest(json).unwrap();
        assert_eq!(m.file_schema, "Bucket, Key, Size, StorageClass");
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].key, "inventory/damrs-dev/daily/data/a.csv.gz");
    }

    #[test]
    fn rows_map_to_entries_by_the_declared_column_order() {
        let schema = "Bucket, Key, Size, StorageClass";
        let csv = "\"damrs-dev\",\"blobs/ab/cd/hash1\",\"1024\",\"STANDARD\"\n\
                   \"damrs-dev\",\"blobs/ef/01/hash2\",\"7\",\"GLACIER\"\n";
        let entries = entries_from_gzipped_csv(schema, &gzip(csv)).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "blobs/ab/cd/hash1");
        assert_eq!(entries[0].size, 1024);
        assert_eq!(entries[0].storage_class, StorageClass::Standard);
        assert_eq!(entries[0].checksum, None);
        assert_eq!(entries[1].key, "blobs/ef/01/hash2");
        assert_eq!(entries[1].size, 7);
        assert_eq!(entries[1].storage_class, StorageClass::Glacier);
    }

    #[test]
    fn a_different_column_order_is_honoured() {
        // The whole point of reading the schema: the same reader must cope with Size before Key.
        let schema = "Size, Key";
        let csv = "\"55\",\"imported/holiday photo.jpg\"\n";
        let entries = entries_from_gzipped_csv(schema, &gzip(csv)).unwrap();
        assert_eq!(entries[0].key, "imported/holiday photo.jpg");
        assert_eq!(entries[0].size, 55);
    }

    #[test]
    fn urlencoded_keys_are_decoded() {
        let schema = "Key, Size";
        // "a b/déjà.txt" as S3 Inventory encodes it.
        let csv = "\"a%20b/d%C3%A9j%C3%A0.txt\",\"3\"\n";
        let entries = entries_from_gzipped_csv(schema, &gzip(csv)).unwrap();
        assert_eq!(entries[0].key, "a b/déjà.txt");
    }

    #[test]
    fn a_key_with_an_embedded_comma_and_quote_survives_csv_quoting() {
        let schema = "Key, Size";
        // Field value: weird,"name".bin  → quotes doubled, whole field quoted.
        let csv = "\"weird,\"\"name\"\".bin\",\"9\"\n";
        let entries = entries_from_gzipped_csv(schema, &gzip(csv)).unwrap();
        assert_eq!(entries[0].key, "weird,\"name\".bin");
        assert_eq!(entries[0].size, 9);
    }

    #[test]
    fn a_schema_without_size_is_refused_with_a_fix() {
        let err = entries_from_gzipped_csv("Bucket, Key", &gzip("\"b\",\"k\"\n")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Size"),
            "message should name the missing field: {msg}"
        );
    }

    #[test]
    fn an_unknown_storage_class_falls_back_to_standard() {
        let schema = "Key, Size, StorageClass";
        let csv = "\"k\",\"1\",\"REDUCED_REDUNDANCY\"\n";
        let entries = entries_from_gzipped_csv(schema, &gzip(csv)).unwrap();
        assert_eq!(entries[0].storage_class, StorageClass::Standard);
    }
}
