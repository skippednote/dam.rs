//! Metadata field definitions and payload validation (2.1).
//!
//! `field_defs` is how a tenant says what their metadata means; this decides whether a payload
//! conforms. It runs before anything is written, which makes it the last cheap place to refuse a bad
//! value — after this the value is in a `jsonb` column, in a Tantivy document, and in whatever a
//! connector has already pushed downstream.
//!
//! Pure on purpose. The one thing it cannot decide is whether a taxonomy term belongs to the taxonomy a
//! field points at, because that is a row in the database; so it validates the shape and returns the
//! references for the caller to resolve in **one** query. Doing it per value would turn a metadata write
//! into a query per term.
//!
//! ## Four decisions that matter more than the type checking
//!
//! **An unknown key is refused, never ignored.** Ignoring is the friendlier-looking option, and it
//! silently discards data the user believes they saved: `brnad: "Acme"` returns 200 and stores nothing.
//! They find out when the field is empty months later, and there is nothing left to recover.
//!
//! **Every rejection is collected.** Stopping at the first turns a twenty-field import into twenty round
//! trips, and the person fixing it never knows how close they are.
//!
//! **`required` applies on create, not on patch.** Enforcing it on a patch would demand the whole record
//! for every single-field edit, which makes every edit a read-modify-write with a lost-update race in
//! it.
//!
//! **`ai_writable` is a restriction on enrichment, not on the field.** A person may write anywhere; an
//! enrichment run may write only where the tenant has said so. Without that, one enrichment pass
//! overwrites a caption a person wrote, and the person has no way to protect it.

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

/// The `kind` column's vocabulary, matching `field_defs`' CHECK constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Textarea,
    LongText,
    Int,
    Decimal,
    Date,
    DateTime,
    Bool,
    Select,
    MultiSelect,
    TaxonomyRef,
    UserRef,
    Url,
    Geo,
}

impl FieldKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Textarea => "textarea",
            Self::LongText => "long_text",
            Self::Int => "int",
            Self::Decimal => "decimal",
            Self::Date => "date",
            Self::DateTime => "datetime",
            Self::Bool => "bool",
            Self::Select => "select",
            Self::MultiSelect => "multiselect",
            Self::TaxonomyRef => "taxonomy_ref",
            Self::UserRef => "user_ref",
            Self::Url => "url",
            Self::Geo => "geo",
        }
    }

    /// Parses the stored value.
    ///
    /// An unknown kind is an error rather than a default: defaulting to `text` would silently drop
    /// validation for a field whose migration added a kind this build does not know about, which is
    /// exactly when validation matters most.
    pub fn parse(raw: &str) -> Result<Self, crate::Error> {
        let kind = match raw {
            "text" => Self::Text,
            "textarea" => Self::Textarea,
            "long_text" => Self::LongText,
            "int" => Self::Int,
            "decimal" => Self::Decimal,
            "date" => Self::Date,
            "datetime" => Self::DateTime,
            "bool" => Self::Bool,
            "select" => Self::Select,
            "multiselect" => Self::MultiSelect,
            "taxonomy_ref" => Self::TaxonomyRef,
            "user_ref" => Self::UserRef,
            "url" => Self::Url,
            "geo" => Self::Geo,
            other => {
                return Err(crate::Error::Validation {
                    field: "field_defs.kind".into(),
                    reason: format!("unknown field kind {other:?}"),
                });
            }
        };
        Ok(kind)
    }

    /// Whether values of this kind are text for search purposes.
    ///
    /// Used by 2.6 to decide which fields contribute to the full-text field. Here rather than there so
    /// the answer cannot disagree between the validator and the indexer.
    pub fn is_textual(self) -> bool {
        matches!(
            self,
            Self::Text | Self::Textarea | Self::LongText | Self::Select | Self::MultiSelect
        )
    }
}

/// The `validation` jsonb, parsed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Constraints {
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub min_length: Option<usize>,
    pub max_length: Option<usize>,
    /// A regular expression, applied anchored — see [`MAX_PATTERN_BYTES`].
    pub pattern: Option<String>,
    pub enum_values: Option<Vec<String>>,
    /// When this field applies at all (Q.19b).
    ///
    /// In `validation` rather than a column of its own, and that is a judgement worth stating: a dependency
    /// *is* a constraint — "this field is only meaningful when that one says so" — and the column already
    /// exists, so the alternative was a migration plus a new `FieldDef` member plus every construction site in
    /// the workspace. An older build reading a newer definition ignores an unknown member, which here means
    /// showing the field always: the safe direction, since the other one refuses somebody's data.
    pub depends_on: Option<Dependency>,
}

