//! The hash under the tamper-evident audit log (G10).
//!
//! The chain is only worth the properties of its hash, and each of these is a way the construction could look
//! right and prove nothing:
//!
//! - **Distinct entries hash distinctly, including at field boundaries.** The formula in migration 0007 says
//!   to concatenate; concatenation lets meaning slide across a boundary without changing the digest, so one
//!   row's signature would cover a different row's content.
//! - **Absent is not empty.** A missing target and a present-but-blank target are different facts.
//! - **The payload's key order is not content.** Two writers who build the same object in different orders
//!   must produce the same hash, or verification depends on how a struct was serialised that day.
//! - **An array's order is content.** Reordering a list changes what it says.
//! - **The timestamp is fixed-width.** A trimming format would fail verification for exactly those entries
//!   whose microseconds end in zero — intermittent tamper evidence, which is worse than none.
//! - **The link is covered.** If `prev_hash` were outside the digest, a row could be relinked to a different
//!   predecessor without detection, and the chain would be a list.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, TimeZone as _, Utc};
use dam_core::audit::{self, CHAIN_VERSION, Link};
use serde_json::{Value, json};
use uuid::Uuid;

fn at(nanos: u32) -> DateTime<Utc> {
    Utc.timestamp_opt(1_775_000_000, nanos)
        .single()
        .expect("a representable instant")
}

fn actor() -> Uuid {
    Uuid::parse_str("11111111-1111-4111-8111-111111111111").expect("a fixed uuid")
}

fn link<'a>(payload: &'a Value, prev: Option<&'a str>) -> Link<'a> {
    Link {
        seq: 7,
        at: at(0),
        actor_id: Some(actor()),
        actor_kind: "user",
        action: "legal_hold.placed",
        target_kind: "asset",
        target_id: Some("abc"),
        payload,
        prev_hash: prev,
    }
}

#[test]
fn the_same_entry_hashes_the_same_way_twice() {
    let payload = json!({"reason": "litigation hold", "matter": "2026-114"});
    let once = audit::hash(&link(&payload, Some("deadbeef")));
    let again = audit::hash(&link(&payload, Some("deadbeef")));
    assert_eq!(once, again);
    // Lowercase hex of a sha256, so an auditor can reproduce it with sha256sum.
    assert_eq!(once.len(), 64);
    assert!(
        once.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
    );
}

#[test]
fn a_field_boundary_cannot_be_moved_without_changing_the_hash() {
    // The collision plain concatenation admits: "a" + "bc" and "ab" + "c" are the same bytes.
    let payload = json!({});
    let mut left = link(&payload, None);
    left.action = "a";
    left.target_kind = "bc";
    let mut right = link(&payload, None);
    right.action = "ab";
    right.target_kind = "c";
    assert_ne!(audit::hash(&left), audit::hash(&right));
}

#[test]
fn an_absent_target_and_an_empty_one_are_different_entries() {
    let payload = json!({});
    let mut absent = link(&payload, None);
    absent.target_id = None;
    let mut empty = link(&payload, None);
    empty.target_id = Some("");
    assert_ne!(audit::hash(&absent), audit::hash(&empty));
}

#[test]
fn an_absent_actor_and_an_absent_previous_hash_are_distinguished_from_each_other() {
    // Both render as "nothing", and both sit next to a field that can also be empty. If the encoding
    // collapsed either, a system action would forge as a user action with a missing id.
    let payload = json!({});
    let mut no_actor = link(&payload, Some("aa"));
    no_actor.actor_id = None;
    let mut no_prev = link(&payload, None);
    no_prev.actor_id = Some(actor());
    assert_ne!(audit::hash(&no_actor), audit::hash(&no_prev));
}

#[test]
fn the_payloads_key_order_is_not_part_of_the_entry() {
    let one: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).expect("valid json");
    let other: Value = serde_json::from_str(r#"{"b":2,"a":1}"#).expect("valid json");
    assert_eq!(
        audit::hash(&link(&one, None)),
        audit::hash(&link(&other, None))
    );
}

