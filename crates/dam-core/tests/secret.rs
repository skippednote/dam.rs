//! A secret must not leak through `Debug`, `Display`, or serialisation.
//!
//! Config and error values end up in logs, spans, and OTel attributes. The only
//! reliable defence is a type whose formatting cannot reveal the value, because
//! every other approach depends on remembering at each call site.

// Panicking IS the assertion in a test, so the workspace's `unwrap_used` /
// `expect_used` denials are relaxed here only. `result_large_err` fires on
// figment::Jail closures, whose Err type we do not control.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::result_large_err)]

use dam_core::Secret;

const PLAINTEXT: &str = "super-secret-value-9f2a";

#[test]
fn debug_does_not_reveal_the_value() {
    let s = Secret::new(PLAINTEXT.to_owned());
    let rendered = format!("{s:?}");
    assert!(!rendered.contains(PLAINTEXT), "Debug leaked: {rendered}");
    assert!(rendered.contains("REDACTED"));
}

#[test]
fn display_does_not_reveal_the_value() {
    let s = Secret::new(PLAINTEXT.to_owned());
    assert!(!format!("{s}").contains(PLAINTEXT));
}

#[test]
fn nested_in_a_struct_debug_it_still_does_not_leak() {
    #[derive(Debug)]
    #[allow(dead_code)]
    struct Holder {
        name: &'static str,
        key: Secret<String>,
    }
    let h = Holder {
        name: "signing",
        key: Secret::new(PLAINTEXT.to_owned()),
    };
    let rendered = format!("{h:?}");
    assert!(
        !rendered.contains(PLAINTEXT),
        "leaked via parent: {rendered}"
    );
    assert!(
        rendered.contains("signing"),
        "non-secret fields still print"
    );
}

#[test]
fn serialising_does_not_reveal_the_value() {
    let s = Secret::new(PLAINTEXT.to_owned());
    let json = serde_json::to_string(&s).expect("serialise");
    assert!(!json.contains(PLAINTEXT), "Serialize leaked: {json}");
}

#[test]
fn expose_is_the_single_explicit_way_out() {
    let s = Secret::new(PLAINTEXT.to_owned());
    assert_eq!(s.expose(), PLAINTEXT);
}
