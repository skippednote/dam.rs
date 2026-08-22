//! Metadata validation (2.1), the pure half.
//!
//! `field_defs` describes what a tenant's metadata means; this decides whether a payload conforms. It
//! runs before anything is written, so it is the only place a bad value can still be refused cheaply.
//!
//! Four choices here are worth more than the type checking, and each has a test:
//!
//! - **An unknown key is refused, never ignored.** Ignoring is the friendlier-looking option and it
//!   silently discards data the user believes they saved. `brnad: "Acme"` would return 200 and store
//!   nothing.
//! - **Every rejection is collected.** A validator that stops at the first error turns a twenty-field
//!   import into twenty round trips.
//! - **`required` applies on create, not on patch.** Enforcing it on a patch makes single-field updates
//!   impossible, which is most updates.
//! - **A `url` field refuses non-HTTP schemes.** A `javascript:` URL in a field the UI renders as a
//!   link is stored cross-site scripting, and metadata is exactly the kind of field that gets rendered
//!   as a link.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::fields::{self, Constraints, FieldDef, FieldKind, Mode, Writer};
use serde_json::{Map, json};
use uuid::Uuid;

fn def(key: &str, kind: FieldKind) -> FieldDef {
    FieldDef {
        key: key.to_owned(),
        kind,
        taxonomy_id: None,
        multivalued: false,
        required: false,
        read_only: false,
        ai_writable: false,
        facetable: false,
        constraints: Constraints::default(),
    }
}

/// Validates a payload written by a person, creating an asset.
fn create(
    defs: &[FieldDef],
    payload: serde_json::Value,
) -> Result<fields::Accepted, Vec<fields::Rejection>> {
    let object = payload.as_object().expect("an object").clone();
    fields::validate(defs, &object, Mode::Create, Writer::Human, &Map::new())
}

fn patch(
    defs: &[FieldDef],
    payload: serde_json::Value,
) -> Result<fields::Accepted, Vec<fields::Rejection>> {
    let object = payload.as_object().expect("an object").clone();
    fields::validate(defs, &object, Mode::Patch, Writer::Human, &Map::new())
}

fn codes(rejections: &[fields::Rejection]) -> Vec<&str> {
    rejections.iter().map(|r| r.code).collect()
}

// ─── the four choices ───────────────────────────────────────────────────────

#[test]
fn an_unknown_key_is_refused_rather_than_dropped() {
    // The most important one. A typo'd key that returns 200 and stores nothing is data loss the user
    // cannot see, and they find out when the field is empty months later.
    let defs = vec![def("brand", FieldKind::Text)];
    let rejected = create(&defs, json!({"brnad": "Acme"})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["unknown_field"]);
    assert_eq!(rejected[0].key, "brnad");
}

#[test]
fn every_rejection_is_reported_not_just_the_first() {
    // A twenty-field import that reports one problem per attempt is twenty round trips, and the person
    // fixing it has no idea how close they are.
    let defs = vec![
        def("brand", FieldKind::Text),
        def("year", FieldKind::Int),
        def("live", FieldKind::Bool),
    ];
    let rejected = create(
        &defs,
        json!({"brand": 42, "year": "not a number", "live": "yes"}),
    )
    .expect_err("must refuse");
    assert_eq!(rejected.len(), 3, "got {rejected:?}");
    let keys: Vec<&str> = rejected.iter().map(|r| r.key.as_str()).collect();
    assert!(keys.contains(&"brand") && keys.contains(&"year") && keys.contains(&"live"));
}

#[test]
fn required_is_enforced_on_create_and_not_on_patch() {
    // Most updates change one field. Enforcing `required` on a patch would demand the whole record
    // every time, which turns every edit into a read-modify-write with a lost-update race in it.
    let mut brand = def("brand", FieldKind::Text);
    brand.required = true;
    let defs = vec![brand, def("year", FieldKind::Int)];

    let rejected = create(&defs, json!({"year": 2026})).expect_err("create must refuse");
    assert_eq!(codes(&rejected), vec!["required"]);

    patch(&defs, json!({"year": 2026})).expect("a patch may omit a required field");
}