/// The condition under which a field applies (Q.19b).
///
/// One level only: a field's parent must not itself be dependent. Chains are where this becomes a graph, and a
/// graph needs cycle detection, an evaluation order, and an answer to "what does a field two hops from a
/// cleared parent mean" — none of which anybody has asked for. The rule is checked where a definition is
/// written, so a schema cannot hold one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// The field whose value decides.
    pub key: String,
    /// The values that make this field apply. Compared as text, so `true`, `2026` and `Yes` all work — the
    /// parent's own kind already governs what can be stored there.
    pub values: Vec<String>,
}

impl Dependency {
    /// Whether this field applies, given the effective document.
    ///
    /// The *effective* document is the patch over what is stored: a dependent field's parent is often
    /// untouched by the write that fills the child in, and judging the condition on the payload alone would
    /// refuse a perfectly good edit.
    #[must_use]
    pub fn holds(&self, effective: &Map<String, Value>) -> bool {
        let Some(actual) = effective.get(&self.key) else {
            return false;
        };
        let matches = |value: &Value| {
            let rendered = match value {
                Value::String(text) => text.clone(),
                Value::Null => return false,
                other => other.to_string(),
            };
            self.values.iter().any(|wanted| wanted == &rendered)
        };
        match actual {
            // A multivalued parent applies if *any* of its values match, which is what "when the shoot is
            // tagged editorial" means to the person writing the rule.
            Value::Array(items) => items.iter().any(matches),
            single => matches(single),
        }
    }
}

/// Longest permitted `pattern`.
///
/// A field definition is tenant-controlled input reaching a regex compiler on every write. The `regex`
/// crate cannot backtrack, so this is not catastrophic-blowup territory — but compiling a megabyte of
/// pattern still costs real time per request, and no legitimate pattern is anywhere near this.
pub const MAX_PATTERN_BYTES: usize = 512;

impl Constraints {
    /// Reads the constraints out of a `field_defs.validation` object.
    ///
    /// Unknown members are ignored rather than refused, which is the opposite of the rule for payload
    /// keys — and deliberately: this is our own schema, so an unknown member is a newer build's
    /// constraint that an older one cannot enforce, and refusing would take the whole tenant down on a
    /// rollback. A payload key, by contrast, is a user's data.
    pub fn from_json(value: &Value) -> Self {
        let object = match value.as_object() {
            Some(object) => object,
            None => return Self::default(),
        };
        Self {
            min: object.get("min").and_then(Value::as_f64),
            max: object.get("max").and_then(Value::as_f64),
            min_length: object
                .get("min_length")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok()),
            max_length: object
                .get("max_length")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok()),
            pattern: object
                .get("pattern")
                .and_then(Value::as_str)
                .map(str::to_owned),
            enum_values: object.get("enum").and_then(Value::as_array).map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }),
            // A dependency with no values is no dependency: it could never hold, so the field would be
            // permanently inapplicable — read as absent rather than stored as a field nobody can fill.
            depends_on: object
                .get("depends_on")
                .and_then(Value::as_object)
                .and_then(|dependency| {
                    let key = dependency.get("key").and_then(Value::as_str)?;
                    let values: Vec<String> = dependency
                        .get("values")
                        .and_then(Value::as_array)?
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    (!values.is_empty()).then(|| Dependency {
                        key: key.to_owned(),
                        values,
                    })
                }),
        }
    }

    /// The `validation` object these constraints came from, for writing one back.
    ///
    /// Only what is set, so a definition with no constraints stores `{}` rather than a document of nulls — and
    /// an unknown member a newer build wrote is *lost* here rather than preserved, which is the one thing this
    /// round trip cannot do and is worth knowing before somebody adds a constraint on an older deployment.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        if let Some(min) = self.min {
            object.insert("min".to_owned(), min.into());
        }
        if let Some(max) = self.max {
            object.insert("max".to_owned(), max.into());
        }
        if let Some(min_length) = self.min_length {
            object.insert("min_length".to_owned(), min_length.into());
        }
        if let Some(max_length) = self.max_length {
            object.insert("max_length".to_owned(), max_length.into());
        }
        if let Some(pattern) = &self.pattern {
            object.insert("pattern".to_owned(), pattern.clone().into());
        }
        if let Some(values) = &self.enum_values {
            object.insert("enum".to_owned(), values.clone().into());
        }
        if let Some(dependency) = &self.depends_on {
            object.insert(
                "depends_on".to_owned(),
                serde_json::json!({"key": dependency.key, "values": dependency.values}),
            );
        }
        Value::Object(object)
    }
}

