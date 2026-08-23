//! Mapping a source library's metadata onto this one's (G7).
//!
//! ## Why this is the part that decides whether a migration succeeds
//!
//! GAPS §G7 says it plainly: "underestimating metadata cleanup is the single most common cause of failed DAM
//! migrations." Moving bytes is a solved problem — a loop over an API and a `PUT`. What fails is the mapping: a
//! source field nobody noticed, a taxonomy term with no equivalent, a date in a format that silently parsed as
//! something else. So the crosswalk is a first-class artifact with its own review phase, and the dry-run report
//! it produces is the thing a customer signs off on.
//!
//! ## The dry run uses the real validator
//!
//! [`apply`] produces a payload and nothing more; whether that payload is *acceptable* is
//! [`crate::fields::validate`]'s answer, and the dry run asks it. A dry run with its own idea of validity would
//! certify something different from what the transfer does, which is worse than no dry run at all — it would
//! be a signed-off report followed by a failed run.
//!
//! ## An empty source field is not a loss
//!
//! A CSV header lists every column; most rows leave most of them blank. Reporting each blank as an unmapped
//! field would bury the twelve that matter under forty thousand that do not, and a report nobody reads is a
//! report that certifies nothing. So a warning is raised only where a value actually existed and did not
//! arrive.
//!
//! ## Every loss is named, and losing quietly is the one thing forbidden
//!
//! A transform that cannot do its job — an unparseable date, a value with no entry in a mapping table, several
//! values going into a single-valued field — records a [`Warning`] and drops the value rather than guessing.
//! Guessing is how a library arrives with plausible wrong dates that nobody notices for two years.
//!
//! `on_miss` on a mapping table is the one place the caller chooses: keep the source value, drop it, or fail
//! the record. All three are legitimate — a rights vocabulary wants failure, a free-text keyword list wants the
//! source value — and defaulting would silently pick one.

use crate::fields::FieldDef;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// What to do with a value a mapping table does not cover.
///
/// Serialised because a crosswalk is stored as `jsonb` and reviewed as a file — one definition rather than a
/// parallel document type, so a rule that round-trips through the database cannot mean something different from
/// the one in the file somebody reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnMiss {
    /// Pass the source value through unchanged. Right for an open vocabulary — a keyword nobody has seen is
    /// still a keyword.
    Keep,
    /// Drop it, with a warning. Right for a closed list where an unknown value is noise.
    Drop,
    /// Fail the whole record. Right for anything a rights decision rests on: an unmappable licence value must
    /// stop the asset arriving rather than arrive without it.
    Fail,
}

/// How one source value becomes a target value.
///
/// Tagged by a `type` field: `{"type":"copy"}`, `{"type":"split","on":";"}`.
///
/// Internally tagged rather than externally, and writing a real crosswalk file is what settled it. Externally
/// tagged, a transform with no parameters spells `"copy"` while one with parameters spells
/// `{"split":{"on":";"}}` — so a hand-written file needs its author to know which transforms happen to take
/// arguments, and `{"copy":{}}` fails with "invalid type: map, expected unit". One uniform shape, and a missing
/// or unknown `type` is a parse error rather than a silently ignored rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Transform {
    /// Straight across, with surrounding whitespace removed.
    ///
    /// Trimming is not a separate transform because there is no case for keeping it: a leading space in a CSV
    /// cell is an artifact of the export, and a value that differs from its neighbour only by whitespace
    /// becomes a second facet bucket nobody wanted.
    Copy,
    /// One value into many, on a delimiter. For a source that packs a multi-value field into one cell.
    Split { on: String },
    /// Many values into one. For a source that had several where this library has one.
    Join { with: String },
    /// Parse a date in a named format and emit ISO-8601.
    ///
    /// The format is required rather than guessed. `03/04/2026` is two different dates depending on the
    /// source's locale, and a guesser that got it right in testing will get it wrong on a customer's data.
    Date { format: String },
    /// Translate values through a table — a taxonomy reconciliation, a rights vocabulary.
    Map {
        table: BTreeMap<String, String>,
        on_miss: OnMiss,
    },
    /// Ignore the source and write a fixed value. For a field the source did not have and the migration
    /// decides — "imported from Widen", a default rights state.
    Constant { value: Value },
}

/// One source field's destination.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    /// The source's own field name, exactly as discovery found it.
    pub source: String,
    /// The `field_definitions.key` it lands in.
    pub target: String,
    pub transform: Transform,
}