#[test]
fn a_url_field_refuses_a_javascript_scheme() {
    // Stored XSS if a UI renders the value as a link, which is what a URL field is for. `data:` is
    // refused for the same reason.
    let defs = vec![def("homepage", FieldKind::Url)];
    create(&defs, json!({"homepage": "https://example.com/a"})).expect("https is fine");

    for hostile in [
        "javascript:alert(1)",
        "data:text/html,<script>alert(1)</script>",
        "vbscript:msgbox",
        "file:///etc/passwd",
    ] {
        let rejected = create(&defs, json!({"homepage": hostile})).expect_err("must refuse");
        assert_eq!(codes(&rejected), vec!["url_scheme"], "accepted {hostile}");
    }
}

// ─── kinds ──────────────────────────────────────────────────────────────────

#[test]
fn an_int_field_refuses_a_fractional_number() {
    // JSON has one number type, so 1.0 and 1 are indistinguishable on the wire but 1.5 is not an int.
    // Truncating silently would store a value the caller never sent.
    let defs = vec![def("year", FieldKind::Int)];
    create(&defs, json!({"year": 2026})).expect("an integer");
    create(&defs, json!({"year": 2026.0})).expect("an integral float is an integer");

    let rejected = create(&defs, json!({"year": 2026.5})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["type"]);
}

#[test]
fn an_int_field_refuses_a_value_outside_i64() {
    // Postgres has no arbitrary-precision integer in a jsonb numeric context that survives a round
    // trip through i64, so a value that fits in JSON and not in the column must be refused here rather
    // than at the insert, where the error names a column instead of a field.
    let defs = vec![def("count", FieldKind::Int)];
    let rejected = create(&defs, json!({"count": 1e30})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["type"]);
}

#[test]
fn a_bool_field_refuses_the_string_true() {
    // Form encodings hand over "true" and "on" constantly. Coercing them means a field can never be
    // set to the *string* "true", and it hides a client that is not sending JSON types.
    let defs = vec![def("live", FieldKind::Bool)];
    create(&defs, json!({"live": true})).expect("a bool");
    let rejected = create(&defs, json!({"live": "true"})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["type"]);
}

#[test]
fn a_date_field_refuses_a_datetime_and_the_reverse() {
    // Different meanings. A `date` that accepts a timestamp silently acquires a timezone, and "shot on
    // 2026-08-17" becomes a different day depending on where it is read.
    let date = vec![def("shot_on", FieldKind::Date)];
    create(&date, json!({"shot_on": "2026-08-17"})).expect("a date");
    let rejected =
        create(&date, json!({"shot_on": "2026-08-17T10:00:00Z"})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["type"]);

    let datetime = vec![def("published_at", FieldKind::DateTime)];
    create(&datetime, json!({"published_at": "2026-08-17T10:00:00Z"})).expect("a datetime");
    let rejected =
        create(&datetime, json!({"published_at": "2026-08-17"})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["type"]);
}

#[test]
fn a_datetime_must_carry_an_offset() {
    // A local timestamp with no offset is ambiguous by up to 26 hours, and the ambiguity surfaces as
    // an embargo lifting on the wrong day.
    let defs = vec![def("published_at", FieldKind::DateTime)];
    let rejected =
        create(&defs, json!({"published_at": "2026-08-17T10:00:00"})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["type"]);
}

#[test]
fn a_geo_field_requires_a_lat_lon_pair_in_range() {
    let defs = vec![def("shot_at", FieldKind::Geo)];
    create(&defs, json!({"shot_at": {"lat": 51.5, "lon": -0.12}})).expect("a coordinate");

    for bad in [
        json!({"lat": 91.0, "lon": 0.0}),
        json!({"lat": 0.0, "lon": 181.0}),
        json!({"lat": 51.5}),
        json!([51.5, -0.12]),
    ] {
        let rejected = create(&defs, json!({"shot_at": bad})).expect_err("must refuse");
        assert!(!rejected.is_empty());
    }
}

