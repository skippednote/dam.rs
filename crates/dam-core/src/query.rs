//! The query IR (2.4): one parsed representation, shared by SQL and Tantivy.
//!
//! §12 puts it plainly — the access rules are compiled once and reused by SQL, Tantivy and MCP, because
//! *divergence here is a data leak*. That is what this module is for. A user's query is parsed into
//! [`Query`], validated against the tenant's `field_defs`, and combined with an
//! [`AccessPredicate`](crate::policy::AccessPredicate) into a [`Planned`] that each back end renders in
//! its own dialect. Two back ends parsing the same string separately is the arrangement where one of them
//! eventually returns a row the other would have hidden.
//!
//! ## The access filter cannot be forgotten
//!
//! §7 requires it in the query rather than as a post-filter, and gives the reason: pagination counts
//! alone disclose the existence of assets a caller cannot see. A post-filter returns the same *rows*, so
//! the two are indistinguishable until someone compares a `count(*)` with the row set.
//!
//! Rather than trust every renderer to remember, [`Planned`] is the only thing a renderer accepts and its
//! only constructor takes an `AccessPredicate`. There is no way to express an unfiltered query in this
//! type system — which is a stronger guarantee than a test, because it survives the next person who adds
//! a third back end.
//!
//! ## A query is untrusted input
//!
//! It arrives from an HTTP request, and both renderers walk it recursively. So depth and node count are
//! bounded ([`MAX_DEPTH`], [`MAX_NODES`]) before any rendering happens: without that, a few kilobytes of
//! nested boolean is a stack overflow in the renderer and a query planner sitting on thousands of
//! subqueries. Refusing at parse time costs nothing and is the only place the bound is cheap.
//!
//! ## Empty conjunctions and disjunctions
//!
//! `And([])` is true and `Or([])` is false, which is the standard reading and also the safe one. The
//! dangerous mistake is rendering an empty `Or` as nothing at all: `WHERE ()` is a syntax error if you
//! are lucky, and a silently dropped filter — every asset in the tenant — if you are not.

use crate::fields::{FieldDef, FieldKind, Rejection};
use crate::policy::AccessPredicate;
use chrono::{DateTime, NaiveDate, Utc};
use uuid::Uuid;

/// Deepest boolean nesting accepted.
///
/// Both renderers recurse, so this is a stack bound as much as a cost bound. Sixteen is far past any
/// query a person writes or a filter rail produces.
pub const MAX_DEPTH: usize = 16;

/// Most nodes accepted in one query.
///
/// A thousand `OR` terms is a plausible machine-generated filter ("any of these 900 asset ids"), so the
/// limit is well above hand-written queries while still bounding the SQL handed to the planner.
pub const MAX_NODES: usize = 1024;

/// A value in a comparison.
///
/// Typed rather than a string, so a renderer binds a parameter of the right Postgres type instead of
/// casting text at query time — which would defeat the `jsonb` GIN index and turn a filter into a scan.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Text(String),
    Int(i64),
    Decimal(f64),
    Bool(bool),
    Date(NaiveDate),
    DateTime(DateTime<Utc>),
    Uuid(Uuid),
}

impl Literal {
    fn describe(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Int(_) => "an integer",
            Self::Decimal(_) => "a decimal",
            Self::Bool(_) => "a boolean",
            Self::Date(_) => "a date",
            Self::DateTime(_) => "a timestamp",
            Self::Uuid(_) => "a UUID",
        }
    }

    /// Whether this literal can be compared against a field of `kind`.
    ///
    /// An integral literal is accepted for a `decimal` field, because `2` and `2.0` are the same number
    /// and refusing would make `price > 2` fail for no reason a user could understand.
    fn fits(&self, kind: FieldKind) -> bool {
        match kind {
            FieldKind::Text
            | FieldKind::Textarea
            | FieldKind::LongText
            | FieldKind::Select
            | FieldKind::MultiSelect
            | FieldKind::Url => matches!(self, Self::Text(_)),
            FieldKind::Int => matches!(self, Self::Int(_)),
            FieldKind::Decimal => matches!(self, Self::Decimal(_) | Self::Int(_)),
            FieldKind::Bool => matches!(self, Self::Bool(_)),
            FieldKind::Date => matches!(self, Self::Date(_)),
            FieldKind::DateTime => matches!(self, Self::DateTime(_)),
            FieldKind::TaxonomyRef | FieldKind::UserRef => matches!(self, Self::Uuid(_)),
            // No ordering or equality a user would mean by it; matched only by presence.
            FieldKind::Geo => false,
        }
    }
}