/// The whole mapping, plus what discovery found that it does not cover.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Crosswalk {
    pub rules: Vec<Rule>,
    /// Source fields deliberately not mapped.
    ///
    /// Explicit, because "not mapped yet" and "decided against" are different states and only one of them is a
    /// finding. A field listed here stops appearing in the report's unmapped column, which is what lets the
    /// column shrink to nothing and mean something.
    pub ignored: Vec<String>,
}

/// Why one value did not arrive intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// The source field it came from.
    pub source: String,
    /// A stable code, so a report can group forty thousand of these into six rows.
    pub code: &'static str,
    pub detail: String,
}

/// What a record maps to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mapped {
    /// The payload to hand to [`crate::fields::validate`].
    pub payload: Map<String, Value>,
    /// Non-fatal losses. Aggregated into the dry-run report so the crosswalk can be fixed before the real run.
    pub warnings: Vec<Warning>,
    /// Set when a transform said the record must not arrive at all — see [`OnMiss::Fail`].
    pub fatal: Option<Warning>,
}

/// Codes a report groups by.
pub mod code {
    /// A source field carried a value and no rule mentioned it.
    pub const UNMAPPED: &str = "unmapped_field";
    /// A mapping table had no entry for the value.
    pub const UNMAPPED_VALUE: &str = "unmapped_value";
    /// A date did not match its declared format.
    pub const BAD_DATE: &str = "unparseable_date";
    /// Several values were produced for a field that holds one.
    pub const TOO_MANY_VALUES: &str = "single_valued_target";
    /// A rule names a field this library does not have.
    pub const UNKNOWN_TARGET: &str = "unknown_target_field";
    /// A rule names a read-only field, which no writer may set.
    pub const READ_ONLY_TARGET: &str = "read_only_target_field";
}

/// Maps one source record.
///
/// `defs` is consulted for two things only: whether a target exists, and whether it is multivalued. Everything
/// else about validity — types, constraints, required-ness — is [`crate::fields::validate`]'s job, and asking
/// it twice in two ways is how a dry run comes to disagree with a transfer.
#[must_use]
pub fn apply(crosswalk: &Crosswalk, record: &Map<String, Value>, defs: &[FieldDef]) -> Mapped {
    let mut out = Mapped::default();

    for rule in &crosswalk.rules {
        let Some(def) = defs.iter().find(|def| def.key == rule.target) else {
            // A rule pointing at nothing is a crosswalk error, not a data error — and it is worth reporting
            // once per record rather than being silently skipped, because a mistyped target key would
            // otherwise present as a field that mysteriously never populates.
            out.warnings.push(Warning {
                source: rule.source.clone(),
                code: code::UNKNOWN_TARGET,
                detail: format!("no field named {:?} in this library", rule.target),
            });
            continue;
        };
        if def.read_only {
            out.warnings.push(Warning {
                source: rule.source.clone(),
                code: code::READ_ONLY_TARGET,
                detail: format!(
                    "{:?} is maintained by the system; nothing may import into it",
                    rule.target
                ),
            });
            continue;
        }

        // A constant ignores the source entirely, which is the point: it fills a field the source did not have.
        if let Transform::Constant { value } = &rule.transform {
            out.payload.insert(rule.target.clone(), value.clone());
            continue;
        }

        let Some(raw) = record.get(&rule.source) else {
            continue;
        };
        if is_blank(raw) {
            // Nothing to lose and nothing to report. See the module docs on why an empty cell is not a finding.
            continue;
        }

        match transform(rule, raw, def) {
            Outcome::Value(value) => {
                out.payload.insert(rule.target.clone(), value);
            }
            Outcome::Dropped(warning) => out.warnings.push(warning),
            Outcome::Fatal(warning) => {
                out.fatal = Some(warning);
                return out;
            }
        }
    }

    // Source fields that carried something and went nowhere. The report's most important column: this is the
    // metadata cleanup §G7 says migrations underestimate.
    for (name, value) in record {
        if is_blank(value)
            || crosswalk.ignored.iter().any(|one| one == name)
            || crosswalk.rules.iter().any(|rule| &rule.source == name)
        {
            continue;
        }
        out.warnings.push(Warning {
            source: name.clone(),
            code: code::UNMAPPED,
            detail: "carried a value and no rule mentions it".to_owned(),
        });
    }

    out
}

enum Outcome {
    Value(Value),
    Dropped(Warning),
    Fatal(Warning),
}