#[test]
fn a_select_field_refuses_a_value_outside_its_enum() {
    let mut region = def("region", FieldKind::Select);
    region.constraints.enum_values = Some(vec!["emea".to_owned(), "apac".to_owned()]);
    let defs = vec![region];

    create(&defs, json!({"region": "emea"})).expect("in the enum");
    let rejected = create(&defs, json!({"region": "latam"})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["enum"]);
}

// ─── multivalued ────────────────────────────────────────────────────────────

#[test]
fn a_multivalued_field_refuses_a_bare_scalar() {
    // Coercion looks harmless until a client sends "red,blue" meaning two values. Wrapping it produces
    // one wrong value that nothing later can distinguish from a deliberate one.
    let mut colours = def("colours", FieldKind::Text);
    colours.multivalued = true;
    let defs = vec![colours];

    create(&defs, json!({"colours": ["red", "blue"]})).expect("an array");
    let rejected = create(&defs, json!({"colours": "red,blue"})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["not_multivalued"]);
}

#[test]
fn a_single_valued_field_refuses_an_array() {
    let defs = vec![def("brand", FieldKind::Text)];
    let rejected = create(&defs, json!({"brand": ["Acme", "Globex"]})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["multivalued"]);
}

#[test]
fn each_element_of_a_multivalued_field_is_checked() {
    // A validator that checks only the first element is a validator that passes ["2026", "oops"].
    let mut years = def("years", FieldKind::Int);
    years.multivalued = true;
    let defs = vec![years];

    let rejected = create(&defs, json!({"years": [2026, "oops", 2027]})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["type"]);
    assert!(
        rejected[0].detail.contains('1'),
        "the rejection must say which element: {}",
        rejected[0].detail
    );
}

// ─── who may write ──────────────────────────────────────────────────────────

#[test]
fn a_read_only_field_refuses_every_writer() {
    // `read_only` means system-maintained: dimensions, byte counts, content hashes. A client that can
    // set them can make the metadata disagree with the file.
    let mut width = def("width_px", FieldKind::Int);
    width.read_only = true;
    let defs = vec![width];

    for writer in [Writer::Human, Writer::Ai] {
        let object = json!({"width_px": 100})
            .as_object()
            .expect("object")
            .clone();
        let rejected = fields::validate(&defs, &object, Mode::Create, writer, &Map::new())
            .expect_err("read-only must refuse");
        assert_eq!(codes(&rejected), vec!["read_only"]);
    }
}

#[test]
fn enrichment_may_only_write_where_ai_writable_says_so() {
    // §8's governance boundary. Without it, an enrichment run can overwrite a caption a person wrote,
    // and the person has no way to protect it — which is how customers stop trusting AI features.
    let mut caption = def("caption", FieldKind::Text);
    caption.ai_writable = true;
    let legal = def("legal_notes", FieldKind::Text);
    let defs = vec![caption, legal];

    let allowed = json!({"caption": "a dog on a beach"})
        .as_object()
        .expect("object")
        .clone();
    fields::validate(&defs, &allowed, Mode::Patch, Writer::Ai, &Map::new())
        .expect("ai may write a caption");

    let refused = json!({"legal_notes": "probably fine"})
        .as_object()
        .expect("object")
        .clone();
    let rejected = fields::validate(&defs, &refused, Mode::Patch, Writer::Ai, &Map::new())
        .expect_err("ai must not write legal notes");
    assert_eq!(codes(&rejected), vec!["not_ai_writable"]);

    // And a person may write both, which is what makes the flag a restriction on enrichment rather
    // than on the field.
    let both = json!({"caption": "a dog", "legal_notes": "cleared"})
        .as_object()
        .expect("object")
        .clone();
    fields::validate(&defs, &both, Mode::Create, Writer::Human, &Map::new())
        .expect("a person may write both");
}