/// One end of a range.
#[derive(Debug, Clone, PartialEq)]
pub enum Endpoint {
    Unbounded,
    Inclusive(Literal),
    Exclusive(Literal),
}

impl Endpoint {
    fn literal(&self) -> Option<&Literal> {
        match self {
            Self::Unbounded => None,
            Self::Inclusive(literal) | Self::Exclusive(literal) => Some(literal),
        }
    }
}

/// What a field comparison asks.
#[derive(Debug, Clone, PartialEq)]
pub enum Comparison {
    Equals(Literal),
    NotEquals(Literal),
    /// At least one end must be bounded — see [`Query::validate`].
    Range {
        lower: Endpoint,
        upper: Endpoint,
    },
    /// The key is present and not null.
    Exists,
    /// The key is absent or null. Not the negation of `Exists` for multivalued fields, where an empty
    /// array is present *and* empty; both treat that as missing, which is what a user means by "no brand".
    Missing,
    /// Case-insensitive substring. Text kinds only.
    Contains(String),
    /// Case-insensitive prefix. Cheaper than `Contains` and index-usable, so it is a separate operator
    /// rather than a special case a renderer has to detect.
    StartsWith(String),
}

impl Comparison {
    fn name(&self) -> &'static str {
        match self {
            Self::Equals(_) => "=",
            Self::NotEquals(_) => "!=",
            Self::Range { .. } => "a range",
            Self::Exists => "exists",
            Self::Missing => "missing",
            Self::Contains(_) => "contains",
            Self::StartsWith(_) => "starts-with",
        }
    }
}

/// A parsed query.
///
/// Deliberately not carrying an access filter: this is *what the user asked for*, and mixing the two into
/// one tree is how an access term ends up inside a `Not`. The combination happens once, in [`Planned`].
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    /// Every asset the caller may see. Still access-filtered — see [`Planned`].
    All,
    /// Free text over the tenant's `searchable` fields.
    Text(String),
    Field {
        key: String,
        op: Comparison,
    },
    /// Taxonomy membership.
    Term {
        term_id: Uuid,
        /// Whether terms below this one in the hierarchy also match. Almost always what a user means by
        /// clicking a category, and the reason the paths are `ltree`.
        include_descendants: bool,
    },
    /// Collection membership.
    InCollection(Uuid),
    /// The asset's *average* rating, compared. Caller-independent: it is the library's shared judgement.
    Rating(Comparison),
    /// The asset's lifecycle status — `active`, `archived`, and the transient ones (Q.15).
    ///
    /// A column rather than a metadata field, so it is a clause of its own: a tenant field called `status`
    /// would otherwise shadow it, and the filter rail's own links would stop working for reasons nobody could
    /// see. The value is not validated against the CHECK here — an unknown one simply matches nothing, which
    /// is what a query for a status that does not exist should do.
    Status(String),
    /// The shape of the frame, derived from the stored dimensions (Q.15).
    ///
    /// Free: `width` and `height` are already probed and stored, so this needs no column, no backfill and no
    /// reindex. An asset with no dimensions — a PDF, an audio file — matches no orientation rather than
    /// defaulting to one.
    Orientation(Orientation),
    /// Whether paperwork is attached to the asset (Q.15).
    ///
    /// `has:attachment`. Relational — it is an `EXISTS` over the assets that name this one as their parent —
    /// and deliberately not "how many": a rail offers the distinction between some and none, and a count of
    /// release forms is a detail panel's business.
    HasAttachment,
    /// Something only the caller can answer about themselves.
    ///
    /// Deliberately holds no identity. A saved search stores the query IR, so an identity baked in here would
    /// make a search shared with a colleague return *the author's* favourites — the permanent leak wearing the
    /// shape of a bookmark that `dam_db::saved_searches` exists to avoid. Who "mine" refers to is caller context
    /// and travels with the access predicate, in [`Planned::viewed_by`].
    Mine(Personal),
    And(Vec<Query>),
    Or(Vec<Query>),
    Not(Box<Query>),
}

/// The shape of a frame.
///
/// Three values and no "tall"/"wide" synonyms: the rail writes what the parser reads, and a fourth spelling is
/// a fourth thing to keep in step. A square is its own bucket rather than being folded into landscape, because
/// a square crop is a deliberate thing somebody filters *for*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    Landscape,
    Portrait,
    Square,
}

impl Orientation {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Landscape => "landscape",
            Self::Portrait => "portrait",
            Self::Square => "square",
        }
    }

    /// The value a query string or a facet bucket names, or `None`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "landscape" => Some(Self::Landscape),
            "portrait" => Some(Self::Portrait),
            "square" => Some(Self::Square),
            _ => None,
        }
    }
}