#[test]
fn nested_objects_are_sorted_at_every_level() {
    let deep: Value =
        serde_json::from_str(r#"{"z":{"y":1,"x":{"b":2,"a":3}}}"#).expect("valid json");
    assert_eq!(
        audit::canonical_json(&deep),
        r#"{"z":{"x":{"a":3,"b":2},"y":1}}"#
    );
}

#[test]
fn an_arrays_order_is_content_and_changes_the_hash() {
    let forwards = json!({"assets": ["a", "b"]});
    let backwards = json!({"assets": ["b", "a"]});
    assert_ne!(
        audit::hash(&link(&forwards, None)),
        audit::hash(&link(&backwards, None))
    );
}

#[test]
fn keys_sort_by_bytes_rather_than_by_anything_locale_shaped() {
    // Uppercase sorts before lowercase in byte order. A collation-aware sort would put "a" first in some
    // locales and not others, so the hash would depend on the server's environment.
    let mixed: Value = serde_json::from_str(r#"{"a":1,"B":2,"Z":3}"#).expect("valid json");
    assert_eq!(audit::canonical_json(&mixed), r#"{"B":2,"Z":3,"a":1}"#);
}

#[test]
fn the_canonical_payload_carries_no_whitespace() {
    let spaced: Value = serde_json::from_str("{ \"a\" : [ 1 , 2 ] }").expect("valid json");
    assert_eq!(audit::canonical_json(&spaced), r#"{"a":[1,2]}"#);
}

#[test]
fn a_microsecond_ending_in_zero_still_renders_six_digits() {
    // The failure this guards: `.5` written, `.500000` read back, one entry in ten reported as tampered.
    let payload = json!({});
    let half = audit::canonical(&link(&payload, None));
    let mut trailing = link(&payload, None);
    trailing.at = at(500_000_000);
    let rendered = String::from_utf8_lossy(&audit::canonical(&trailing)).to_string();
    assert!(
        rendered.contains(".500000Z"),
        "expected six fractional digits, got: {rendered}"
    );
    // And a zero-microsecond instant is not rendered bare.
    let zero = String::from_utf8_lossy(&half).to_string();
    assert!(
        zero.contains(".000000Z"),
        "expected six digits, got: {zero}"
    );
}

#[test]
fn sub_microsecond_precision_is_not_hashed_because_the_column_cannot_hold_it() {
    // timestamptz stores microseconds. If nanoseconds reached the digest, an entry would verify only until
    // it was read back — so two instants differing below the column's resolution must hash alike.
    let payload = json!({});
    let mut coarse = link(&payload, None);
    coarse.at = at(500_000_000);
    let mut finer = link(&payload, None);
    finer.at = at(500_000_999);
    assert_eq!(audit::hash(&coarse), audit::hash(&finer));
}

#[test]
fn relinking_an_entry_to_another_predecessor_changes_its_hash() {
    let payload = json!({});
    assert_ne!(
        audit::hash(&link(&payload, Some("aaaa"))),
        audit::hash(&link(&payload, Some("bbbb")))
    );
}

#[test]
fn the_sequence_number_is_covered() {
    let payload = json!({});
    let mut first = link(&payload, None);
    first.seq = 7;
    let mut second = link(&payload, None);
    second.seq = 8;
    assert_ne!(audit::hash(&first), audit::hash(&second));
}

#[test]
fn the_version_is_the_first_byte_so_a_format_change_is_visible() {
    let payload = json!({});
    let bytes = audit::canonical(&link(&payload, None));
    assert_eq!(bytes.first(), Some(&CHAIN_VERSION));
}

#[test]
fn a_string_payload_value_that_looks_like_structure_stays_a_string() {
    // Otherwise a caller could write `{"a": "1,\"b\":2"}` and have it hash as two fields.
    let injected = json!({"a": "1,\"b\":2"});
    let two_fields = json!({"a": 1, "b": 2});
    assert_ne!(
        audit::canonical_json(&injected),
        audit::canonical_json(&two_fields)
    );
    assert_eq!(audit::canonical_json(&injected), r#"{"a":"1,\"b\":2"}"#);
}