/// One row of `field_defs`, as far as validation cares.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDef {
    pub key: String,
    pub kind: FieldKind,
    /// Required when `kind` is `taxonomy_ref`; the CHECK constraint enforces that in the database.
    pub taxonomy_id: Option<Uuid>,
    pub multivalued: bool,
    pub required: bool,
    /// System-maintained: dimensions, byte counts, hashes. No writer may set it.
    pub read_only: bool,
    /// Whether enrichment may write here. See the module docs.
    pub ai_writable: bool,
    /// Whether this field may be faceted on.
    ///
    /// An administrator's decision, not a capability. Faceting a free-text field produces one bucket per
    /// distinct value — a million buckets on a million-asset library — so the flag is both governance and
    /// a resource guard, and a facet request naming a field without it is refused.
    pub facetable: bool,
    pub constraints: Constraints,
}

/// Who is writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    Human,
    /// An enrichment run. Restricted to `ai_writable` fields.
    Ai,
}

/// Whether the payload is the whole record or a change to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The complete metadata for a new asset. `required` applies.
    Create,
    /// A change. Absent keys are left alone, so `required` does not apply — but a key present with
    /// `null` is an instruction to clear, and that *is* refused for a required field.
    Patch,
}

/// Why one value was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// The payload key, or the field key for a missing required field.
    pub key: String,
    /// A stable machine-readable code. Stable because API clients branch on it and a UI maps it to a
    /// message in the user's language, neither of which can be done with prose.
    pub code: &'static str,
    pub detail: String,
}

impl Rejection {
    fn new(key: &str, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            key: key.to_owned(),
            code,
            detail: detail.into(),
        }
    }
}

/// A taxonomy term the caller must still resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxonomyRef {
    pub key: String,
    /// The taxonomy the *field* is bound to. The term must belong to this one.
    pub taxonomy_id: Uuid,
    pub term_id: Uuid,
}

/// A payload that passed the pure checks.
#[derive(Debug, Clone, PartialEq)]
pub struct Accepted {
    /// The normalised values, ordered by key.
    ///
    /// Ordered so equal input produces byte-identical `jsonb`, which is what makes "did this write
    /// change anything" answerable without a deep compare and keeps an audit diff readable.
    pub values: BTreeMap<String, Value>,
    /// Every taxonomy reference, in payload order, for the caller to resolve in one query.
    pub taxonomy_refs: Vec<TaxonomyRef>,
}

