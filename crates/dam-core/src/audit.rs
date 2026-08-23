//! The canonical form and hash of one audit entry (G10).
//!
//! `audit_log` has carried a `prev_hash` column since migration 0007, with the intended formula written in a
//! comment beside it and nothing computing it. This is the computation; `dam_db::audit` is the writer that
//! chains it. Kept here, pure, for the reason the schema comment gives: the canonicalisation has to be
//! explicit and version-pinned, and a hash whose definition lives inside a database query is a hash nobody
//! can re-derive.
//!
//! ## What tamper evidence actually buys
//!
//! `audit_log` refuses UPDATE and DELETE by rule, which is a control an auditor can be shown — and a control
//! a superuser can drop. So the rule is the fence and the chain is the alarm: altering a row changes its
//! hash, and removing one breaks the next row's link, and neither can be repaired without re-hashing every
//! later row. That is the honest claim. The chain does not make the log unalterable; it makes an alteration
//! *say so*.
//!
//! It follows that the chain proves nothing unless somebody verifies it. `dam_db::audit::verify` is the
//! other half, and an export that carries the hashes is what lets the check happen somewhere we do not
//! control.
//!
//! ## The schema comment specifies concatenation, and concatenation is wrong
//!
//! The formula in 0007 reads `seq || at || actor_id || action || target_kind || target_id || payload ||
//! prev_hash`. Joined that way, `action = "a", target_kind = "bc"` hashes identically to `action = "ab",
//! target_kind = "c"` — so one row's hash can cover a different row's content, and a caller who influences
//! any adjacent pair can move meaning across the boundary without changing the digest. Length prefixes make
//! the encoding injective. This is the same break [`crate::signed_url`] documents, in the same shape, and it
//! is why that module's framing rule is reused here rather than restated.
//!
//! An optional field needs more than an empty one. A `None` target id and a `Some("")` target id both render
//! as zero bytes, so a present-but-empty value would forge as an absent one; a marker byte inside the field
//! separates them.
//!
//! ## Why the payload is canonicalised rather than serialised
//!
//! `serde_json::to_string` renders object keys in whatever order the `Map` type holds them, and which type
//! that is depends on the `preserve_order` feature. Cargo features are additive across a workspace: any
//! crate — including one arriving three levels down in a dependency tree — that turns it on switches `Map`
//! to insertion order everywhere, and every hash written before that day stops verifying. The failure would
//! present as historical tamper evidence, which is the worst possible way to learn about a feature flag.
//!
//! Sorting the keys here costs an allocation per object and removes the dependency entirely.
//!
//! Array order is preserved, because in an array order is the content.
//!
//! ## Timestamps are fixed-width or they are a false alarm
//!
//! `at` is hashed as RFC 3339 with exactly six fractional digits, which is `timestamptz`'s own resolution.
//! Rendering with a trimming format instead would write `.5` for a value that reads back as `.500000`, so
//! roughly one entry in ten — the ones landing on a trailing zero — would fail verification. Intermittent
//! tamper evidence is worse than none: it teaches the reader that the alarm is noise.

use chrono::{DateTime, SecondsFormat, Utc};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

/// The version of the hash construction, hashed first.
///
/// So a change to the canonical form is a verification failure at the point of change rather than a silent
/// re-interpretation of every earlier row. A chain that spans a version bump verifies in two segments, and
/// the seam is visible.
pub const CHAIN_VERSION: u8 = 1;

/// The marker byte distinguishing a present value from an absent one.
const PRESENT: u8 = 1;

/// One entry's hashable content.
///
/// Borrowed rather than owned because both callers already hold the parts: the writer has them before the
/// insert, and the verifier has them after the read.
#[derive(Debug, Clone, Copy)]
pub struct Link<'a> {
    pub seq: i64,
    pub at: DateTime<Utc>,
    pub actor_id: Option<Uuid>,
    pub actor_kind: &'a str,
    pub action: &'a str,
    pub target_kind: &'a str,
    pub target_id: Option<&'a str>,
    /// Already normalised the way the database will store it — see `dam_db::audit::record`, which casts it
    /// through `jsonb` before hashing. `jsonb` rewrites some values on the way in — `-0.0` reads back as
    /// `0.0` — so hashing the value as submitted would fail to match the value as stored.
    pub payload: &'a serde_json::Value,
    /// `None` only for the first entry in a tenant's chain.
    pub prev_hash: Option<&'a str>,
}

