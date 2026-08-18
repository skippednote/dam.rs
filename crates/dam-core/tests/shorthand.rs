//! The shorthand search syntax (2.5).
//!
//! What a person types into one box. It parses to the same [`Query`](dam_core::query::Query) the API
//! accepts, so there is no second query language and no second set of semantics to keep in step.
//!
//! The test TASKS.md names is first, and it is about error reporting rather than parsing: **an unclosed
//! quote is a parse error with a column, not a silent whole-string match.** The silent version is the one
//! every hand-written search box ships with — `"beach holiday` becomes a search for the literal text
//! `"beach holiday`, which returns nothing, and the user has no way to know the quote was the problem.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::fields::{Constraints, FieldDef, FieldKind};
use dam_core::query::{Comparison, Endpoint, Literal, Query};
use dam_core::shorthand;
use uuid::Uuid;

fn def(key: &str, kind: FieldKind, alias: Option<&str>) -> FieldDef {
    let mut def = FieldDef {
        key: key.to_owned(),
        kind,
        taxonomy_id: None,
        multivalued: false,
        required: false,
        read_only: false,
        ai_writable: false,
        constraints: Constraints::default(),
    };
    if kind == FieldKind::TaxonomyRef {
        def.taxonomy_id = Some(Uuid::from_u128(1));
    }
    let _ = alias;
    def
}

/// The tenant's schema, with the aliases the shorthand resolves.
fn schema() -> shorthand::Schema {
    shorthand::Schema::new(
        fields(),
        [("bra", "brand"), ("yr", "year")]
            .into_iter()
            .map(|(alias, key)| (alias.to_owned(), key.to_owned()))
            .collect(),
    )
}

fn parse(input: &str) -> Query {
    shorthand::parse(input, &schema()).unwrap_or_else(|e| panic!("parsing {input:?}: {e}"))
}

fn parse_err(input: &str) -> shorthand::ParseError {
    shorthand::parse(input, &schema()).expect_err(&format!("{input:?} should not parse"))
}

fn field(key: &str, op: Comparison) -> Query {
    Query::Field {
        key: key.to_owned(),
        op,
    }
}

// ─── the named test ─────────────────────────────────────────────────────────

#[test]
fn an_unclosed_quote_is_a_parse_error_with_a_column() {
    // The failure every hand-written search box ships with: `"beach holiday` becomes a search for the
    // literal text `"beach holiday`, returns nothing, and the user cannot tell why. A column is what
    // makes the message actionable — a UI can underline the offending character.
    let error = parse_err("\"beach holiday");
    assert_eq!(error.code, "unclosed_quote");
    assert_eq!(
        error.column, 1,
        "the column must point at the opening quote, which is the character to fix"
    );
    assert!(
        error.to_string().contains("quote"),
        "the message must name the problem: {error}"
    );
}

#[test]
fn an_unclosed_quote_later_in_the_input_reports_its_own_column() {
    // One column for every input would be as useless as none. This also pins that columns are
    // 1-based and counted in characters.
    let error = parse_err("beach \"holiday");
    assert_eq!(error.code, "unclosed_quote");
    assert_eq!(error.column, 7);
}

#[test]
fn a_column_counts_characters_not_bytes() {
    // A multi-byte prefix must not shift the column. Underlining byte 9 of a string whose first token is
    // "café" points at the wrong character, and the user sees a caret under the wrong thing.
    let error = parse_err("café \"holiday");
    assert_eq!(
        error.column, 6,
        "five characters plus a space, so the quote is the sixth character"
    );
}

// ─── bare terms and phrases ─────────────────────────────────────────────────

#[test]
fn a_bare_word_is_a_free_text_search() {
    assert_eq!(parse("beach"), Query::Text("beach".to_owned()));
}

#[test]
fn several_bare_words_are_conjoined_not_concatenated() {
    // `beach holiday` means both, not the phrase. Concatenating into one `Text` would make the shorthand
    // silently phrase-search, which is what quotes are for — and would return far less than the user
    // expects.
    assert_eq!(
        parse("beach holiday"),
        Query::And(vec![
            Query::Text("beach".to_owned()),
            Query::Text("holiday".to_owned()),
        ])
    );
}

#[test]
fn a_quoted_phrase_stays_one_term() {
    assert_eq!(
        parse("\"beach holiday\""),
        Query::Text("beach holiday".to_owned())
    );
}