/// Validates `payload` against `defs`.
///
/// Returns every rejection rather than the first. See the module docs for why that, `required`-on-create
/// only, and unknown-key refusal are the way round they are.
pub fn validate(
    defs: &[FieldDef],
    payload: &Map<String, Value>,
    mode: Mode,
    writer: Writer,
    stored: &Map<String, Value>,
) -> Result<Accepted, Vec<Rejection>> {
    let mut rejections = Vec::new();
    let mut values = BTreeMap::new();
    let mut taxonomy_refs = Vec::new();

    // The document as it will be, for judging dependencies (Q.19b): the patch over what is already there. A
    // condition judged on the payload alone would refuse a write that filled in a child field without
    // restating its parent, which is the ordinary shape of an edit.
    let mut effective = stored.clone();
    for (key, value) in payload {
        effective.insert(key.clone(), value.clone());
    }

    for (key, value) in payload {
        let Some(def) = defs.iter().find(|d| &d.key == key) else {
            // Refused, not ignored. See the module docs.
            rejections.push(Rejection::new(
                key,
                "unknown_field",
                "no field definition with this key; a typo here would otherwise discard the value \
                 silently",
            ));
            continue;
        };

        if def.read_only {
            rejections.push(Rejection::new(
                key,
                "read_only",
                "this field is maintained by the system; a client that could set it could make the \
                 metadata disagree with the file",
            ));
            continue;
        }
        if writer == Writer::Ai && !def.ai_writable {
            rejections.push(Rejection::new(
                key,
                "not_ai_writable",
                "enrichment may not write to this field",
            ));
            continue;
        }

        // A field whose condition does not hold is not a field this asset has (Q.19b). Refused rather than
        // dropped, for the same reason an unknown key is: a value silently discarded is a value somebody
        // believes they saved. Clearing is allowed through, because letting somebody empty a field that has
        // become irrelevant is the only way to tidy up after a parent changes.
        if let Some(dependency) = &def.constraints.depends_on
            && !value.is_null()
            && !dependency.holds(&effective)
        {
            rejections.push(Rejection::new(
                key,
                "not_applicable",
                format!(
                    "this field applies only when {} is one of {}",
                    dependency.key,
                    dependency.values.join(", ")
                ),
            ));
            continue;
        }

        // `null` is an instruction to clear, which is different from absence — without the distinction
        // there is no way to empty a field at all.
        if value.is_null() {
            if def.required {
                rejections.push(Rejection::new(
                    key,
                    "required",
                    "a required field cannot be cleared",
                ));
            } else {
                values.insert(key.clone(), Value::Null);
            }
            continue;
        }

        let before = rejections.len();
        let normalised = match (def.multivalued, value) {
            (true, Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    match check_one(def, item, Some(index), &mut taxonomy_refs) {
                        Ok(value) => out.push(value),
                        Err(rejection) => rejections.push(rejection),
                    }
                }
                Value::Array(out)
            }
            (true, _) => {
                // Not coerced. Wrapping "red,blue" produces one wrong value that nothing downstream can
                // tell from a deliberate one.
                rejections.push(Rejection::new(
                    key,
                    "not_multivalued",
                    "this field takes an array; a bare value is not wrapped, because a delimited \
                     string would silently become one wrong value",
                ));
                Value::Null
            }
            (false, Value::Array(_)) => {
                rejections.push(Rejection::new(
                    key,
                    "multivalued",
                    "this field takes a single value, not an array",
                ));
                Value::Null
            }
            (false, single) => match check_one(def, single, None, &mut taxonomy_refs) {
                Ok(value) => value,
                Err(rejection) => {
                    rejections.push(rejection);
                    Value::Null
                }
            },
        };

        if rejections.len() == before {
            // A blank string satisfies presence and means nothing, so it is treated as absent for the
            // purpose of `required` — checked here rather than in `check_one` because it is about the
            // field's presence, not the value's type.
            if def.required && is_blank(&normalised) {
                rejections.push(Rejection::new(
                    key,
                    "required",
                    "a blank value does not satisfy a required field",
                ));
            } else {
                values.insert(key.clone(), normalised);
            }
        }
    }

    if mode == Mode::Create {
        for def in defs.iter().filter(|d| d.required && !d.read_only) {
            // A required *dependent* field is required only when it applies (Q.19b). Otherwise every asset
            // would have to answer a question that is not being asked of it — which is the whole point of
            // making the field dependent.
            if def
                .constraints
                .depends_on
                .as_ref()
                .is_some_and(|dependency| !dependency.holds(&effective))
            {
                continue;
            }
            if !values.contains_key(&def.key) && !payload.contains_key(&def.key) {
                rejections.push(Rejection::new(
                    &def.key,
                    "required",
                    "this field must be supplied when creating an asset",
                ));
            }
        }
    }

    if rejections.is_empty() {
        Ok(Accepted {
            values,
            taxonomy_refs,
        })
    } else {
        Err(rejections)
    }
}

