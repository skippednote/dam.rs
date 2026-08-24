//! Mapping a source library's metadata onto this one's (G7).
//!
//! GAPS §G7: "underestimating metadata cleanup is the single most common cause of failed DAM migrations." So the
//! properties worth defending are all about *not losing quietly*:
//!
//! - **An empty source cell is not a finding.** A CSV header lists every column and most rows leave most blank;
//!   reporting each one would bury the twelve that matter under forty thousand that do not, and a report nobody
//!   reads certifies nothing.
//! - **A value that carried and went nowhere is always a finding**, even if the crosswalk looks complete.
//! - **Nothing is guessed.** An unparseable date is dropped and named rather than parsed hopefully — a plausible
//!   wrong date is worse than a missing one because nobody notices it for two years.
//! - **A mapping miss is the caller's decision**, because keep, drop and fail are each right somewhere and
//!   defaulting would silently pick one.
//! - **Coverage is per source field**, because "did my Photographer column arrive" is the question, and it is
//!   the finding that stops a migration being signed off.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::crosswalk::{self, Coverage, Crosswalk, OnMiss, Report, Rule, Transform, code};
use dam_core::fields::{Constraints, FieldDef, FieldKind, Mode, Writer};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

fn def(key: &str, kind: FieldKind, multivalued: bool, required: bool) -> FieldDef {
    FieldDef {
        key: key.to_owned(),
        kind,
        taxonomy_id: None,
        multivalued,
        required,
        read_only: false,
        ai_writable: false,
        facetable: false,
        constraints: Constraints::default(),
    }
}

fn defs() -> Vec<FieldDef> {
    vec![
        def("caption", FieldKind::Text, false, false),
        def("keywords", FieldKind::MultiSelect, true, false),
        def("shot_on", FieldKind::Date, false, false),
        def("brand", FieldKind::Text, false, true),
        FieldDef {
            read_only: true,
            ..def("ingested_at", FieldKind::DateTime, false, false)
        },
    ]
}