#[test]
fn a_quoted_phrase_keeps_characters_that_would_otherwise_be_syntax() {
    // Inside quotes, `-`, `:` and `OR` are just text. Otherwise a user cannot search for "sold-out" or
    // "9:30" at all, and the shorthand becomes a trap rather than a convenience.
    assert_eq!(
        parse("\"sold-out OR 9:30\""),
        Query::Text("sold-out OR 9:30".to_owned())
    );
}

#[test]
fn an_empty_query_matches_everything_the_caller_may_see() {
    // Not an error. An empty search box is the default state of a search page, and the answer is the
    // library — access-filtered, as everything is.
    assert_eq!(parse(""), Query::All);
    assert_eq!(parse("   "), Query::All);
}

// ─── field queries ──────────────────────────────────────────────────────────

#[test]
fn a_field_key_and_an_alias_resolve_to_the_same_query() {
    // Aliases exist so `bra:acme` works; they must not become a second way for the meaning to drift.
    let expected = field(
        "brand",
        Comparison::Equals(Literal::Text("acme".to_owned())),
    );
    assert_eq!(parse("brand:acme"), expected);
    assert_eq!(parse("bra:acme"), expected);
}

#[test]
fn a_field_value_is_typed_from_its_field_kind() {
    // The parser cannot know `2026` is a number without the schema, and guessing would make `brand:2026`
    // an integer comparison against a text column.
    assert_eq!(
        parse("year:2026"),
        field("year", Comparison::Equals(Literal::Int(2026)))
    );
    assert_eq!(
        parse("brand:2026"),
        field(
            "brand",
            Comparison::Equals(Literal::Text("2026".to_owned()))
        )
    );
    assert_eq!(
        parse("live:true"),
        field("live", Comparison::Equals(Literal::Bool(true)))
    );
}

#[test]
fn a_value_that_does_not_fit_its_field_is_a_parse_error_with_a_column() {
    let error = parse_err("year:recently");
    assert_eq!(error.code, "bad_value");
    assert_eq!(
        error.column, 6,
        "the column points at the value, not the key"
    );
}

#[test]
fn a_quoted_field_value_may_contain_spaces() {
    assert_eq!(
        parse("brand:\"Acme Corp\""),
        field(
            "brand",
            Comparison::Equals(Literal::Text("Acme Corp".to_owned()))
        )
    );
}

#[test]
fn an_unknown_field_is_an_error_rather_than_a_text_search() {
    // A typo'd key silently becoming free text returns nothing and tells the user nothing. Erroring
    // names the key, which is the one piece of information that helps.
    let error = parse_err("brnad:acme");
    assert_eq!(error.code, "unknown_field");
    assert_eq!(error.column, 1);
    assert!(error.to_string().contains("brnad"), "{error}");
}

#[test]
fn a_url_is_text_rather_than_a_field_query() {
    // `https://example.com` parses as key `https` under the naive rule. Anything containing `://` is
    // treated as text, because a user pasting a URL into a search box is far more common than a field
    // named after a URL scheme.
    assert_eq!(
        parse("https://example.com/a"),
        Query::Text("https://example.com/a".to_owned())
    );
}

#[test]
fn a_key_that_is_not_field_shaped_is_text() {
    // `9:30` and `note:` with punctuation are not field references, so they must not be errors. The
    // shape is the same one `field_defs_key_shape` enforces.
    assert_eq!(parse("9:30"), Query::Text("9:30".to_owned()));
    assert_eq!(
        parse("Ratio:16"),
        Query::Text("Ratio:16".to_owned()),
        "an uppercase key is not the field-key shape"
    );
}

// ─── presence ───────────────────────────────────────────────────────────────

#[test]
fn a_field_with_a_star_asks_for_presence_and_a_bare_dash_for_absence() {
    assert_eq!(parse("brand:*"), field("brand", Comparison::Exists));
    assert_eq!(parse("brand:-"), field("brand", Comparison::Missing));
}

// ─── ranges ─────────────────────────────────────────────────────────────────

#[test]
fn a_double_dot_range_is_inclusive_at_both_ends() {
    assert_eq!(
        parse("year:2020..2026"),
        field(
            "year",
            Comparison::Range {
                lower: Endpoint::Inclusive(Literal::Int(2020)),
                upper: Endpoint::Inclusive(Literal::Int(2026)),
            }
        )
    );
}