// ─── constraints ────────────────────────────────────────────────────────────

#[test]
fn length_and_range_constraints_are_applied() {
    let mut code = def("code", FieldKind::Text);
    code.constraints.min_length = Some(3);
    code.constraints.max_length = Some(5);
    let mut year = def("year", FieldKind::Int);
    year.constraints.min = Some(1900.0);
    year.constraints.max = Some(2100.0);
    let defs = vec![code, year];

    create(&defs, json!({"code": "abcd", "year": 2026})).expect("within bounds");
    assert_eq!(
        codes(&create(&defs, json!({"code": "ab"})).expect_err("too short")),
        vec!["min_length"]
    );
    assert_eq!(
        codes(&create(&defs, json!({"code": "abcdef"})).expect_err("too long")),
        vec!["max_length"]
    );
    assert_eq!(
        codes(&create(&defs, json!({"year": 1800})).expect_err("too small")),
        vec!["min"]
    );
}

#[test]
fn a_length_constraint_counts_characters_not_bytes() {
    // A 5-character limit that rejects "café" at 4 characters is a bug a European customer finds on
    // their first import, and an emoji makes it worse.
    let mut code = def("code", FieldKind::Text);
    code.constraints.max_length = Some(4);
    let defs = vec![code];
    create(&defs, json!({"code": "café"})).expect("four characters");
    create(&defs, json!({"code": "日本語だ"})).expect("four characters");
}

#[test]
fn a_pattern_constraint_is_anchored() {
    // An unanchored pattern matches a substring, so `^[A-Z]{3}$` written as `[A-Z]{3}` would accept
    // "oops ABC oops". Anchoring here means a tenant cannot write a permissive pattern by accident.
    let mut sku = def("sku", FieldKind::Text);
    sku.constraints.pattern = Some("[A-Z]{3}".to_owned());
    let defs = vec![sku];

    create(&defs, json!({"sku": "ABC"})).expect("a full match");
    let rejected = create(&defs, json!({"sku": "oops ABC oops"})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["pattern"]);
}

#[test]
fn an_unparseable_pattern_refuses_the_write_rather_than_passing_it() {
    // Fail closed. A tenant with a broken regex in their field definition must not silently get a
    // field with no validation at all — that is the failure mode where nobody notices for a year.
    let mut sku = def("sku", FieldKind::Text);
    sku.constraints.pattern = Some("([unclosed".to_owned());
    let defs = vec![sku];

    let rejected = create(&defs, json!({"sku": "anything"})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["pattern_invalid"]);
}

#[test]
fn an_oversized_pattern_is_refused_before_it_is_compiled() {
    // A field definition is tenant-controlled input reaching a regex compiler on every write. The
    // `regex` crate cannot backtrack, so this is not catastrophic blowup — but a megabyte of pattern
    // still costs real time per request, and there is no legitimate reason for one.
    let mut sku = def("sku", FieldKind::Text);
    sku.constraints.pattern = Some("a".repeat(10_000));
    let defs = vec![sku];

    let rejected = create(&defs, json!({"sku": "aaa"})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["pattern_invalid"]);
}

// ─── null and absence ───────────────────────────────────────────────────────

#[test]
fn an_explicit_null_clears_a_field_but_not_a_required_one() {
    // `null` and absent mean different things in a patch: absent is "leave it alone", null is "empty
    // it". Without the distinction there is no way to clear a field at all.
    let defs = vec![def("brand", FieldKind::Text)];
    let accepted = patch(&defs, json!({"brand": null})).expect("null clears");
    assert_eq!(accepted.values.get("brand"), Some(&serde_json::Value::Null));

    let mut required = def("brand", FieldKind::Text);
    required.required = true;
    let rejected =
        patch(&[required], json!({"brand": null})).expect_err("a required field cannot be cleared");
    assert_eq!(codes(&rejected), vec!["required"]);
}