fn record(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn rule(source: &str, target: &str, transform: Transform) -> Rule {
    Rule {
        source: source.to_owned(),
        target: target.to_owned(),
        transform,
    }
}

#[test]
fn an_empty_source_cell_is_not_a_finding() {
    // The property that decides whether the report is readable at all. A CSV of forty thousand rows with twenty
    // columns has hundreds of thousands of blanks in it.
    let crosswalk = Crosswalk {
        rules: vec![rule("Title", "caption", Transform::Copy)],
        ignored: vec![],
    };
    let blank = record(&[
        ("Title", json!("  ")),
        ("Notes", json!("")),
        ("Photographer", Value::Null),
        ("Tags", json!([])),
    ]);
    let mapped = crosswalk::apply(&crosswalk, &blank, &defs());
    assert!(mapped.payload.is_empty(), "{:?}", mapped.payload);
    assert!(
        mapped.warnings.is_empty(),
        "nothing was lost, so nothing is reported: {:?}",
        mapped.warnings
    );
}

#[test]
fn a_value_that_went_nowhere_is_always_reported() {
    // The report's most important column. A crosswalk can look complete and still be dropping the one field the
    // customer cares about.
    let crosswalk = Crosswalk {
        rules: vec![rule("Title", "caption", Transform::Copy)],
        ignored: vec![],
    };
    let mapped = crosswalk::apply(
        &crosswalk,
        &record(&[
            ("Title", json!("A harbour at dawn")),
            ("Photographer", json!("Ada Lovelace")),
        ]),
        &defs(),
    );
    assert_eq!(mapped.payload["caption"], json!("A harbour at dawn"));
    assert_eq!(mapped.warnings.len(), 1, "{:?}", mapped.warnings);
    assert_eq!(mapped.warnings[0].source, "Photographer");
    assert_eq!(mapped.warnings[0].code, code::UNMAPPED);

    // Unless it was *decided* against. "Not mapped yet" and "deliberately dropped" are different states, and
    // only one is a finding — which is what lets the column shrink to nothing and mean something.
    let decided = Crosswalk {
        rules: vec![rule("Title", "caption", Transform::Copy)],
        ignored: vec!["Photographer".to_owned()],
    };
    let mapped = crosswalk::apply(
        &decided,
        &record(&[
            ("Title", json!("A harbour at dawn")),
            ("Photographer", json!("Ada Lovelace")),
        ]),
        &defs(),
    );
    assert!(mapped.warnings.is_empty(), "{:?}", mapped.warnings);
}

#[test]
fn a_date_is_parsed_by_a_declared_format_or_dropped_and_named() {
    // `03/04/2026` is two different dates depending on the source's locale. A guesser that got it right in
    // testing will get it wrong on a customer's data, and a plausible wrong date is worse than a missing one.
    let uk = Crosswalk {
        rules: vec![rule(
            "Shot",
            "shot_on",
            Transform::Date {
                format: "%d/%m/%Y".to_owned(),
            },
        )],
        ignored: vec![],
    };
    let mapped = crosswalk::apply(&uk, &record(&[("Shot", json!("03/04/2026"))]), &defs());
    assert_eq!(mapped.payload["shot_on"], json!("2026-04-03"), "day first");

    let us = Crosswalk {
        rules: vec![rule(
            "Shot",
            "shot_on",
            Transform::Date {
                format: "%m/%d/%Y".to_owned(),
            },
        )],
        ignored: vec![],
    };
    let mapped = crosswalk::apply(&us, &record(&[("Shot", json!("03/04/2026"))]), &defs());
    assert_eq!(
        mapped.payload["shot_on"],
        json!("2026-03-04"),
        "the same string, the other date — which is why the format is required",
    );

    // And a value that does not match is dropped with its reason, not coerced.
    let mapped = crosswalk::apply(&uk, &record(&[("Shot", json!("last Tuesday"))]), &defs());
    assert!(!mapped.payload.contains_key("shot_on"));
    assert_eq!(mapped.warnings.len(), 1);
    assert_eq!(mapped.warnings[0].code, code::BAD_DATE);
    assert!(
        mapped.warnings[0].detail.contains("%d/%m/%Y"),
        "the reason names the format it was measured against: {}",
        mapped.warnings[0].detail
    );
}

#[test]
fn a_packed_cell_becomes_many_values_and_refuses_to_guess_when_it_cannot() {
    let split = Transform::Split { on: ";".to_owned() };
    let crosswalk = Crosswalk {
        rules: vec![rule("Tags", "keywords", split.clone())],
        ignored: vec![],
    };
    let mapped = crosswalk::apply(
        &crosswalk,
        &record(&[("Tags", json!("harbour; dawn ; boats"))]),
        &defs(),
    );
    // Trimmed, and the empty piece a trailing delimiter leaves is not a keyword.
    assert_eq!(
        mapped.payload["keywords"],
        json!(["harbour", "dawn", "boats"]),
    );

    // Into a single-valued field, several values are dropped rather than joined or truncated. Both of those are
    // guesses, and the point of the report is that somebody decides.
    let into_one = Crosswalk {
        rules: vec![rule("Tags", "caption", split)],
        ignored: vec![],
    };
    let mapped = crosswalk::apply(
        &into_one,
        &record(&[("Tags", json!("harbour; dawn"))]),
        &defs(),
    );
    assert!(!mapped.payload.contains_key("caption"));
    assert_eq!(mapped.warnings[0].code, code::TOO_MANY_VALUES);
}

#[test]
fn one_value_into_a_multivalued_field_still_lands_as_a_list() {
    // Otherwise `validate` refuses a bare string where the field holds an array, and a successful mapping
    // becomes a failed record for a reason invisible from the crosswalk.
    let crosswalk = Crosswalk {
        rules: vec![rule("Tag", "keywords", Transform::Copy)],
        ignored: vec![],
    };
    let mapped = crosswalk::apply(&crosswalk, &record(&[("Tag", json!("harbour"))]), &defs());
    assert_eq!(mapped.payload["keywords"], json!(["harbour"]));
}

#[test]
fn a_mapping_miss_is_the_callers_decision_in_all_three_directions() {
    let mut table = BTreeMap::new();
    table.insert("RF".to_owned(), "royalty_free".to_owned());

    for (on_miss, expected) in [
        // An open vocabulary: a keyword nobody has seen is still a keyword.
        (OnMiss::Keep, Some(json!("RM"))),
        // A closed list: an unknown value is noise.
        (OnMiss::Drop, None),
    ] {
        let crosswalk = Crosswalk {
            rules: vec![rule(
                "Licence",
                "caption",
                Transform::Map {
                    table: table.clone(),
                    on_miss,
                },
            )],
            ignored: vec![],
        };
        let mapped = crosswalk::apply(&crosswalk, &record(&[("Licence", json!("RM"))]), &defs());
        assert_eq!(
            mapped.payload.get("caption").cloned(),
            expected,
            "{on_miss:?}"
        );
        assert!(mapped.fatal.is_none());
    }

    // Anything a rights decision rests on: the asset must not arrive without it.
    let fail = Crosswalk {
        rules: vec![rule(
            "Licence",
            "caption",
            Transform::Map {
                table: table.clone(),
                on_miss: OnMiss::Fail,
            },
        )],
        ignored: vec![],
    };
    let mapped = crosswalk::apply(&fail, &record(&[("Licence", json!("RM"))]), &defs());
    let fatal = mapped.fatal.expect("fatal");
    assert_eq!(fatal.code, code::UNMAPPED_VALUE);
    assert!(fatal.detail.contains("RM"), "{}", fatal.detail);

    // And a hit maps, whichever the policy.
    let mapped = crosswalk::apply(&fail, &record(&[("Licence", json!("RF"))]), &defs());
    assert_eq!(mapped.payload["caption"], json!("royalty_free"));
    assert!(mapped.fatal.is_none());
}

#[test]
fn a_rule_pointing_at_nothing_or_at_a_system_field_says_so() {
    // A mistyped target would otherwise present as a field that mysteriously never populates — the worst kind
    // of migration bug, because the crosswalk looks right.
    let crosswalk = Crosswalk {
        rules: vec![
            rule("Title", "captionn", Transform::Copy),
            rule("Imported", "ingested_at", Transform::Copy),
        ],
        ignored: vec![],
    };
    let mapped = crosswalk::apply(
        &crosswalk,
        &record(&[("Title", json!("x")), ("Imported", json!("2026-01-01"))]),
        &defs(),
    );
    let codes: Vec<&str> = mapped.warnings.iter().map(|w| w.code).collect();
    assert!(
        codes.contains(&code::UNKNOWN_TARGET),
        "{:?}",
        mapped.warnings
    );
    assert!(
        codes.contains(&code::READ_ONLY_TARGET),
        "{:?}",
        mapped.warnings
    );
    assert!(mapped.payload.is_empty());
}

#[test]
fn a_constant_fills_a_field_the_source_never_had() {
    // For a field the migration decides rather than the source: "imported from the old DAM", a default rights state.
    let crosswalk = Crosswalk {
        rules: vec![rule(
            "",
            "brand",
            Transform::Constant {
                value: json!("acme"),
            },
        )],
        ignored: vec![],
    };
    let mapped = crosswalk::apply(&crosswalk, &record(&[("Title", json!("x"))]), &defs());
    assert_eq!(mapped.payload["brand"], json!("acme"));
    // The unmapped `Title` is still reported: a constant does not excuse a loss elsewhere.
    assert_eq!(mapped.warnings.len(), 1);
    assert_eq!(mapped.warnings[0].source, "Title");
}

#[test]
fn a_dry_run_report_names_the_column_that_never_arrives() {
    // The finding that stops a migration being signed off, and the reason coverage is per *source* field.
    let crosswalk = Crosswalk {
        rules: vec![
            rule("Title", "caption", Transform::Copy),
            rule(
                "Shot",
                "shot_on",
                Transform::Date {
                    format: "%Y-%m-%d".to_owned(),
                },
            ),
        ],
        ignored: vec![],
    };
    let defs = defs();
    let mut report = Report::default();

    for row in 0..5 {
        let source = record(&[
            ("Title", json!(format!("Photo {row}"))),
            // Never parses under the declared format, so this column is a total loss.
            ("Shot", json!("03/04/2026")),
            ("Photographer", json!("Ada")),
        ]);
        let mapped = crosswalk::apply(&crosswalk, &source, &defs);
        // The *real* validator, which is the whole point: a dry run with its own idea of validity would certify
        // something different from what the transfer does.
        let outcome = dam_core::fields::validate(
            &defs,
            &mapped.payload,
            Mode::Create,
            Writer::Human,
            &Map::new(),
        );
        if let Err(rejections) = &outcome {
            crosswalk::accrue_rejections(&mut report, rejections);
        }
        crosswalk::accrue(&mut report, &crosswalk, &source, &mapped, outcome.is_ok());
    }

    assert_eq!(report.records, 5);
    // `brand` is required and nothing maps to it, so every record would be refused — by the real validator,
    // not by a guess.
    assert_eq!(report.would_arrive, 0);
    assert_eq!(report.would_be_invalid, 5);
    assert!(report.is_futile(), "nothing would arrive at all");
    assert!(
        report.rejections.values().sum::<u64>() >= 5,
        "the validator's own reasons, grouped: {:?}",
        report.rejections
    );

    // Coverage per source field. `Title` arrives; `Shot` and `Photographer` do not.
    assert_eq!(
        report.coverage.get("Title").copied(),
        Some(Coverage {
            present: 5,
            mapped: 5,
            ignored: false
        }),
    );
    let losses = report.total_losses();
    let names: Vec<&str> = losses.iter().map(|(name, _)| *name).collect();
    assert!(names.contains(&"Shot"), "{losses:?}");
    assert!(names.contains(&"Photographer"), "{losses:?}");
    assert!(!names.contains(&"Title"));

    // Forty thousand losses read as a handful of rows, which is what makes the report usable.
    assert_eq!(report.warnings.get(code::BAD_DATE).copied(), Some(5));
    assert_eq!(report.warnings.get(code::UNMAPPED).copied(), Some(5));
}

#[test]
fn a_report_over_a_workable_crosswalk_says_so() {
    // The other side: the same machinery has to be able to say "this is fine", or nobody would trust it saying
    // otherwise.
    let mut crosswalk = Crosswalk {
        rules: vec![
            rule("Title", "caption", Transform::Copy),
            rule(
                "Shot",
                "shot_on",
                Transform::Date {
                    format: "%d/%m/%Y".to_owned(),
                },
            ),
            rule("Brand", "brand", Transform::Copy),
            rule("Tags", "keywords", Transform::Split { on: ";".to_owned() }),
        ],
        ignored: vec!["Internal Notes".to_owned()],
    };
    let defs = defs();
    let mut report = Report::default();

    for row in 0..3 {
        let source = record(&[
            ("Title", json!(format!("Photo {row}"))),
            ("Shot", json!("03/04/2026")),
            ("Brand", json!("acme")),
            ("Tags", json!("harbour;dawn")),
            ("Internal Notes", json!("do not migrate")),
        ]);
        let mapped = crosswalk::apply(&crosswalk, &source, &defs);
        let outcome = dam_core::fields::validate(
            &defs,
            &mapped.payload,
            Mode::Create,
            Writer::Human,
            &Map::new(),
        );
        assert!(outcome.is_ok(), "{outcome:?}");
        crosswalk::accrue(&mut report, &crosswalk, &source, &mapped, outcome.is_ok());
    }

    assert_eq!(report.would_arrive, 3);
    assert_eq!(report.would_be_invalid, 0);
    assert_eq!(report.would_fail, 0);
    assert!(!report.is_futile());
    assert!(
        report.total_losses().is_empty(),
        "{:?}",
        report.total_losses()
    );
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);

    // And an ignored column is covered-as-not-mapped rather than absent: an operator reviewing the crosswalk
    // needs to see that the decision was made, not that the column vanished.
    let notes = report
        .coverage
        .get("Internal Notes")
        .copied()
        .expect("notes");
    assert_eq!(notes.present, 3);
    assert_eq!(notes.mapped, 0);
    assert!(notes.ignored, "the decision is visible");
    assert!(
        !notes.is_total_loss(),
        "and it is not a finding — a report that listed deliberately-dropped columns among its losses \
         would defeat the point of being able to decide against a field, and the column an operator scans \
         has to be able to shrink to nothing",
    );

    // Adding a rule for it makes the report clean without touching the data.
    crosswalk
        .rules
        .push(rule("Internal Notes", "caption", Transform::Copy));
    crosswalk.ignored.clear();
    let source = record(&[("Brand", json!("acme")), ("Internal Notes", json!("x"))]);
    let mapped = crosswalk::apply(&crosswalk, &source, &defs);
    assert_eq!(mapped.payload["caption"], json!("x"));
}