#[test]
fn a_half_open_range_leaves_the_other_end_unbounded() {
    assert_eq!(
        parse("year:2020.."),
        field(
            "year",
            Comparison::Range {
                lower: Endpoint::Inclusive(Literal::Int(2020)),
                upper: Endpoint::Unbounded,
            }
        )
    );
    assert_eq!(
        parse("year:..2026"),
        field(
            "year",
            Comparison::Range {
                lower: Endpoint::Unbounded,
                upper: Endpoint::Inclusive(Literal::Int(2026)),
            }
        )
    );
}

#[test]
fn comparison_operators_produce_the_matching_exclusivity() {
    // `>` exclusive and `>=` inclusive. Getting these the same way round as the symbols is the whole
    // reason they exist, and an off-by-one here is a filter that silently includes a boundary row.
    assert_eq!(
        parse("year:>2020"),
        field(
            "year",
            Comparison::Range {
                lower: Endpoint::Exclusive(Literal::Int(2020)),
                upper: Endpoint::Unbounded,
            }
        )
    );
    assert_eq!(
        parse("year:>=2020"),
        field(
            "year",
            Comparison::Range {
                lower: Endpoint::Inclusive(Literal::Int(2020)),
                upper: Endpoint::Unbounded,
            }
        )
    );
    assert_eq!(
        parse("year:<2026"),
        field(
            "year",
            Comparison::Range {
                lower: Endpoint::Unbounded,
                upper: Endpoint::Exclusive(Literal::Int(2026)),
            }
        )
    );
    assert_eq!(
        parse("year:<=2026"),
        field(
            "year",
            Comparison::Range {
                lower: Endpoint::Unbounded,
                upper: Endpoint::Inclusive(Literal::Int(2026)),
            }
        )
    );
}

#[test]
fn a_date_range_parses_dates_not_numbers() {
    assert_eq!(
        parse("shot_on:2026-01-01..2026-12-31"),
        field(
            "shot_on",
            Comparison::Range {
                lower: Endpoint::Inclusive(Literal::Date(
                    chrono::NaiveDate::from_ymd_opt(2026, 1, 1).expect("date")
                )),
                upper: Endpoint::Inclusive(Literal::Date(
                    chrono::NaiveDate::from_ymd_opt(2026, 12, 31).expect("date")
                )),
            }
        )
    );
}

#[test]
fn a_range_on_a_text_field_is_a_parse_error() {
    // Caught here rather than by the IR's validation, so the message can carry a column. Both refuse it;
    // this one refuses it usefully.
    let error = parse_err("brand:a..z");
    assert_eq!(error.code, "not_orderable");
}

#[test]
fn a_range_with_neither_bound_is_a_parse_error() {
    let error = parse_err("year:..");
    assert_eq!(error.code, "empty_range");
}

// ─── negation and boolean structure ─────────────────────────────────────────

#[test]
fn a_leading_dash_negates_the_term_it_is_attached_to() {
    assert_eq!(
        parse("-brand:acme"),
        Query::Not(Box::new(field(
            "brand",
            Comparison::Equals(Literal::Text("acme".to_owned()))
        )))
    );
    assert_eq!(
        parse("-beach"),
        Query::Not(Box::new(Query::Text("beach".to_owned())))
    );
}

#[test]
fn a_dash_inside_a_word_is_not_a_negation() {
    // `sold-out` is a word. Treating the internal dash as an operator would make hyphenated terms
    // unsearchable, which is a large fraction of real product names.
    assert_eq!(parse("sold-out"), Query::Text("sold-out".to_owned()));
}

#[test]
fn uppercase_or_is_an_operator_and_lowercase_or_is_a_word() {
    // Case-sensitivity is the cheapest way to keep "or" searchable. A user typing `cats or dogs`
    // overwhelmingly means the word.
    assert_eq!(
        parse("beach OR mountain"),
        Query::Or(vec![
            Query::Text("beach".to_owned()),
            Query::Text("mountain".to_owned()),
        ])
    );
    assert_eq!(
        parse("beach or mountain"),
        Query::And(vec![
            Query::Text("beach".to_owned()),
            Query::Text("or".to_owned()),
            Query::Text("mountain".to_owned()),
        ])
    );
}