/// A per-caller engagement state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Personal {
    Favourite,
    Watched,
    /// Rated by this caller, at any number of stars. Distinct from [`Query::Rating`], which is about the asset.
    Rated,
}

impl Personal {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Favourite => "favourite",
            Self::Watched => "watched",
            Self::Rated => "rated",
        }
    }
}

impl Query {
    /// Nodes in this tree.
    pub fn node_count(&self) -> usize {
        match self {
            Self::All
            | Self::Text(_)
            | Self::Field { .. }
            | Self::Term { .. }
            | Self::InCollection(_)
            | Self::Rating(_)
            | Self::Status(_)
            | Self::Orientation(_)
            | Self::HasAttachment
            | Self::Mine(_) => 1,
            Self::And(children) | Self::Or(children) => {
                1 + children.iter().map(Self::node_count).sum::<usize>()
            }
            Self::Not(inner) => 1 + inner.node_count(),
        }
    }

    /// Deepest nesting in this tree.
    ///
    /// Computed iteratively over an explicit stack rather than recursively: a query deep enough to be
    /// refused must not overflow the stack *while being measured*, which a recursive depth check would
    /// do before it could refuse anything.
    pub fn depth(&self) -> usize {
        let mut deepest = 0usize;
        let mut stack = vec![(self, 1usize)];
        while let Some((node, level)) = stack.pop() {
            deepest = deepest.max(level);
            // Bail out once the answer can only exceed the limit. Without this a maliciously deep tree
            // is still fully walked, which is the cost the limit exists to avoid.
            if deepest > MAX_DEPTH {
                return deepest;
            }
            match node {
                Self::And(children) | Self::Or(children) => {
                    for child in children {
                        stack.push((child, level + 1));
                    }
                }
                Self::Not(inner) => stack.push((inner.as_ref(), level + 1)),
                _ => {}
            }
        }
        deepest
    }

    /// Checks the query against the tenant's field definitions and the size bounds.
    ///
    /// Collects every problem, for the same reason 2.1 does: a filter rail that reports one broken clause
    /// at a time is a filter rail people give up on.
    pub fn validate(&self, defs: &[FieldDef]) -> Result<(), Vec<Rejection>> {
        let mut rejections = Vec::new();

        // Size first. The walks below are bounded by the tree, so refusing an oversized tree before
        // walking it is the point.
        let depth = self.depth();
        if depth > MAX_DEPTH {
            rejections.push(Rejection {
                key: "query".to_owned(),
                code: "too_deep",
                detail: format!(
                    "nested {depth} levels, maximum {MAX_DEPTH}; both renderers walk this tree, so a \
                     deeper one is a stack overflow rather than a slow query"
                ),
            });
        }
        let nodes = self.node_count();
        if nodes > MAX_NODES {
            rejections.push(Rejection {
                key: "query".to_owned(),
                code: "too_large",
                detail: format!("{nodes} clauses, maximum {MAX_NODES}"),
            });
        }
        if !rejections.is_empty() {
            return Err(rejections);
        }

        self.check_fields(defs, &mut rejections);
        if rejections.is_empty() {
            Ok(())
        } else {
            Err(rejections)
        }
    }

    fn check_fields(&self, defs: &[FieldDef], rejections: &mut Vec<Rejection>) {
        match self {
            Self::All | Self::Text(_) | Self::Term { .. } | Self::InCollection(_) => {}
            // Columns on `assets`, not fields the tenant defined, so there is no definition to check against.
            Self::Status(_) | Self::Orientation(_) | Self::HasAttachment => {}
            // Nothing to check against the schema: who "mine" refers to is caller context, not a field.
            Self::Mine(_) => {}
            Self::Rating(op) => check_rating(op, rejections),
            Self::And(children) | Self::Or(children) => {
                for child in children {
                    child.check_fields(defs, rejections);
                }
            }
            Self::Not(inner) => inner.check_fields(defs, rejections),
            Self::Field { key, op } => {
                let Some(def) = defs.iter().find(|d| &d.key == key) else {
                    // Refused rather than dropped, exactly as in 2.1: a query with a typo'd field that
                    // silently ignores the clause returns *more* than the user asked for, which for a
                    // filter is the wrong direction to be wrong in.
                    rejections.push(Rejection {
                        key: key.clone(),
                        code: "unknown_field",
                        detail:
                            "no field definition with this key; ignoring the clause would widen \
                                 the result set rather than narrow it"
                                .to_owned(),
                    });
                    return;
                };
                check_comparison(def, op, rejections);
            }
        }
    }
}