/// The entry's hash, lowercase hex.
///
/// Hex rather than base64 so an auditor can reproduce a digest with `sha256sum` and compare by eye.
#[must_use]
pub fn hash(link: &Link<'_>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical(link));
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        // Cannot fail: writing to a String.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The exact bytes hashed, exposed so a test can assert the format and a reader can port it.
///
/// The format is the interface: an auditor re-verifying an export needs to build these bytes from the
/// exported columns, in another language, without this crate. Hiding the construction would make the
/// published hash unreproducible, which defeats the point of exporting it.
#[must_use]
pub fn canonical(link: &Link<'_>) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.push(CHAIN_VERSION);
    push_field(&mut out, &link.seq.to_be_bytes());
    push_field(&mut out, canonical_time(link.at).as_bytes());
    push_opt(
        &mut out,
        link.actor_id.as_ref().map(Uuid::as_bytes).map(|b| &b[..]),
    );
    push_field(&mut out, link.actor_kind.as_bytes());
    push_field(&mut out, link.action.as_bytes());
    push_field(&mut out, link.target_kind.as_bytes());
    push_opt(&mut out, link.target_id.map(str::as_bytes));
    push_field(&mut out, canonical_json(link.payload).as_bytes());
    push_opt(&mut out, link.prev_hash.map(str::as_bytes));
    out
}

/// The timestamp exactly as it is hashed.
///
/// Public because an export has to carry *this* string rather than whatever the serialiser would choose.
/// `chrono`'s own `Serialize` renders with `SecondsFormat::AutoSi`, which drops the fractional part entirely
/// when the microseconds are zero — so an extract serialised that way carries an `at` that does not reproduce
/// the digest beside it, and an auditor following the documented formula concludes the record was altered.
/// One function, used by both the hash and the view, is the only way those two stay in step.
#[must_use]
pub fn canonical_time(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// A payload rendered with object keys in byte order and no whitespace.
///
/// Two values that mean the same thing render the same way, and two that differ cannot render alike.
#[must_use]
pub fn canonical_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value);
    out
}

fn write_canonical(out: &mut String, value: &serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            // Sorted by the key's UTF-8 bytes rather than by any collation, because a collation is a locale
            // and a locale is a thing that changes under you.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                // Cannot be absent: the key came from this map.
                if let Some(child) = map.get(*key) {
                    write_canonical(out, child);
                }
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        }
        // Scalars go through serde_json's own rendering: its string escaping and number formatting are
        // deterministic, and reimplementing either would be a second definition of JSON to keep in step.
        other => out.push_str(&serde_json::to_string(other).unwrap_or_else(|_| "null".to_owned())),
    }
}

fn write_string(out: &mut String, value: &str) {
    let rendered = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned());
    out.push_str(&rendered);
}

fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    // A 32-bit big-endian length, as in `signed_url`: a payload or an action string can exceed 255 bytes and
    // a truncating length is a collision.
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn push_opt(out: &mut Vec<u8>, value: Option<&[u8]>) {
    match value {
        // A zero-length field for absent, and a marker byte ahead of the bytes for present. Without the
        // marker, `Some("")` and `None` encode identically and a present-but-empty target id forges as a
        // missing one.
        None => push_field(out, &[]),
        Some(bytes) => {
            let len = u32::try_from(bytes.len().saturating_add(1)).unwrap_or(u32::MAX);
            out.extend_from_slice(&len.to_be_bytes());
            out.push(PRESENT);
            out.extend_from_slice(bytes);
        }
    }
}