/// Whether a value counts as "nothing was really supplied".
fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// Checks and normalises one scalar. `index` is `Some` for an element of a multivalued field.
fn check_one(
    def: &FieldDef,
    value: &Value,
    index: Option<usize>,
    taxonomy_refs: &mut Vec<TaxonomyRef>,
) -> Result<Value, Rejection> {
    let at = |detail: String| -> Rejection {
        // The index is in the message because "expected an integer" on a twelve-element array is not
        // actionable without it.
        let detail = match index {
            Some(index) => format!("element {index}: {detail}"),
            None => detail,
        };
        Rejection::new(&def.key, "type", detail)
    };

    let normalised = match def.kind {
        FieldKind::Text | FieldKind::Textarea | FieldKind::LongText => {
            let text = value
                .as_str()
                .ok_or_else(|| at("expected a string".to_owned()))?;
            check_text(def, text, index)?;
            // Not trimmed: trimming silently alters a value somebody chose.
            Value::String(text.to_owned())
        }
        FieldKind::Select | FieldKind::MultiSelect => {
            let text = value
                .as_str()
                .ok_or_else(|| at("expected a string".to_owned()))?;
            check_text(def, text, index)?;
            Value::String(text.to_owned())
        }
        FieldKind::Int => {
            let number = value
                .as_f64()
                .ok_or_else(|| at("expected a number".to_owned()))?;
            // JSON has one number type, so 2026.0 *is* an integer while 2026.5 is not. Truncating would
            // store a value nobody sent.
            if number.fract() != 0.0 {
                return Err(at(format!("expected an integer, got {number}")));
            }
            if !number.is_finite() || number.abs() > i64::MAX as f64 {
                return Err(at(format!(
                    "{number} does not fit in a 64-bit integer; refused here rather than at the \
                     insert, where the error would name a column instead of this field"
                )));
            }
            check_range(def, number, index)?;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "bounded against i64::MAX immediately above, and the fraction is zero"
            )]
            Value::from(number as i64)
        }
        FieldKind::Decimal => {
            let number = value
                .as_f64()
                .ok_or_else(|| at("expected a number".to_owned()))?;
            if !number.is_finite() {
                return Err(at("expected a finite number".to_owned()));
            }
            check_range(def, number, index)?;
            value.clone()
        }
        FieldKind::Bool => {
            // Not coerced from "true"/"on". Coercion means the string "true" can never be stored, and
            // it hides a client that is not sending JSON types.
            value
                .as_bool()
                .ok_or_else(|| at("expected true or false, not a string".to_owned()))?;
            value.clone()
        }
        FieldKind::Date => {
            let text = value
                .as_str()
                .ok_or_else(|| at("expected a string".to_owned()))?;
            // Strict, and specifically not accepting a timestamp: a `date` that swallows one acquires a
            // timezone, and "shot on 2026-08-17" becomes a different day depending on where it is read.
            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .map_err(|_| at(format!("expected YYYY-MM-DD, got {text:?}")))?;
            Value::String(text.to_owned())
        }
        FieldKind::DateTime => {
            let text = value
                .as_str()
                .ok_or_else(|| at("expected a string".to_owned()))?;
            // RFC 3339, which requires an offset. A local timestamp is ambiguous by up to 26 hours, and
            // the ambiguity surfaces as an embargo lifting on the wrong day.
            chrono::DateTime::parse_from_rfc3339(text).map_err(|_| {
                at(format!(
                    "expected an RFC 3339 timestamp with an offset, got {text:?}"
                ))
            })?;
            Value::String(text.to_owned())
        }
        FieldKind::UserRef => {
            let text = value
                .as_str()
                .ok_or_else(|| at("expected a string".to_owned()))?;
            Uuid::parse_str(text).map_err(|_| at(format!("expected a UUID, got {text:?}")))?;
            Value::String(text.to_owned())
        }
        FieldKind::TaxonomyRef => {
            let Some(taxonomy_id) = def.taxonomy_id else {
                // Unreachable from the database, where a CHECK constraint requires it. Reported as a
                // *definition* error so nobody debugs their payload when the schema is wrong.
                return Err(Rejection::new(
                    &def.key,
                    "definition_invalid",
                    "this field is a taxonomy_ref with no taxonomy_id; the field definition is wrong, \
                     not the value",
                ));
            };
            let text = value
                .as_str()
                .ok_or_else(|| at("expected a string".to_owned()))?;
            let term_id = Uuid::parse_str(text).map_err(|_| {
                at(format!(
                    "expected a term UUID, got {text:?} — a slug or label means the client has not \
                     resolved its terms, and resolving arbitrary text per value turns a write into a scan"
                ))
            })?;
            taxonomy_refs.push(TaxonomyRef {
                key: def.key.clone(),
                taxonomy_id,
                term_id,
            });
            Value::String(term_id.to_string())
        }
        FieldKind::Url => {
            let text = value
                .as_str()
                .ok_or_else(|| at("expected a string".to_owned()))?;
            let parsed = url::Url::parse(text)
                .map_err(|e| at(format!("expected a URL, got {text:?} ({e})")))?;
            // An allowlist, not a denylist. A `javascript:` or `data:` URL in a field a UI renders as a
            // link is stored cross-site scripting, and a denylist of the schemes we thought of is one
            // scheme away from being wrong.
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(Rejection::new(
                    &def.key,
                    "url_scheme",
                    format!(
                        "only http and https are accepted, got {:?}; other schemes are executable in \
                         a browser when the value is rendered as a link",
                        parsed.scheme()
                    ),
                ));
            }
            check_text(def, text, index)?;
            Value::String(text.to_owned())
        }
        FieldKind::Geo => {
            let object = value
                .as_object()
                .ok_or_else(|| at("expected an object with lat and lon".to_owned()))?;
            let lat = object
                .get("lat")
                .and_then(Value::as_f64)
                .ok_or_else(|| at("expected a numeric lat".to_owned()))?;
            let lon = object
                .get("lon")
                .and_then(Value::as_f64)
                .ok_or_else(|| at("expected a numeric lon".to_owned()))?;
            if !(-90.0..=90.0).contains(&lat) {
                return Err(at(format!("lat {lat} is outside -90..=90")));
            }
            if !(-180.0..=180.0).contains(&lon) {
                return Err(at(format!("lon {lon} is outside -180..=180")));
            }
            serde_json::json!({"lat": lat, "lon": lon})
        }
    };
    Ok(normalised)
}