/// Validates a rating comparison.
///
/// A rating is a number from 1 to 5, so a substring operator is meaningless and a bound outside the range is a
/// filter that can only match everything or nothing. Both are refused rather than clamped: `stars:>=9` is a
/// mistake, and quietly turning it into `stars:>=5` would answer a question nobody asked.
///
/// `Missing` and `Exists` are allowed and mean "unrated" and "rated by anyone" — the two facts a rail needs
/// beside the star buckets, and neither is expressible any other way.
fn check_rating(op: &Comparison, rejections: &mut Vec<Rejection>) {
    let refuse = |code: &'static str, detail: String, rejections: &mut Vec<Rejection>| {
        rejections.push(Rejection {
            key: "stars".to_owned(),
            code,
            detail,
        });
    };
    let in_range = |literal: &Literal, rejections: &mut Vec<Rejection>| match literal {
        Literal::Int(stars) if (1..=5).contains(stars) => {}
        Literal::Int(stars) => refuse(
            "stars_range",
            format!("a rating is 1 to 5; {stars} can only match everything or nothing"),
            rejections,
        ),
        other => refuse(
            "stars_type",
            format!(
                "a rating is a whole number of stars, not {}",
                other.describe()
            ),
            rejections,
        ),
    };

    match op {
        Comparison::Equals(literal) | Comparison::NotEquals(literal) => {
            in_range(literal, rejections);
        }
        Comparison::Range { lower, upper } => {
            for endpoint in [lower, upper] {
                if let Endpoint::Inclusive(literal) | Endpoint::Exclusive(literal) = endpoint {
                    in_range(literal, rejections);
                }
            }
        }
        Comparison::Exists | Comparison::Missing => {}
        Comparison::Contains(_) | Comparison::StartsWith(_) => refuse(
            "stars_operator",
            format!(
                "a rating is a number, so {} does not apply to it",
                op.name()
            ),
            rejections,
        ),
    }
}

fn check_comparison(def: &FieldDef, op: &Comparison, rejections: &mut Vec<Rejection>) {
    let mismatch = |literal: &Literal, rejections: &mut Vec<Rejection>| {
        if !literal.fits(def.kind) {
            rejections.push(Rejection {
                key: def.key.clone(),
                code: "literal_type",
                detail: format!(
                    "{} is a {} field and cannot be compared with {}",
                    def.key,
                    def.kind.as_str(),
                    literal.describe()
                ),
            });
        }
    };

    match op {
        Comparison::Equals(literal) | Comparison::NotEquals(literal) => {
            mismatch(literal, rejections)
        }
        Comparison::Exists | Comparison::Missing => {}
        Comparison::Range { lower, upper } => {
            if matches!(lower, Endpoint::Unbounded) && matches!(upper, Endpoint::Unbounded) {
                // An unbounded range matches everything, which is a filter the user did not ask for and
                // almost certainly a client bug. `Exists` says the thing they probably meant.
                rejections.push(Rejection {
                    key: def.key.clone(),
                    code: "empty_range",
                    detail:
                        "a range needs at least one bound; an unbounded range matches every asset, \
                             which is what `exists` is for"
                            .to_owned(),
                });
            }
            if !is_orderable(def.kind) {
                rejections.push(Rejection {
                    key: def.key.clone(),
                    code: "not_orderable",
                    detail: format!(
                        "{} is a {} field, which has no ordering to range over",
                        def.key,
                        def.kind.as_str()
                    ),
                });
            }
            for endpoint in [lower, upper] {
                if let Some(literal) = endpoint.literal() {
                    mismatch(literal, rejections);
                }
            }
        }
        Comparison::Contains(_) | Comparison::StartsWith(_) => {
            if !def.kind.is_textual() && def.kind != FieldKind::Url {
                rejections.push(Rejection {
                    key: def.key.clone(),
                    code: "not_textual",
                    detail: format!(
                        "{} is a {} field, so {} does not apply — a substring match on a number or a \
                         date is a comparison the user did not mean",
                        def.key,
                        def.kind.as_str(),
                        op.name()
                    ),
                });
            }
        }
    }
}

/// Whether values of this kind have an ordering a range can use.
fn is_orderable(kind: FieldKind) -> bool {
    matches!(
        kind,
        FieldKind::Int | FieldKind::Decimal | FieldKind::Date | FieldKind::DateTime
    )
}

