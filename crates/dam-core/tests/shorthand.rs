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
use std::collections::HashMap;
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
        facetable: false,
        constraints: Constraints::default(),
    };
    if kind == FieldKind::TaxonomyRef {
        def.taxonomy_id = Some(Uuid::from_u128(1));
    }
    let _ = alias;
    def
}

/// The category term ids the `in:` selector resolves to.
const EXTERIOR: Uuid = Uuid::from_u128(0xE0);
const YELLOW: Uuid = Uuid::from_u128(0xE1);

/// The tenant's schema, with the aliases and category paths the shorthand resolves.
fn schema() -> shorthand::Schema {
    shorthand::Schema::new(
        fields(),
        [("bra", "brand"), ("yr", "year")]
            .into_iter()
            .map(|(alias, key)| (alias.to_owned(), key.to_owned()))
            .collect(),
    )
    .with_categories(
        [("exterior", EXTERIOR), ("exterior.yellow", YELLOW)]
            .into_iter()
            .map(|(path, id)| (path.to_owned(), id))
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

#[test]
fn in_filters_by_category_and_includes_everything_beneath_it() {
    // Categories live in the query string rather than a separate parameter, because the filter rail edits one
    // string and "copy this search" has to copy all of it.
    assert_eq!(
        parse("in:exterior.yellow"),
        Query::Term {
            term_id: YELLOW,
            // Always true: "in Exterior" colloquially means everything filed beneath it, which is what
            // clicking a branch in a browse tree means and why the paths are ltree.
            include_descendants: true,
        }
    );

    // A branch, and it composes with everything else the shorthand can express.
    assert_eq!(
        parse("in:exterior bra:acme"),
        Query::And(vec![
            Query::Term {
                term_id: EXTERIOR,
                include_descendants: true,
            },
            Query::Field {
                key: "brand".to_owned(),
                op: Comparison::Equals(Literal::Text("acme".to_owned())),
            },
        ])
    );

    // Negation works, which is what makes "everything except the interiors" expressible at all.
    assert_eq!(
        parse("-in:exterior"),
        Query::Not(Box::new(Query::Term {
            term_id: EXTERIOR,
            include_descendants: true,
        }))
    );

    // Case-folded like every other selector value: the rail's own links must keep working if a label's case
    // changes, and a user typing what they see on screen is not wrong.
    assert_eq!(
        parse("in:Exterior.Yellow"),
        Query::Term {
            term_id: YELLOW,
            include_descendants: true,
        }
    );
}

#[test]
fn an_unknown_category_is_refused_rather_than_searched_for_as_text() {
    // Same reasoning as an unknown field: treating `in:extrior` as free text returns nothing and explains
    // nothing, and the user concludes the library is empty rather than that they mistyped.
    let error = parse_err("in:extrior");
    assert_eq!(error.code, "unknown_category");
    // The column points at the *path*, not at `in`, because that is the part to fix.
    assert_eq!(error.column, 4, "underline the path: {error:?}");

    let error = parse_err("in:");
    assert_eq!(error.code, "empty_category");
}

#[test]
fn a_field_cannot_shadow_the_category_selector() {
    // `in` is reserved. A tenant defining a field called `in` would otherwise take over the browse tree, and
    // the rail's links would break for a reason nobody could see from either side.
    let with_in_field =
        shorthand::Schema::new(vec![def("in", FieldKind::Text, None)], HashMap::new())
            .with_categories(
                [("exterior", EXTERIOR)]
                    .into_iter()
                    .map(|(path, id)| (path.to_owned(), id))
                    .collect(),
            );
    assert_eq!(
        shorthand::parse("in:exterior", &with_in_field).expect("parses"),
        Query::Term {
            term_id: EXTERIOR,
            include_descendants: true,
        },
        "the category selector wins over a field of the same name"
    );
}

#[test]
fn a_quoted_in_is_text_like_every_other_quoted_selector() {
    // Quoting suppresses every operator meaning. Somebody searching for the literal string "in:exterior" —
    // in a filename, say — must be able to.
    assert_eq!(
        parse("\"in:exterior\""),
        Query::Text("in:exterior".to_owned())
    );
}

// ─── engagement selectors (Q.5b·2) ──────────────────────────────────────────

#[test]
fn is_filters_by_the_callers_own_engagement() {
    use dam_core::query::Personal;

    assert_eq!(parse("is:favourite"), Query::Mine(Personal::Favourite));
    assert_eq!(parse("is:watched"), Query::Mine(Personal::Watched));
    assert_eq!(parse("is:rated"), Query::Mine(Personal::Rated));

    // Both spellings, because the tenant is in Pune and the vendor is in Boston, and a search box that takes
    // only one of them is wrong for somebody every single day.
    assert_eq!(parse("is:favorite"), Query::Mine(Personal::Favourite));
    // And "watching" as well as "watched": the button says Watch, so both the state and the act read naturally.
    assert_eq!(parse("is:watching"), Query::Mine(Personal::Watched));

    // The *value* is case-insensitive; the selector name is not, and neither is a field key. `is_field_shaped`
    // gates on lowercase before any selector is considered, so `IS:` is free text — the same rule that makes
    // `Brand:Acme` a text search and `IN:exterior` one too. Asserted rather than assumed, because a reader
    // seeing `eq_ignore_ascii_case` on the selector name would otherwise expect the opposite.
    assert_eq!(parse("is:FAVOURITE"), Query::Mine(Personal::Favourite));
    assert_eq!(
        parse("IS:Favourite"),
        Query::Text("IS:Favourite".to_owned())
    );
    assert_eq!(parse("IN:exterior"), Query::Text("IN:exterior".to_owned()));

    // The query tree carries no identity at all. This is the property that makes a saved search shareable: with
    // an identity in the tree, a colleague opening the search would get the author's favourites.
    let tree = parse("is:favourite");
    let json = format!("{tree:?}");
    assert!(
        !json.contains('-'),
        "a personal clause must not carry a uuid: {json}"
    );
}

#[test]
fn an_unknown_personal_state_is_refused_with_its_column() {
    let error = parse_err("is:starred");
    assert_eq!(error.code, "unknown_personal");
    // Points at the value, because that is the part to fix.
    assert_eq!(error.column, 4, "{error:?}");
    assert!(error.detail.contains("favourite"), "{error:?}");
}

#[test]
fn stars_filters_by_the_assets_average_rating() {
    assert_eq!(
        parse("stars:>=4"),
        Query::Rating(Comparison::Range {
            lower: Endpoint::Inclusive(Literal::Int(4)),
            upper: Endpoint::Unbounded,
        })
    );
    assert_eq!(
        parse("stars:5"),
        Query::Rating(Comparison::Equals(Literal::Int(5)))
    );
    assert_eq!(
        parse("stars:2..4"),
        Query::Rating(Comparison::Range {
            lower: Endpoint::Inclusive(Literal::Int(2)),
            upper: Endpoint::Inclusive(Literal::Int(4)),
        })
    );
    // The same syntax a field uses, all the way down: `*` is "rated by anyone" and `-` is "unrated", which are
    // the two buckets a rail needs beside the stars and are not expressible any other way.
    assert_eq!(parse("stars:*"), Query::Rating(Comparison::Exists));
    assert_eq!(parse("stars:-"), Query::Rating(Comparison::Missing));
}

#[test]
fn status_orientation_and_has_are_the_builtin_facet_selectors() {
    use dam_core::query::Orientation;

    // Q.15. Each one is what its facet bucket composes into, which is the constraint that decided the
    // spellings: the rail writes the string the parser reads, so a bucket labelled `landscape` has to become
    // `orientation:landscape` and nothing else.
    assert_eq!(
        parse("status:archived"),
        Query::Status("archived".to_owned())
    );
    // The *value* is case-folded; the selector *name* is not, because `is_field_shaped` requires lower case
    // and an upper-case name is free text throughout this parser. `Status:Archived` is a phrase somebody
    // typed, not a filter, and that rule predates these three selectors.
    assert_eq!(
        parse("status:Archived"),
        Query::Status("archived".to_owned())
    );
    assert_eq!(
        parse("STATUS:Archived"),
        Query::Text("STATUS:Archived".to_owned())
    );
    // Not validated against the CHECK constraint: an unknown status matches nothing, which is the honest
    // answer, and the list of them belongs to a migration rather than to the parser.
    assert_eq!(parse("status:banana"), Query::Status("banana".to_owned()));

    assert_eq!(
        parse("orientation:landscape"),
        Query::Orientation(Orientation::Landscape)
    );
    assert_eq!(
        parse("orientation:PORTRAIT"),
        Query::Orientation(Orientation::Portrait)
    );
    assert_eq!(
        parse("has:ATTACHMENT"),
        Query::HasAttachment,
        "the value folds even though the name may not"
    );
    assert_eq!(
        parse("orientation:square"),
        Query::Orientation(Orientation::Square)
    );

    assert_eq!(parse("has:attachment"), Query::HasAttachment);
    // The plural too. Somebody typing this is describing a set of things, and refusing them over an `s` is
    // the kind of pedantry that makes a search box feel hostile.
    assert_eq!(parse("has:attachments"), Query::HasAttachment);
}

#[test]
fn the_builtin_selectors_name_what_they_accept() {
    // A refusal that says which values exist, pointing at the value rather than the selector: the selector is
    // not the part to fix.
    let orientation = parse_err("orientation:tall");
    assert_eq!(orientation.code, "unknown_orientation");
    assert_eq!(orientation.column, 13, "{orientation:?}");
    assert!(orientation.detail.contains("landscape"), "{orientation:?}");

    let presence = parse_err("has:comments");
    assert_eq!(presence.code, "unknown_presence");
    assert!(presence.detail.contains("attachment"), "{presence:?}");

    assert_eq!(parse_err("status:").code, "empty_status");
    // And quoted, they are text — the same rule the other selectors follow.
    assert_eq!(
        parse("\"status:archived\""),
        Query::Text("status:archived".to_owned())
    );
}

#[test]
fn a_field_cannot_shadow_the_engagement_selectors() {
    // Same reservation as `in:`, for the same reason: a tenant field called `is` or `stars` would take over the
    // selector and the rail's own links would stop working.
    let shadowing = shorthand::Schema::new(
        vec![
            def("is", FieldKind::Text, None),
            def("stars", FieldKind::Text, None),
            def("status", FieldKind::Text, None),
            def("orientation", FieldKind::Text, None),
            def("has", FieldKind::Text, None),
        ],
        HashMap::new(),
    );
    assert_eq!(
        shorthand::parse("is:favourite", &shadowing).expect("parses"),
        Query::Mine(dam_core::query::Personal::Favourite),
    );
    assert_eq!(
        shorthand::parse("stars:3", &shadowing).expect("parses"),
        Query::Rating(Comparison::Equals(Literal::Int(3))),
    );
    // And the three Q.15 selectors, for the same reason: `status` is a column with a CHECK behind it, so a
    // tenant field of that name would be redefining something that is not theirs.
    assert_eq!(
        shorthand::parse("status:active", &shadowing).expect("parses"),
        Query::Status("active".to_owned()),
    );
    assert_eq!(
        shorthand::parse("orientation:square", &shadowing).expect("parses"),
        Query::Orientation(dam_core::query::Orientation::Square),
    );
    assert_eq!(
        shorthand::parse("has:attachment", &shadowing).expect("parses"),
        Query::HasAttachment,
    );
}

#[test]
fn a_quoted_selector_is_text_and_an_empty_one_is_an_error() {
    assert_eq!(
        parse("\"is:favourite\""),
        Query::Text("is:favourite".to_owned())
    );
    assert_eq!(parse("\"stars:4\""), Query::Text("stars:4".to_owned()));
    assert_eq!(parse_err("stars:").code, "empty_rating");
    assert_eq!(parse_err("is:").code, "unknown_personal");
}

#[test]
fn a_rating_outside_the_scale_is_refused_by_the_ir() {
    // The parser accepts the number and the IR refuses the value, which is the same split every field uses:
    // syntax here, meaning there.
    let parsed = parse("stars:>=9");
    let rejections = parsed.validate(&fields()).expect_err("out of range");
    assert!(
        rejections.iter().any(|r| r.code == "stars_range"),
        "{rejections:?}"
    );

    // And an operator that makes no sense for a number. Checked here, in the crate that owns the validation:
    // the same assertion in dam-db's renderer suite proved nothing about dam-core, and mutation testing found
    // exactly that gap — the refusal could be deleted and only the *other* crate's tests noticed.
    let rejections = Query::Rating(Comparison::Contains("4".to_owned()))
        .validate(&fields())
        .expect_err("a substring of a number");
    assert!(
        rejections.iter().any(|r| r.code == "stars_operator"),
        "{rejections:?}"
    );
    let rejections = Query::Rating(Comparison::StartsWith("4".to_owned()))
        .validate(&fields())
        .expect_err("a prefix of a number");
    assert!(
        rejections.iter().any(|r| r.code == "stars_operator"),
        "{rejections:?}"
    );
}