#[test]
fn an_empty_payload_is_accepted_on_patch_and_checked_on_create() {
    let mut brand = def("brand", FieldKind::Text);
    brand.required = true;
    let defs = vec![brand];

    patch(&defs, json!({})).expect("an empty patch changes nothing");
    assert_eq!(
        codes(&create(&defs, json!({})).expect_err("create must supply required fields")),
        vec!["required"]
    );
}

// ─── taxonomy references ────────────────────────────────────────────────────

#[test]
fn a_taxonomy_ref_collects_the_terms_it_referenced_for_resolution() {
    // The pure layer cannot know which taxonomy a term belongs to — that is a row in the database. So
    // it validates the shape and hands back what has to be checked, and the caller resolves every term
    // in one query rather than one per value.
    let taxonomy = Uuid::from_u128(7);
    let mut category = def("category", FieldKind::TaxonomyRef);
    category.taxonomy_id = Some(taxonomy);
    category.multivalued = true;
    let defs = vec![category];

    let term_a = Uuid::from_u128(100);
    let term_b = Uuid::from_u128(101);
    let accepted = create(
        &defs,
        json!({"category": [term_a.to_string(), term_b.to_string()]}),
    )
    .expect("well-shaped");

    assert_eq!(
        accepted.taxonomy_refs,
        vec![
            fields::TaxonomyRef {
                key: "category".to_owned(),
                taxonomy_id: taxonomy,
                term_id: term_a
            },
            fields::TaxonomyRef {
                key: "category".to_owned(),
                taxonomy_id: taxonomy,
                term_id: term_b
            },
        ]
    );
}

#[test]
fn a_taxonomy_ref_that_is_not_a_uuid_is_refused_before_any_query() {
    // A slug or a label here is a client that has not resolved its terms, and turning arbitrary text
    // into a database lookup per value is how a metadata write becomes a scan.
    let mut category = def("category", FieldKind::TaxonomyRef);
    category.taxonomy_id = Some(Uuid::from_u128(7));
    let defs = vec![category];

    let rejected = create(&defs, json!({"category": "outdoor/beach"})).expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["type"]);
}

#[test]
fn a_taxonomy_ref_field_with_no_taxonomy_is_a_definition_error_not_a_value_error() {
    // The CHECK constraint makes this unreachable from the database, so if it happens the definition
    // came from somewhere else — and reporting it as a bad *value* would send someone to debug their
    // payload instead of their schema.
    let defs = vec![def("category", FieldKind::TaxonomyRef)];
    let rejected = create(&defs, json!({"category": Uuid::from_u128(1).to_string()}))
        .expect_err("must refuse");
    assert_eq!(codes(&rejected), vec!["definition_invalid"]);
}

// ─── output ─────────────────────────────────────────────────────────────────

#[test]
fn accepted_values_are_normalised_and_ordered() {
    // Deterministic output so a stored jsonb is byte-identical for equal input — which is what makes
    // "did this write change anything" answerable without a deep compare, and keeps a diff readable.
    let defs = vec![
        def("year", FieldKind::Int),
        def("brand", FieldKind::Text),
        def("live", FieldKind::Bool),
    ];
    let accepted = create(
        &defs,
        json!({"live": true, "year": 2026.0, "brand": "Acme"}),
    )
    .expect("valid");

    let keys: Vec<&String> = accepted.values.keys().collect();
    assert_eq!(keys, vec!["brand", "live", "year"], "keys must be ordered");
    // The integral float came in as 2026.0 and is stored as an integer, so two clients sending the
    // same value produce the same bytes.
    assert_eq!(accepted.values.get("year"), Some(&json!(2026)));
}