#[test]
fn a_fatal_record_is_counted_apart_from_an_invalid_one() {
    // Different failures wanting different fixes: a fatal one is a mapping table missing an entry, an invalid
    // one is a field the source never had. Collapsing them would send somebody to the wrong screen.
    let mut table = BTreeMap::new();
    table.insert("RF".to_owned(), "royalty_free".to_owned());
    let crosswalk = Crosswalk {
        rules: vec![
            rule("Brand", "brand", Transform::Copy),
            rule(
                "Licence",
                "caption",
                Transform::Map {
                    table,
                    on_miss: OnMiss::Fail,
                },
            ),
        ],
        ignored: vec![],
    };
    let defs = defs();
    let mut report = Report::default();

    for licence in ["RF", "RM", "RF"] {
        let source = record(&[("Brand", json!("acme")), ("Licence", json!(licence))]);
        let mapped = crosswalk::apply(&crosswalk, &source, &defs);
        let valid = dam_core::fields::validate(
            &defs,
            &mapped.payload,
            Mode::Create,
            Writer::Human,
            &Map::new(),
        )
        .is_ok();
        crosswalk::accrue(&mut report, &crosswalk, &source, &mapped, valid);
    }

    assert_eq!(report.records, 3);
    assert_eq!(report.would_arrive, 2);
    assert_eq!(report.would_fail, 1, "the unmappable licence");
    assert_eq!(report.would_be_invalid, 0);
    assert_eq!(report.warnings.get(code::UNMAPPED_VALUE).copied(), Some(1));
}