fn transform(rule: &Rule, raw: &Value, def: &FieldDef) -> Outcome {
    let warn = |code: &'static str, detail: String| Warning {
        source: rule.source.clone(),
        code,
        detail,
    };

    match &rule.transform {
        // Handled by the caller; a constant never reaches here.
        Transform::Constant { value } => Outcome::Value(value.clone()),

        Transform::Copy => Outcome::Value(one_or_many(trimmed(raw), def)),

        Transform::Split { on } => {
            let text = as_text(raw);
            let parts: Vec<Value> = text
                .split(on.as_str())
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(|part| Value::String(part.to_owned()))
                .collect();
            if !def.multivalued && parts.len() > 1 {
                // Dropped rather than joined back together or truncated to the first. Both of those are
                // guesses, and the point of the report is that somebody decides.
                return Outcome::Dropped(warn(
                    code::TOO_MANY_VALUES,
                    format!(
                        "{} produced {} values and {:?} holds one",
                        rule.source,
                        parts.len(),
                        rule.target
                    ),
                ));
            }
            if def.multivalued {
                Outcome::Value(Value::Array(parts))
            } else {
                Outcome::Value(parts.into_iter().next().unwrap_or(Value::Null))
            }
        }

        Transform::Join { with } => {
            let joined = match raw {
                Value::Array(items) => items
                    .iter()
                    .map(as_text)
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(with),
                other => as_text(other),
            };
            Outcome::Value(Value::String(joined))
        }

        Transform::Date { format } => {
            let text = as_text(raw);
            match chrono::NaiveDate::parse_from_str(text.trim(), format) {
                Ok(date) => Outcome::Value(Value::String(date.to_string())),
                // Dropped, never guessed. `03/04/2026` is two different dates depending on the source's
                // locale, and a plausible wrong date is worse than a missing one because nobody notices it.
                Err(_) => Outcome::Dropped(warn(
                    code::BAD_DATE,
                    format!("{text:?} does not match the format {format:?}"),
                )),
            }
        }

        Transform::Map { table, on_miss } => {
            let apply_one = |text: &str| table.get(text.trim()).cloned();
            match raw {
                Value::Array(items) => {
                    let mut mapped = Vec::with_capacity(items.len());
                    for item in items {
                        let text = as_text(item);
                        match apply_one(&text) {
                            Some(value) => mapped.push(Value::String(value)),
                            None => match on_miss {
                                OnMiss::Keep => mapped.push(Value::String(text)),
                                OnMiss::Drop => {}
                                OnMiss::Fail => {
                                    return Outcome::Fatal(warn(
                                        code::UNMAPPED_VALUE,
                                        format!(
                                            "{text:?} has no entry in the mapping for this field"
                                        ),
                                    ));
                                }
                            },
                        }
                    }
                    if def.multivalued {
                        Outcome::Value(Value::Array(mapped))
                    } else if mapped.len() > 1 {
                        Outcome::Dropped(warn(
                            code::TOO_MANY_VALUES,
                            format!("{:?} holds one value", rule.target),
                        ))
                    } else {
                        Outcome::Value(mapped.into_iter().next().unwrap_or(Value::Null))
                    }
                }
                other => {
                    let text = as_text(other);
                    match apply_one(&text) {
                        Some(value) => Outcome::Value(one_or_many(Value::String(value), def)),
                        None => match on_miss {
                            OnMiss::Keep => Outcome::Value(one_or_many(Value::String(text), def)),
                            OnMiss::Drop => Outcome::Dropped(warn(
                                code::UNMAPPED_VALUE,
                                format!("{text:?} has no entry in the mapping for this field"),
                            )),
                            OnMiss::Fail => Outcome::Fatal(warn(
                                code::UNMAPPED_VALUE,
                                format!("{text:?} has no entry in the mapping for this field"),
                            )),
                        },
                    }
                }
            }
        }
    }
}

/// Wraps a single value for a multivalued target.
///
/// A source with one keyword still lands in an array, because the field holds an array and a bare string there
/// is a type error `validate` would refuse — turning a successful mapping into a failed record for a reason
/// nobody could see from the crosswalk.
fn one_or_many(value: Value, def: &FieldDef) -> Value {
    if def.multivalued {
        Value::Array(vec![value])
    } else {
        value
    }
}

fn trimmed(raw: &Value) -> Value {
    match raw {
        Value::String(text) => Value::String(text.trim().to_owned()),
        other => other.clone(),
    }
}

fn as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Whether a source value carries nothing.
///
/// A blank string counts, because a CSV cell that is empty and one that is absent mean the same thing to the
/// person who exported it — and treating them differently would put half a report's rows in one column and
/// half in another for no reason anybody could explain.
fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty(),
        Value::Array(items) => items.iter().all(is_blank),
        _ => false,
    }
}