#[test]
fn a_text_value_keeps_its_whitespace_but_an_empty_string_is_not_a_value() {
    // Trimming would silently alter a value someone chose. But an empty string in a required field is
    // the classic "I filled it in" that satisfies presence and means nothing.
    let mut brand = def("brand", FieldKind::Text);
    brand.required = true;
    let defs = vec![brand];

    let accepted = create(&defs, json!({"brand": "  Acme  "})).expect("whitespace is preserved");
    assert_eq!(accepted.values.get("brand"), Some(&json!("  Acme  ")));

    let rejected = create(&defs, json!({"brand": "   "})).expect_err("blank is not a value");
    assert_eq!(codes(&rejected), vec!["required"]);
}

// ─── dependent fields (Q.19b) ───────────────────────────────────────────────

/// A field with a dependency, and the parent it hangs off.
fn dependent_defs() -> Vec<FieldDef> {
    vec![
        FieldDef {
            key: "has_people".to_owned(),
            kind: FieldKind::Bool,
            taxonomy_id: None,
            multivalued: false,
            required: false,
            read_only: false,
            ai_writable: false,
            facetable: true,
            constraints: Constraints::default(),
        },
        FieldDef {
            key: "release_reference".to_owned(),
            kind: FieldKind::Text,
            taxonomy_id: None,
            multivalued: false,
            // Required *and* dependent: the combination is the point. A model release is required for a
            // photograph with people in it and meaningless for one without.
            required: true,
            read_only: false,
            ai_writable: false,
            facetable: false,
            constraints: Constraints {
                depends_on: Some(dam_core::fields::Dependency {
                    key: "has_people".to_owned(),
                    values: vec!["true".to_owned()],
                }),
                ..Constraints::default()
            },
        },
    ]
}

#[test]
fn a_dependent_field_is_refused_when_its_condition_does_not_hold() {
    let defs = dependent_defs();
    let payload = json!({"has_people": false, "release_reference": "MR-1"});
    let object = payload.as_object().expect("object").clone();
    let rejections = fields::validate(&defs, &object, Mode::Create, Writer::Human, &Map::new())
        .expect_err("a release reference on a photograph with no people in it");
    assert_eq!(rejections[0].key, "release_reference");
    assert_eq!(rejections[0].code, "not_applicable");
    // The refusal names the parent and what it must be, because an administrator reading it has to know
    // which other field to change.
    assert!(
        rejections[0].detail.contains("has_people"),
        "{rejections:?}"
    );
    assert!(rejections[0].detail.contains("true"), "{rejections:?}");
}

#[test]
fn a_dependent_field_is_accepted_when_it_holds() {
    let defs = dependent_defs();
    let payload = json!({"has_people": true, "release_reference": "MR-1"});
    let object = payload.as_object().expect("object").clone();
    let accepted = fields::validate(&defs, &object, Mode::Create, Writer::Human, &Map::new())
        .expect("the condition holds");
    assert_eq!(
        accepted.values.get("release_reference"),
        Some(&json!("MR-1"))
    );
}

#[test]
fn the_condition_is_judged_on_the_document_as_it_will_be() {
    // The ordinary shape of an edit: fill in the child, leave the parent alone. Judging the payload alone
    // would refuse this, which would make a dependent field unfillable in one request.
    let defs = dependent_defs();
    let stored = json!({"has_people": true})
        .as_object()
        .expect("object")
        .clone();
    let patch = json!({"release_reference": "MR-2"})
        .as_object()
        .expect("object")
        .clone();
    let accepted = fields::validate(&defs, &patch, Mode::Patch, Writer::Human, &stored)
        .expect("the stored parent satisfies the condition");
    assert_eq!(
        accepted.values.get("release_reference"),
        Some(&json!("MR-2"))
    );

    // And the other way: a patch that changes the parent to something the child does not apply to refuses
    // the child in the same request, because the document as it will be is what counts.
    let both = json!({"has_people": false, "release_reference": "MR-3"})
        .as_object()
        .expect("object")
        .clone();
    let rejections = fields::validate(&defs, &both, Mode::Patch, Writer::Human, &stored)
        .expect_err("the parent changed in this very patch");
    assert_eq!(rejections[0].code, "not_applicable");
}