fn check_text(def: &FieldDef, text: &str, index: Option<usize>) -> Result<(), Rejection> {
    let position = |detail: String| match index {
        Some(index) => format!("element {index}: {detail}"),
        None => detail,
    };

    // Characters, not bytes. A five-character limit that rejects "café" at four is a bug a European
    // customer finds on their first import.
    let length = text.chars().count();
    if let Some(min) = def.constraints.min_length
        && length < min
    {
        return Err(Rejection::new(
            &def.key,
            "min_length",
            position(format!("{length} characters, minimum {min}")),
        ));
    }
    if let Some(max) = def.constraints.max_length
        && length > max
    {
        return Err(Rejection::new(
            &def.key,
            "max_length",
            position(format!("{length} characters, maximum {max}")),
        ));
    }

    if let Some(allowed) = &def.constraints.enum_values
        && !allowed.iter().any(|value| value == text)
    {
        return Err(Rejection::new(
            &def.key,
            "enum",
            position(format!("{text:?} is not one of {allowed:?}")),
        ));
    }

    if let Some(pattern) = &def.constraints.pattern {
        // Fail closed on a broken pattern. Skipping it would leave a tenant with a field that has no
        // validation at all and no indication of it, which is the failure nobody notices for a year.
        if pattern.len() > MAX_PATTERN_BYTES {
            return Err(Rejection::new(
                &def.key,
                "pattern_invalid",
                format!(
                    "the field's pattern is {} bytes, over the {MAX_PATTERN_BYTES}-byte limit",
                    pattern.len()
                ),
            ));
        }
        // Anchored. An unanchored `[A-Z]{3}` would accept "oops ABC oops", so a tenant could write a
        // permissive pattern without meaning to.
        let anchored = format!("^(?:{pattern})$");
        let compiled = regex::Regex::new(&anchored).map_err(|e| {
            Rejection::new(
                &def.key,
                "pattern_invalid",
                format!("the field's pattern does not compile: {e}"),
            )
        })?;
        if !compiled.is_match(text) {
            return Err(Rejection::new(
                &def.key,
                "pattern",
                position(format!("{text:?} does not match the field's pattern")),
            ));
        }
    }
    Ok(())
}

fn check_range(def: &FieldDef, number: f64, index: Option<usize>) -> Result<(), Rejection> {
    let position = |detail: String| match index {
        Some(index) => format!("element {index}: {detail}"),
        None => detail,
    };
    if let Some(min) = def.constraints.min
        && number < min
    {
        return Err(Rejection::new(
            &def.key,
            "min",
            position(format!("{number} is below the minimum {min}")),
        ));
    }
    if let Some(max) = def.constraints.max
        && number > max
    {
        return Err(Rejection::new(
            &def.key,
            "max",
            position(format!("{number} is above the maximum {max}")),
        ));
    }
    Ok(())
}