/// What a dry run found, aggregated across every record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub records: u64,
    /// Records that would arrive with everything the library requires.
    pub would_arrive: u64,
    /// Records a transform refused outright — see [`OnMiss::Fail`].
    pub would_fail: u64,
    /// Records that would be refused by validation, with the reasons grouped.
    pub would_be_invalid: u64,
    /// Per source field: how many records carried a value, and how many of those arrived.
    pub coverage: BTreeMap<String, Coverage>,
    /// Warning counts by code, so forty thousand losses read as six rows.
    pub warnings: BTreeMap<String, u64>,
    /// Validation rejection counts by code, same reasoning.
    pub rejections: BTreeMap<String, u64>,
}

/// How well one source field is carried across.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Records where the source had a value.
    pub present: u64,
    /// Of those, how many landed somewhere.
    pub mapped: u64,
    /// Whether the crosswalk deliberately drops this field.
    ///
    /// Recorded rather than omitted, because an operator reviewing a crosswalk needs to see that the decision
    /// was *made* — a column that simply vanished from the report is indistinguishable from one nobody noticed.
    /// But it is not a finding: see [`Coverage::is_total_loss`].
    pub ignored: bool,
}

impl Coverage {
    /// Nothing arrived, something was there to arrive, and nobody decided that was fine.
    ///
    /// The `ignored` clause is the interesting one, and it came out of writing the assertion the other way: a
    /// report that listed a deliberately-dropped column among its losses would defeat the point of being able
    /// to decide against a field at all. The column an operator scans has to shrink to nothing as the crosswalk
    /// is finished, or they stop scanning it.
    #[must_use]
    pub const fn is_total_loss(&self) -> bool {
        !self.ignored && self.present > 0 && self.mapped == 0
    }
}

/// Folds one record's outcome into a report.
///
/// Takes the *validation* result rather than computing it, so the caller passes what
/// [`crate::fields::validate`] actually said. That is the whole design point restated as a signature: a report
/// that judged validity itself would certify something different from what the transfer does.
pub fn accrue(
    report: &mut Report,
    crosswalk: &Crosswalk,
    record: &Map<String, Value>,
    mapped: &Mapped,
    valid: bool,
) {
    report.records += 1;

    // Coverage first, and per *source* field rather than per target: the question an operator has is "did my
    // `Keywords` column arrive", and several source fields can land in one target.
    for (name, value) in record {
        if is_blank(value) {
            continue;
        }
        let entry = report.coverage.entry(name.clone()).or_default();
        entry.present += 1;
        entry.ignored = crosswalk.ignored.iter().any(|one| one == name);
        let arrived = crosswalk
            .rules
            .iter()
            .filter(|rule| &rule.source == name)
            .any(|rule| mapped.payload.contains_key(&rule.target));
        if arrived {
            entry.mapped += 1;
        }
    }

    for warning in &mapped.warnings {
        *report.warnings.entry(warning.code.to_owned()).or_default() += 1;
    }

    if let Some(fatal) = &mapped.fatal {
        report.would_fail += 1;
        *report.warnings.entry(fatal.code.to_owned()).or_default() += 1;
        return;
    }
    if valid {
        report.would_arrive += 1;
    } else {
        report.would_be_invalid += 1;
    }
}

/// Folds validation rejections into a report, grouped by code.
///
/// Separate from [`accrue`] so a caller that does not run validation — a discovery pass that only wants
/// coverage — is not forced to invent an answer about validity.
pub fn accrue_rejections(report: &mut Report, rejections: &[crate::fields::Rejection]) {
    for rejection in rejections {
        *report
            .rejections
            .entry(rejection.code.to_owned())
            .or_default() += 1;
    }
}

impl Report {
    /// Source fields that carried values and landed nothing, worst first.
    ///
    /// The line an operator scans for, and the reason coverage is per source field: "your `Photographer` column
    /// is 40,000 records and none of them arrive" is the finding that stops a migration being signed off.
    #[must_use]
    pub fn total_losses(&self) -> Vec<(&str, Coverage)> {
        let mut losses: Vec<(&str, Coverage)> = self
            .coverage
            .iter()
            .filter(|(_, coverage)| coverage.is_total_loss())
            .map(|(name, coverage)| (name.as_str(), *coverage))
            .collect();
        losses.sort_by(|a, b| b.1.present.cmp(&a.1.present).then(a.0.cmp(b.0)));
        losses
    }

    /// Whether this run is worth doing.
    ///
    /// Deliberately not a percentage threshold. A migration where a tenth of assets arrive invalid may be fine
    /// (the field was optional in the source and will be filled in later) or a disaster (it is the rights
    /// field), and no number here can tell those apart. What this answers is the one unambiguous case: nothing
    /// would arrive at all.
    #[must_use]
    pub const fn is_futile(&self) -> bool {
        self.records > 0 && self.would_arrive == 0
    }
}