#[test]
fn a_required_dependent_field_is_required_only_when_it_applies() {
    let defs = dependent_defs();

    // No people: the release reference is not required, and its absence is not a rejection.
    let without = json!({"has_people": false})
        .as_object()
        .expect("object")
        .clone();
    fields::validate(&defs, &without, Mode::Create, Writer::Human, &Map::new())
        .expect("a photograph with no people needs no release");

    // People: it is required, and the message is the ordinary required one rather than anything special.
    let with = json!({"has_people": true})
        .as_object()
        .expect("object")
        .clone();
    let rejections = fields::validate(&defs, &with, Mode::Create, Writer::Human, &Map::new())
        .expect_err("a photograph with people needs a release");
    assert_eq!(rejections[0].key, "release_reference");
    assert_eq!(rejections[0].code, "required");
}

#[test]
fn a_field_that_became_irrelevant_can_still_be_cleared() {
    // The tidy-up path. A parent flipped by mistake leaves a value behind — deliberately, because deleting
    // somebody's work on a checkbox change is worse — and clearing it must not be refused as inapplicable.
    let defs = dependent_defs();
    let stored = json!({"has_people": false, "release_reference": "MR-4"})
        .as_object()
        .expect("object")
        .clone();
    let clearing = json!({"release_reference": null})
        .as_object()
        .expect("object")
        .clone();
    let rejections = fields::validate(&defs, &clearing, Mode::Patch, Writer::Human, &stored)
        .expect_err("it is a required field, so clearing is refused for *that* reason");
    assert_eq!(
        rejections[0].code, "required",
        "the refusal must be about requiredness, not applicability: {rejections:?}"
    );
}

#[test]
fn a_multivalued_parent_matches_on_any_of_its_values() {
    // "When the shoot is tagged editorial" is what somebody writing the rule means, and a shoot carries
    // several tags.
    let defs = vec![
        FieldDef {
            key: "uses".to_owned(),
            kind: FieldKind::Text,
            taxonomy_id: None,
            multivalued: true,
            required: false,
            read_only: false,
            ai_writable: false,
            facetable: true,
            constraints: Constraints::default(),
        },
        FieldDef {
            key: "editorial_note".to_owned(),
            kind: FieldKind::Text,
            taxonomy_id: None,
            multivalued: false,
            required: false,
            read_only: false,
            ai_writable: false,
            facetable: false,
            constraints: Constraints {
                depends_on: Some(dam_core::fields::Dependency {
                    key: "uses".to_owned(),
                    values: vec!["editorial".to_owned()],
                }),
                ..Constraints::default()
            },
        },
    ];
    let payload = json!({"uses": ["advertising", "editorial"], "editorial_note": "page 12"})
        .as_object()
        .expect("object")
        .clone();
    fields::validate(&defs, &payload, Mode::Create, Writer::Human, &Map::new())
        .expect("any value matching is enough");
}

#[test]
fn a_dependency_round_trips_through_the_validation_json() {
    // The definition lives in `field_defs.validation`, so the read and the write have to agree — and an
    // older build reading a newer definition ignores what it does not know, which here means showing the
    // field always rather than refusing data.
    let constraints = Constraints {
        depends_on: Some(dam_core::fields::Dependency {
            key: "has_people".to_owned(),
            values: vec!["true".to_owned(), "maybe".to_owned()],
        }),
        max_length: Some(40),
        ..Constraints::default()
    };
    let json = constraints.to_json();
    assert_eq!(Constraints::from_json(&json), constraints);

    // A dependency with no values could never hold, so it is read as absent rather than stored as a field
    // nobody can fill.
    let empty = json!({"depends_on": {"key": "has_people", "values": []}});
    assert_eq!(Constraints::from_json(&empty).depends_on, None);
}