/// A validated query with its access filter attached.
///
/// The only thing a renderer accepts, and its only constructor takes an [`AccessPredicate`]. That is the
/// §7/§12 guarantee expressed in the type system rather than in a review comment: there is no value of
/// this type that lacks an access filter, so no renderer can omit one and no future back end can forget.
#[derive(Debug, Clone)]
pub struct Planned {
    access: AccessPredicate,
    query: Query,
    text_fields: Vec<String>,
    /// Every field's kind, so a renderer can type its extraction without re-loading the definitions.
    ///
    /// Captured at construction rather than looked up per clause: a renderer holding its own copy of the
    /// definitions could disagree with the one validation used, and the disagreement would show up as a
    /// range comparing text instead of numbers — silently wrong rather than an error.
    kinds: Vec<(String, FieldKind)>,
    /// Who "mine" means, for [`Query::Mine`].
    ///
    /// Beside the access predicate rather than inside the query, for the reason on `Query::Mine`: the tree is
    /// what the user asked for and is shareable, while this is who is asking and is not. `None` until a caller
    /// says; a renderer meeting a personal clause without it fails loudly rather than returning an empty page,
    /// because "nothing matched" and "nobody told me who you are" look identical on a screen.
    viewer: Option<Uuid>,
}

impl Planned {
    /// Validates `query` and binds it to `access`.
    ///
    /// `text_fields` is derived here rather than at each renderer, so a free-text search covers the same
    /// fields in SQL and in Tantivy. Two back ends deciding that separately is the divergence §12 warns
    /// about, in its quietest form: the same query returning different rows depending on which index
    /// happened to serve it.
    pub fn new(
        query: Query,
        access: AccessPredicate,
        defs: &[FieldDef],
    ) -> Result<Self, Vec<Rejection>> {
        query.validate(defs)?;
        let text_fields = defs
            .iter()
            .filter(|def| def.kind.is_textual())
            .map(|def| def.key.clone())
            .collect();
        let kinds = defs.iter().map(|def| (def.key.clone(), def.kind)).collect();
        Ok(Self {
            access,
            query,
            text_fields,
            kinds,
            viewer: None,
        })
    }

    /// Names the caller, so `Query::Mine` clauses can be rendered.
    ///
    /// A builder rather than a fourth argument to [`Planned::new`]: most queries have no personal clause and
    /// would carry the parameter for nothing. The renderer's refusal is what keeps this from being silently
    /// forgettable.
    #[must_use]
    pub fn viewed_by(mut self, identity: Uuid) -> Self {
        self.viewer = Some(identity);
        self
    }

    /// Who "mine" refers to, if anyone has said.
    #[must_use]
    pub fn viewer(&self) -> Option<Uuid> {
        self.viewer
    }

    pub fn access(&self) -> &AccessPredicate {
        &self.access
    }

    pub fn query(&self) -> &Query {
        &self.query
    }

    /// The fields a bare [`Query::Text`] searches.
    ///
    /// Ordered as the definitions are, so two renderers produce the same field list and a differential
    /// test can compare them.
    pub fn text_fields(&self) -> &[String] {
        &self.text_fields
    }

    /// The kind of a field, as validation saw it.
    pub fn field_kind(&self, key: &str) -> Option<FieldKind> {
        self.kinds
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, kind)| *kind)
    }

    /// Whether this query can match nothing at all, whatever the user asked.
    ///
    /// Checked by renderers so they can emit a false condition instead of a query, which is both faster
    /// and — more importantly — impossible to get subtly wrong.
    pub fn matches_nothing(&self) -> bool {
        self.access.matches_nothing()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_is_measured_without_recursing() {
        // The measurement itself must survive a tree deep enough to be refused, or the check overflows
        // the stack before it can refuse anything. Built with 10,000 levels, far past MAX_DEPTH.
        let mut query = Query::All;
        for _ in 0..10_000 {
            query = Query::Not(Box::new(query));
        }
        assert!(query.depth() > MAX_DEPTH);
    }

    #[test]
    fn depth_stops_counting_once_the_limit_is_passed() {
        // A refused query must not be walked in full — that cost is what the limit exists to avoid.
        let mut query = Query::All;
        for _ in 0..1_000 {
            query = Query::Not(Box::new(query));
        }
        let depth = query.depth();
        assert!(depth > MAX_DEPTH);
        assert!(
            depth <= MAX_DEPTH + 1,
            "the walk should stop just past the limit, got {depth}"
        );
    }

    #[test]
    fn an_empty_conjunction_is_one_node() {
        assert_eq!(Query::And(vec![]).node_count(), 1);
        assert_eq!(Query::And(vec![]).depth(), 1);
    }
}