#[test]
fn and_binds_tighter_than_or() {
    // The standard precedence. Getting it backwards turns `a b OR c` into "a AND (b OR c)", which
    // returns a different set and would be very hard to notice.
    assert_eq!(
        parse("a b OR c"),
        Query::Or(vec![
            Query::And(vec![
                Query::Text("a".to_owned()),
                Query::Text("b".to_owned())
            ]),
            Query::Text("c".to_owned()),
        ])
    );
}

#[test]
fn parentheses_override_precedence() {
    assert_eq!(
        parse("a (b OR c)"),
        Query::And(vec![
            Query::Text("a".to_owned()),
            Query::Or(vec![
                Query::Text("b".to_owned()),
                Query::Text("c".to_owned())
            ]),
        ])
    );
}

#[test]
fn an_unclosed_parenthesis_is_an_error_with_a_column() {
    let error = parse_err("a (b OR c");
    assert_eq!(error.code, "unclosed_group");
    assert_eq!(error.column, 3);
}

#[test]
fn an_unexpected_closing_parenthesis_is_an_error_with_a_column() {
    let error = parse_err("a) b");
    assert_eq!(error.code, "unexpected_token");
    assert_eq!(error.column, 2);
}

#[test]
fn a_trailing_operator_is_an_error_rather_than_being_ignored() {
    // `beach OR` silently becoming `beach` looks like it worked and quietly answers a different
    // question.
    let error = parse_err("beach OR");
    assert_eq!(error.code, "unexpected_end");
}

#[test]
fn an_empty_group_is_an_error() {
    let error = parse_err("a ()");
    assert_eq!(error.code, "unexpected_token");
}

// ─── bounds ─────────────────────────────────────────────────────────────────

#[test]
fn an_absurdly_long_input_is_refused_before_it_is_parsed() {
    // A search string is untrusted input from a URL. Bounding the length here is cheaper than bounding
    // the tree afterwards, and it makes the node limit in the IR unreachable from this entry point.
    let error = parse_err(&"beach ".repeat(20_000));
    assert_eq!(error.code, "too_long");
}

#[test]
fn deeply_nested_parentheses_are_refused_with_a_column() {
    // The parser recurses, so this must be refused rather than overflowing the stack.
    let input = format!("{}a{}", "(".repeat(200), ")".repeat(200));
    let error = parse_err(&input);
    assert_eq!(error.code, "too_deep");
}

// ─── the parsed query goes through the same validation as any other ─────────

#[test]
fn a_parsed_query_still_has_to_pass_the_irs_validation() {
    // The parser and the IR check overlapping things, and that is deliberate rather than redundant: the
    // parser catches what it can with a column, and `Planned` is the gate nothing reaches the renderers
    // without. A shorthand query that skipped validation would be a second path into the query layer with
    // its own rules, which is exactly what having one IR is meant to prevent.
    let query = shorthand::parse("bra:acme year:>2020", &schema()).expect("parses");

    let access = dam_core::policy::compile(
        &dam_core::policy::Grants::from(vec![dam_core::policy::Grant {
            permissions: vec!["asset:read".to_owned()],
            asset_group_ids: vec![],
            all_asset_groups: true,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        }]),
        dam_core::policy::Action::Read,
        chrono::Utc::now(),
    );

    // Against the schema it was parsed with, it validates.
    let planned = dam_core::query::Planned::new(query.clone(), access.clone(), &fields())
        .expect("a parsed query must validate against the schema it was parsed with");
    assert_eq!(planned.query(), &query);

    // Against a *different* schema it does not, which is the property that matters: the parser resolved
    // an alias using one tenant's definitions, and validation is what stops that query being run
    // somewhere the field does not exist.
    let rejected = dam_core::query::Planned::new(query, access, &[])
        .expect_err("an empty schema must refuse it");
    assert_eq!(rejected[0].code, "unknown_field");
}

/// The same definitions `schema()` is built from.
fn fields() -> Vec<FieldDef> {
    vec![
        def("brand", FieldKind::Text, None),
        def("year", FieldKind::Int, None),
        def("price", FieldKind::Decimal, None),
        def("shot_on", FieldKind::Date, None),
        def("published_at", FieldKind::DateTime, None),
        def("live", FieldKind::Bool, None),
        def("category", FieldKind::TaxonomyRef, None),
        def("homepage", FieldKind::Url, None),
    ]
}
