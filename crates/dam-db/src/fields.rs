//! Loading field definitions and resolving what the pure validator could not (2.1).
//!
//! `dam_core::fields` does every check that is decidable from the payload and the definitions. Exactly
//! one is not: whether a taxonomy term belongs to the taxonomy its field is bound to. That is a row, so
//! it lands here.
//!
//! Resolution is **one query for every reference in the payload**, not one per value. An asset with
//! twenty category terms is a common shape, and a query per term makes a metadata write twenty round
//! trips — which is the kind of thing that only shows up under a bulk import, when it is expensive.

use crate::Error;
use dam_core::fields::{
    Accepted, Constraints, FieldDef, FieldKind, Mode, Rejection, TaxonomyRef, Writer,
};
use serde_json::{Map, Value};
use std::collections::HashMap;
use uuid::Uuid;

/// Loads every field definition for the current tenant, in display order.
pub async fn load<'e, E>(executor: E) -> Result<Vec<FieldDef>, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            Option<Uuid>,
            bool,
            bool,
            bool,
            bool,
            bool,
            serde_json::Value,
        ),
    >(
        "SELECT key, kind, taxonomy_id, multivalued, required, read_only, ai_writable, facetable, \
                validation \
         FROM field_defs ORDER BY display_order, key",
    )
    .fetch_all(executor)
    .await?;

    rows.into_iter()
        .map(
            |(
                key,
                kind,
                taxonomy_id,
                multivalued,
                required,
                read_only,
                ai_writable,
                facetable,
                validation,
            )| {
                Ok(FieldDef {
                    kind: FieldKind::parse(&kind)?,
                    key,
                    taxonomy_id,
                    multivalued,
                    required,
                    read_only,
                    ai_writable,
                    facetable,
                    constraints: Constraints::from_json(&validation),
                })
            },
        )
        .collect()
}

/// A field definition as an editor needs it.
///
/// Wider than [`FieldDef`], which is the *validator's* view and deliberately carries no presentation: a
/// label and a search alias are affordances rather than properties of the field. A form needs both, plus
/// `multivalued` and `read_only` — and those two are not cosmetic. A UI that does not know a field is
/// multivalued sends a comma-joined string to a field that takes an array and gets a 422 the user cannot
/// act on, and one that does not know a field is read-only offers an edit the server will refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalogued {
    pub key: String,
    pub label: String,
    /// The database spelling, so a client can pick an input type.
    pub kind: String,
    pub multivalued: bool,
    pub required: bool,
    pub read_only: bool,
    pub ai_writable: bool,
    pub facetable: bool,
    /// Whether the value joins the index at all. Not cosmetic and not derivable: a schema editor that
    /// cannot see it cannot explain why a field it just added returns nothing from search.
    pub searchable: bool,
    pub search_alias: Option<String>,
    /// The validation object as stored, so a caller can read the parts it cares about — the dependency, for a
    /// form that has to decide whether to show the field at all (Q.19b). Passed through rather than parsed
    /// here, because `Constraints` is `dam_core`'s vocabulary and this struct is a row.
    pub validation: Value,
    /// The taxonomy a term field is bound to, when it is one.
    pub taxonomy_id: Option<Uuid>,
}

/// Every field definition, in display order, with everything a form needs.
pub async fn catalog<'e, E>(executor: E) -> Result<Vec<Catalogued>, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    let rows = sqlx::query_as::<
        _,
        (
            String,
            String,
            String,
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            Option<String>,
            Option<Uuid>,
            Value,
        ),
    >(
        "SELECT key, label, kind, multivalued, required, read_only, ai_writable, facetable, \
                searchable, search_alias, taxonomy_id, validation \
         FROM field_defs ORDER BY display_order, key",
    )
    .fetch_all(executor)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                key,
                label,
                kind,
                multivalued,
                required,
                read_only,
                ai_writable,
                facetable,
                searchable,
                search_alias,
                taxonomy_id,
                validation,
            )| Catalogued {
                key,
                label,
                kind,
                multivalued,
                required,
                read_only,
                ai_writable,
                facetable,
                searchable,
                search_alias,
                taxonomy_id,
                validation,
            },
        )
        .collect())
}

/// The tenant's search aliases: `search_alias` → field key.
///
/// Separate from [`load`] because a `FieldDef` deliberately does not carry the alias — the alias is a search
/// affordance rather than a property of the field, and the validator has no business knowing about it.
pub async fn aliases<'e, E>(executor: E) -> Result<HashMap<String, String>, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT search_alias, key FROM field_defs WHERE search_alias IS NOT NULL",
    )
    .fetch_all(executor)
    .await?;
    Ok(rows.into_iter().collect())
}

/// The tenant's schema as the shorthand parser needs it.
///
/// Assembled here rather than at each call site, because a caller that loaded definitions and aliases
/// separately could pair a fresh alias with a stale field list — and the symptom would be a shorthand key
/// resolving to a field that no longer exists.
pub async fn search_schema(pool: &sqlx::PgPool) -> Result<dam_core::shorthand::Schema, Error> {
    let mut conn = pool.acquire().await?;
    search_schema_on(&mut conn).await
}

/// [`search_schema`] on a caller's connection.
pub async fn search_schema_on(
    conn: &mut sqlx::PgConnection,
) -> Result<dam_core::shorthand::Schema, Error> {
    let defs = load(&mut *conn).await?;
    let aliases = aliases(&mut *conn).await?;
    // The category paths too, so `in:exterior.yellow` resolves while parsing rather than needing a second
    // round trip afterwards. Loaded from the same connection as the fields for the same reason they are
    // assembled together: a stale half would resolve a selector against a tree that has since changed.
    let categories = category_paths(&mut *conn).await?;
    Ok(dam_core::shorthand::Schema::new(defs, aliases).with_categories(categories))
}

/// Every live category path in every category tree, lower-cased, mapped to its term id.
///
/// Lower-cased because the selector is case-folded like every other one: a user typing the label they see on
/// screen is not wrong, and the filter rail's own links must survive a label's case changing.
///
/// Deprecated categories are excluded. A retired category cannot take new assets ([`crate::categories::file`]),
/// and offering it as a filter would keep it alive in the one place somebody would notice it — which is the
/// opposite of retiring it.
async fn category_paths<'e, E>(executor: E) -> Result<HashMap<String, Uuid>, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    let rows: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT lower(t.path::text), t.id FROM taxonomy_terms t \
         JOIN taxonomies x ON x.id = t.taxonomy_id AND x.kind = 'category' \
         WHERE t.deprecated_at IS NULL",
    )
    .fetch_all(executor)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Validates `payload` against the tenant's field definitions, resolving taxonomy references.
///
/// The definitions are loaded rather than passed in, so a caller cannot validate against a stale set —
/// a field that gained `required` between a form render and its submission must be enforced as it is
/// now, not as the client last saw it.
pub async fn validate<'e, E>(
    executor: E,
    payload: &Map<String, Value>,
    mode: Mode,
    writer: Writer,
) -> Result<Accepted, ValidationOutcome>
where
    E: sqlx::PgExecutor<'e> + Copy,
{
    let defs = load(executor).await.map_err(ValidationOutcome::Failed)?;
    // No stored context: this entry point has no asset, so a dependent field is judged on the payload alone.
    // Every asset-scoped write goes through `validate_for_on`, which loads what is already there.
    let accepted = dam_core::fields::validate(&defs, payload, mode, writer, &Map::new())
        .map_err(ValidationOutcome::Rejected)?;

    let rejections = check_taxonomy_refs(executor, &accepted.taxonomy_refs)
        .await
        .map_err(ValidationOutcome::Failed)?;
    if rejections.is_empty() {
        Ok(accepted)
    } else {
        Err(ValidationOutcome::Rejected(rejections))
    }
}

/// The same validation, on a connection rather than a pool.
///
/// [`validate`] requires `E: Copy` because it uses the executor twice, and `&mut PgConnection` is not `Copy`
/// — so a handler working inside a [`crate::TenantConn`] cannot call it. That handler is exactly the one that
/// must: reading the asset, validating, and writing have to be one transaction, or a concurrent edit lands
/// between them and the loser's merge is computed against a document that no longer exists, silently
/// reverting the winner rather than conflicting with it.
///
/// The two functions do the same three steps in the same order. That duplication is deliberate — the
/// alternative is a generic over "executor or connection" that both callers have to satisfy, which is more
/// machinery than six lines of body is worth.
pub async fn validate_on(
    conn: &mut sqlx::PgConnection,
    payload: &Map<String, Value>,
    mode: Mode,
    writer: Writer,
) -> Result<Accepted, ValidationOutcome> {
    validate_for_on(conn, None, payload, mode, writer).await
}

/// Validation scoped to one asset's metadata type (Q.1).
///
/// With `for_asset` supplied, the definitions are the ones that asset's type includes rather than the whole
/// tenant vocabulary. That is what makes a type mean anything: a field the asset's form does not show is a
/// field a write must not accept, or the type is decoration and the value lands somewhere no form will ever
/// display it again.
///
/// The refusal a caller sees for such a key is `unknown_field`, deliberately the same one an actual typo gets.
/// The alternative — a distinct "not in this type" code — would disclose the rest of the tenant's schema to a
/// caller holding one asset, and the fix is the same either way: send a key this asset's form offers.
///
/// `None` means the whole vocabulary, which is what creation before a type is chosen needs.
pub async fn validate_for_on(
    conn: &mut sqlx::PgConnection,
    for_asset: Option<Uuid>,
    payload: &Map<String, Value>,
    mode: Mode,
    writer: Writer,
) -> Result<Accepted, ValidationOutcome> {
    let defs = match for_asset {
        Some(asset_id) => crate::metadata_types::fields_for_on(&mut *conn, asset_id)
            .await
            .map_err(|refusal| match refusal {
                crate::metadata_types::TypeRefusal::Database(error) => {
                    ValidationOutcome::Failed(error)
                }
                // Every other variant is about editing a type, and none of them can arise from a read.
                other => ValidationOutcome::Failed(Error::Migrate(other.to_string())),
            })?,
        None => load(&mut *conn).await.map_err(ValidationOutcome::Failed)?,
    };
    // What the asset already carries, so a dependent field's condition is judged on the document as it will
    // be rather than on the patch alone (Q.19b). An edit that fills in a child field without restating its
    // parent is the ordinary shape of an edit, and judging the payload alone would refuse it.
    let stored: Map<String, Value> = match for_asset {
        Some(asset_id) => sqlx::query_scalar::<_, Value>(
            "SELECT coalesce(values, '{}'::jsonb) FROM asset_metadata WHERE asset_id = $1",
        )
        .bind(asset_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|error| ValidationOutcome::Failed(Error::from(error)))?
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default(),
        None => Map::new(),
    };
    let accepted = dam_core::fields::validate(&defs, payload, mode, writer, &stored)
        .map_err(ValidationOutcome::Rejected)?;

    let rejections = check_taxonomy_refs(&mut *conn, &accepted.taxonomy_refs)
        .await
        .map_err(ValidationOutcome::Failed)?;
    if rejections.is_empty() {
        Ok(accepted)
    } else {
        Err(ValidationOutcome::Rejected(rejections))
    }
}

/// Either the payload was refused, or the check itself could not be completed.
///
/// Separate variants because they are different answers to the caller: a rejection is a `400` naming
/// what to fix, and a failure is a `500` naming nothing. Collapsing them would report a database outage
/// as the user's mistake.
#[derive(Debug)]
pub enum ValidationOutcome {
    Rejected(Vec<Rejection>),
    Failed(Error),
}

impl std::fmt::Display for ValidationOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(rejections) => {
                write!(f, "{} field(s) refused: ", rejections.len())?;
                let summary: Vec<String> = rejections
                    .iter()
                    .map(|r| format!("{}={}", r.key, r.code))
                    .collect();
                f.write_str(&summary.join(", "))
            }
            Self::Failed(error) => write!(f, "validation could not be completed: {error}"),
        }
    }
}

impl std::error::Error for ValidationOutcome {}

/// Refuses a dependency that cannot be evaluated (Q.19b).
///
/// Three ways it cannot be: naming itself, naming a field that does not exist, or naming a field that is
/// itself dependent. The last is the one-level rule, and it is enforced here rather than tolerated because a
/// chain turns "is this field applicable" into a graph walk with cycles in it — and the failure of a cycle is
/// a form that renders nothing, with no error to read.
async fn check_dependency(
    conn: &mut sqlx::PgConnection,
    key: &str,
    validation: &Value,
) -> Result<(), SchemaRefusal> {
    let Some(dependency) = dam_core::fields::Constraints::from_json(validation).depends_on else {
        return Ok(());
    };
    if dependency.key == key {
        return Err(SchemaRefusal::SelfDependency(key.to_owned()));
    }
    let parent: Option<Value> =
        sqlx::query_scalar("SELECT validation FROM field_defs WHERE key = $1")
            .bind(&dependency.key)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|error| SchemaRefusal::Database(Error::from(error)))?;
    let Some(parent_validation) = parent else {
        return Err(SchemaRefusal::UnknownParent(dependency.key));
    };
    if dam_core::fields::Constraints::from_json(&parent_validation)
        .depends_on
        .is_some()
    {
        return Err(SchemaRefusal::DependencyChain(dependency.key));
    }
    Ok(())
}

/// Confirms every referenced term exists and belongs to its field's taxonomy.
///
/// The check TASKS.md names for this task, and the reason it matters is not tidiness: a term from
/// another taxonomy would index and facet under the wrong vocabulary, so "all assets in Outdoor" would
/// quietly return assets nobody put there.
async fn check_taxonomy_refs<'e, E>(
    executor: E,
    refs: &[TaxonomyRef],
) -> Result<Vec<Rejection>, Error>
where
    E: sqlx::PgExecutor<'e>,
{
    if refs.is_empty() {
        return Ok(Vec::new());
    }

    // One query for all of them. `ANY($1)` over a deduplicated array rather than a query per reference:
    // twenty category terms on one asset is an ordinary shape, and twenty round trips per write is not.
    let mut wanted: Vec<Uuid> = refs.iter().map(|r| r.term_id).collect();
    wanted.sort_unstable();
    wanted.dedup();

    let rows = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT id, taxonomy_id FROM taxonomy_terms WHERE id = ANY($1)",
    )
    .bind(&wanted)
    .fetch_all(executor)
    .await?;
    let found: HashMap<Uuid, Uuid> = rows.into_iter().collect();

    let mut rejections = Vec::new();
    for reference in refs {
        match found.get(&reference.term_id) {
            None => rejections.push(Rejection {
                key: reference.key.clone(),
                code: "term_not_found",
                detail: format!("no taxonomy term {}", reference.term_id),
            }),
            Some(actual) if *actual != reference.taxonomy_id => rejections.push(Rejection {
                key: reference.key.clone(),
                code: "wrong_taxonomy",
                // Both ids are named: "wrong taxonomy" without them leaves the caller guessing which
                // of their vocabularies the term actually came from.
                detail: format!(
                    "term {} belongs to taxonomy {actual}, but this field is bound to {}",
                    reference.term_id, reference.taxonomy_id
                ),
            }),
            Some(_) => {}
        }
    }
    Ok(rejections)
}

// ---------------------------------------------------------------------------------------------------
// Schema administration (F.11b·2)
// ---------------------------------------------------------------------------------------------------
//
// Editing `field_defs` is the one write in the system whose blast radius is *every other subsystem*: the
// validator refuses payloads against these rows, the search renderer decides textual-ness from them, the
// facet counter enumerates them, and the metadata form is drawn from them. Which is why the refusals here
// are the interesting part of the module and the SQL is the boring part.
//
// The rule they all serve: **an edit must never leave stored values and the definition describing them out
// of step**, because nothing downstream can detect that state. Validation happens on write, so a value
// stored under a definition that has since changed shape is never re-checked — it simply sits there,
// describing itself with a rule it does not satisfy.

/// Why a schema edit was refused.
///
/// Named refusals rather than a driver error, because every one of these reaches an administrator in a
/// form and each has a different fix. "duplicate key value violates unique constraint
/// field_defs_alias_idx" is not a sentence anybody can act on.
#[derive(Debug, thiserror::Error)]
pub enum SchemaRefusal {
    #[error("no field is defined with the key `{0}`")]
    UnknownField(String),

    #[error("a field with the key `{0}` is already defined")]
    DuplicateKey(String),

    #[error("the search alias `{0}` already belongs to another field")]
    DuplicateAlias(String),

    #[error("`{key}` is not usable as a field key: {detail}")]
    BadKey { key: String, detail: &'static str },

    #[error("`{0}` is reserved and cannot be a field key")]
    ReservedKey(String),

    #[error("`{0}` is not a field kind this build knows")]
    UnknownKind(String),

    #[error("a taxonomy field needs the taxonomy it is bound to")]
    TaxonomyRequired,

    #[error("`{0}` cannot depend on itself")]
    SelfDependency(String),

    #[error("no field `{0}` to depend on")]
    UnknownParent(String),

    #[error(
        "`{0}` is itself dependent, and a dependency is one level: a chain needs cycle detection, an \
         evaluation order, and an answer to what a field two hops from a cleared parent means"
    )]
    DependencyChain(String),

    #[error("no taxonomy {0} exists")]
    UnknownTaxonomy(Uuid),

    /// The kind cannot change because assets already carry values validated under the old one.
    #[error(
        "`{key}` cannot change kind: {assets} asset(s) already carry a value stored under the current kind"
    )]
    KindLockedByValues { key: String, assets: i64 },

    /// A reorder must name every field exactly once.
    #[error("a reorder must list all {expected} fields exactly once; got {given}")]
    IncompleteOrder { expected: usize, given: usize },

    #[error(transparent)]
    Database(#[from] crate::Error),
}

/// Keys this build uses for something else, so a definition may not claim them.
///
/// Two families. `values`, `asset_id` and friends are names the generated SQL and the index schema already
/// use — a definition with one of these produces an expression or a document field that means two things.
/// The rest are the shorthand parser's own vocabulary.
const RESERVED_KEYS: &[&str] = &[
    "values",
    "asset_id",
    "group_ids",
    "deleted",
    "text",
    "metadata",
    "and",
    "or",
    "not",
];

/// The longest a key may be, so it stays a legible selector and fits an index entry comfortably.
const MAX_KEY_BYTES: usize = 63;

/// Checks a proposed key against every layer that will have to spell it.
///
/// A field key is simultaneously a JSONB member name, a token in the search shorthand, and part of a
/// generated SQL path expression. Each of those has its own escaping story, and a key needing escaping in
/// any of them is a bug waiting for the one input that triggers it. So the rule is enforced once, here, and
/// the permitted shape is the intersection: lower-case ASCII, digits and underscores, starting with a
/// letter.
pub fn check_key(key: &str) -> Result<(), SchemaRefusal> {
    let bad = |detail: &'static str| {
        Err(SchemaRefusal::BadKey {
            key: key.to_owned(),
            detail,
        })
    };
    if key.is_empty() {
        return bad("a key cannot be empty");
    }
    if key.len() > MAX_KEY_BYTES {
        return bad("a key must be 63 bytes or fewer");
    }
    if !key.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
        return bad("a key must start with a lower-case ASCII letter");
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return bad("a key may contain only lower-case ASCII letters, digits and underscores");
    }
    if RESERVED_KEYS.contains(&key) {
        return Err(SchemaRefusal::ReservedKey(key.to_owned()));
    }
    Ok(())
}

/// A field to define.
///
/// Every flag is explicit rather than defaulted, because the defaults are exactly the decisions an
/// administrator is making: whether the field is searchable is not something to inherit silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewField {
    pub key: String,
    pub label: String,
    /// The database spelling — parsed through [`FieldKind::parse`] so an unknown one is refused here
    /// rather than taking out `load` for the whole tenant later.
    pub kind: String,
    pub taxonomy_id: Option<Uuid>,
    pub multivalued: bool,
    pub required: bool,
    pub read_only: bool,
    pub searchable: bool,
    pub facetable: bool,
    pub ai_writable: bool,
    pub search_alias: Option<String>,
    pub validation: Value,
}

/// What may be amended, with `None` meaning "leave alone".
///
/// There is deliberately no `key`. A key is the JSONB member name every stored value already sits under;
/// renaming the definition in place would orphan all of them at once, invisibly. The supported path is
/// define-new, backfill, remove-old — three steps, each individually reversible.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Amendment {
    pub label: Option<String>,
    pub kind: Option<String>,
    pub taxonomy_id: Option<Option<Uuid>>,
    pub multivalued: Option<bool>,
    pub required: Option<bool>,
    pub read_only: Option<bool>,
    pub searchable: Option<bool>,
    pub facetable: Option<bool>,
    pub ai_writable: Option<bool>,
    /// Doubly wrapped: the outer `None` leaves the alias alone, the inner clears it.
    pub search_alias: Option<Option<String>>,
    pub validation: Option<Value>,
}

/// The result of an amendment, with the consequences the caller cannot compute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Amended {
    pub field: Catalogued,
    /// Whether what the index holds has changed, so search is now stale until a rebuild.
    pub reindex_required: bool,
    /// How many assets would now fail a metadata write because they lack a newly-required value.
    ///
    /// Reported rather than refused: requiredness is a forward-looking rule, and refusing it would make
    /// the schema unfixable on a library that predates the rule. But an administrator who has just made
    /// forty thousand assets unsaveable should learn it here, not one 422 at a time.
    pub assets_now_incomplete: i64,
}

/// The result of a removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub key: String,
    /// How many assets still carry a value under this key — kept, not deleted. See [`remove`].
    pub assets_with_values: i64,
    pub reindex_required: bool,
}

/// Defines a new field.
pub async fn define(pool: &sqlx::PgPool, spec: NewField) -> Result<Catalogued, SchemaRefusal> {
    let mut conn = pool.acquire().await.map_err(crate::Error::from)?;
    define_on(&mut conn, spec).await
}

/// [`define`] on a caller's connection, so it joins a tenant-scoped transaction.
pub async fn define_on(
    conn: &mut sqlx::PgConnection,
    spec: NewField,
) -> Result<Catalogued, SchemaRefusal> {
    check_key(&spec.key)?;
    let kind =
        FieldKind::parse(&spec.kind).map_err(|_| SchemaRefusal::UnknownKind(spec.kind.clone()))?;
    check_taxonomy(&mut *conn, kind, spec.taxonomy_id).await?;
    check_dependency(&mut *conn, &spec.key, &spec.validation).await?;

    // Checked rather than left to the unique index, so the refusal names which of the two collided: a
    // driver error on a two-column insert cannot tell an administrator whether to change the key or the
    // alias.
    if exists(&mut *conn, &spec.key).await? {
        return Err(SchemaRefusal::DuplicateKey(spec.key));
    }
    if let Some(alias) = &spec.search_alias
        && alias_taken(&mut *conn, alias, None).await?
    {
        return Err(SchemaRefusal::DuplicateAlias(alias.clone()));
    }

    // Appended, not inserted: a new field lands at the end of the form, where it is visible as new.
    sqlx::query(
        "INSERT INTO field_defs \
             (id, key, label, kind, taxonomy_id, multivalued, required, read_only, searchable, \
              facetable, ai_writable, search_alias, validation, display_order) \
         VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, \
                 coalesce((SELECT max(display_order) + 1 FROM field_defs), 0))",
    )
    .bind(&spec.key)
    .bind(&spec.label)
    .bind(&spec.kind)
    .bind(spec.taxonomy_id)
    .bind(spec.multivalued)
    .bind(spec.required)
    .bind(spec.read_only)
    .bind(spec.searchable)
    .bind(spec.facetable)
    .bind(spec.ai_writable)
    .bind(spec.search_alias.as_deref())
    .bind(&spec.validation)
    .execute(&mut *conn)
    .await
    .map_err(crate::Error::from)?;

    one(&mut *conn, &spec.key).await
}

/// Amends an existing field.
pub async fn amend(
    pool: &sqlx::PgPool,
    key: &str,
    change: Amendment,
) -> Result<Amended, SchemaRefusal> {
    let mut conn = pool.acquire().await.map_err(crate::Error::from)?;
    amend_on(&mut conn, key, change).await
}

/// [`amend`] on a caller's connection.
pub async fn amend_on(
    conn: &mut sqlx::PgConnection,
    key: &str,
    change: Amendment,
) -> Result<Amended, SchemaRefusal> {
    let before = one(&mut *conn, key).await?;

    // An amendment can introduce a dependency, so it gets the same three checks a definition does (Q.19b).
    if let Some(validation) = &change.validation {
        check_dependency(&mut *conn, key, validation).await?;
    }

    let kind = match &change.kind {
        Some(raw) => {
            let parsed =
                FieldKind::parse(raw).map_err(|_| SchemaRefusal::UnknownKind(raw.clone()))?;
            // The one refusal that exists purely to protect stored data: a kind change re-describes every
            // value already written under the old kind, and no later read re-validates them.
            if *raw != before.kind {
                // `stored_anywhere`, not `usage`: a soft-deleted asset keeps its values, and a restore
                // would bring them back under a kind they were never validated against. The lock has to
                // hold for anything recoverable, not just what is currently visible.
                let assets = stored_anywhere(&mut *conn, key).await?;
                if assets > 0 {
                    return Err(SchemaRefusal::KindLockedByValues {
                        key: key.to_owned(),
                        assets,
                    });
                }
            }
            Some(parsed)
        }
        None => None,
    };

    let taxonomy_id = change.taxonomy_id.unwrap_or(before.taxonomy_id);
    let effective_kind = kind.unwrap_or(
        FieldKind::parse(&before.kind)
            .map_err(|_| SchemaRefusal::UnknownKind(before.kind.clone()))?,
    );
    check_taxonomy(&mut *conn, effective_kind, taxonomy_id).await?;

    if let Some(Some(alias)) = &change.search_alias
        && alias_taken(&mut *conn, alias, Some(key)).await?
    {
        return Err(SchemaRefusal::DuplicateAlias(alias.clone()));
    }

    // `coalesce($n, column)` per field: one statement, and an omitted member means "unchanged" without a
    // read-modify-write that could lose a concurrent edit to a different member.
    sqlx::query(
        "UPDATE field_defs SET \
             label = coalesce($2, label), \
             kind = coalesce($3, kind), \
             taxonomy_id = $4, \
             multivalued = coalesce($5, multivalued), \
             required = coalesce($6, required), \
             read_only = coalesce($7, read_only), \
             searchable = coalesce($8, searchable), \
             facetable = coalesce($9, facetable), \
             ai_writable = coalesce($10, ai_writable), \
             search_alias = CASE WHEN $11 THEN $12 ELSE search_alias END, \
             validation = coalesce($13, validation), \
             updated_at = now() \
         WHERE key = $1",
    )
    .bind(key)
    .bind(change.label.as_deref())
    .bind(change.kind.as_deref())
    .bind(taxonomy_id)
    .bind(change.multivalued)
    .bind(change.required)
    .bind(change.read_only)
    .bind(change.searchable)
    .bind(change.facetable)
    .bind(change.ai_writable)
    .bind(change.search_alias.is_some())
    .bind(change.search_alias.clone().flatten())
    .bind(change.validation.as_ref())
    .execute(&mut *conn)
    .await
    .map_err(crate::Error::from)?;

    let field = one(&mut *conn, key).await?;

    // Newly required, and only newly: re-reporting the count on every unrelated edit would make the number
    // look like a consequence of that edit.
    let assets_now_incomplete = if field.required && !before.required {
        missing(&mut *conn, key).await?
    } else {
        0
    };

    Ok(Amended {
        reindex_required: changes_the_index(&before, &field),
        assets_now_incomplete,
        field,
    })
}

/// Removes a definition, leaving every stored value exactly where it is.
///
/// The values are deliberately kept. Deleting them would make a mis-clicked removal on a large tenant a
/// data-loss event with no undo, whereas leaving them makes the removal reversible: re-defining the same
/// key with the same kind brings the data back, unedited. The cost is JSONB members no definition
/// describes, which every reader already ignores — the validator refuses unknown keys on *write*, and the
/// catalogue is what forms and facets enumerate.
///
/// The count of assets still carrying values is returned so the caller can say what is about to go dark.
pub async fn remove(pool: &sqlx::PgPool, key: &str) -> Result<Removed, SchemaRefusal> {
    let mut conn = pool.acquire().await.map_err(crate::Error::from)?;
    remove_on(&mut conn, key).await
}

/// [`remove`] on a caller's connection.
pub async fn remove_on(conn: &mut sqlx::PgConnection, key: &str) -> Result<Removed, SchemaRefusal> {
    let before = one(&mut *conn, key).await?;
    let assets_with_values = usage(&mut *conn, key).await?;

    sqlx::query("DELETE FROM field_defs WHERE key = $1")
        .bind(key)
        .execute(&mut *conn)
        .await
        .map_err(crate::Error::from)?;

    Ok(Removed {
        key: before.key,
        assets_with_values,
        // A field that was in the index is now gone from the schema's view of the tenant, so documents
        // still carrying it are stale until rebuilt.
        reindex_required: before.searchable || before.facetable,
    })
}

/// Sets the form's field order.
///
/// `keys` must name every field exactly once. A partial list is refused rather than applied to the fields
/// it names: display order is a total order, so a client sending half the schema has a stale copy — and
/// applying it would reposition fields whose current place that client never showed anybody.
pub async fn reorder(pool: &sqlx::PgPool, keys: &[String]) -> Result<(), SchemaRefusal> {
    let mut conn = pool.acquire().await.map_err(crate::Error::from)?;
    reorder_on(&mut conn, keys).await
}

/// [`reorder`] on a caller's connection.
pub async fn reorder_on(
    conn: &mut sqlx::PgConnection,
    keys: &[String],
) -> Result<(), SchemaRefusal> {
    let existing: Vec<String> = sqlx::query_scalar("SELECT key FROM field_defs")
        .fetch_all(&mut *conn)
        .await
        .map_err(crate::Error::from)?;

    let given: std::collections::BTreeSet<&str> = keys.iter().map(String::as_str).collect();
    if given.len() != keys.len() || given.len() != existing.len() {
        return Err(SchemaRefusal::IncompleteOrder {
            expected: existing.len(),
            given: keys.len(),
        });
    }
    if let Some(unknown) = existing.iter().find(|key| !given.contains(key.as_str())) {
        return Err(SchemaRefusal::UnknownField(unknown.clone()));
    }

    // One statement over the whole list: a loop of UPDATEs would leave the form in an order nobody chose
    // if it failed halfway, and `display_order` has no uniqueness to protect it from the intermediate
    // states a loop passes through.
    sqlx::query(
        "UPDATE field_defs SET display_order = position.ord - 1, updated_at = now() \
         FROM unnest($1::text[]) WITH ORDINALITY AS position(key, ord) \
         WHERE field_defs.key = position.key",
    )
    .bind(keys)
    .execute(&mut *conn)
    .await
    .map_err(crate::Error::from)?;
    Ok(())
}

/// How many live assets carry a value under `key`.
///
/// The number an administrator needs before changing or removing a definition, and the one they cannot
/// get from the schema itself.
pub async fn usage<'e, E>(executor: E, key: &str) -> Result<i64, SchemaRefusal>
where
    E: sqlx::PgExecutor<'e>,
{
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM asset_metadata m \
         JOIN assets a ON a.id = m.asset_id AND a.deleted_at IS NULL \
         WHERE m.values ? $1 AND m.values -> $1 <> 'null'::jsonb",
    )
    .bind(key)
    .fetch_one(executor)
    .await
    .map_err(crate::Error::from)?;
    Ok(count)
}

/// How many assets carry a value under `key`, soft-deleted ones included.
///
/// Separate from [`usage`] because the two answer different questions. `usage` is what an administrator is
/// shown — the live library. This one guards the kind lock, where a soft-deleted asset counts: its JSONB
/// values are untouched by the delete, so restoring it after a kind change would produce exactly the
/// mismatch the lock exists to prevent.
async fn stored_anywhere(conn: &mut sqlx::PgConnection, key: &str) -> Result<i64, SchemaRefusal> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM asset_metadata \
         WHERE values ? $1 AND values -> $1 <> 'null'::jsonb",
    )
    .bind(key)
    .fetch_one(&mut *conn)
    .await
    .map_err(crate::Error::from)?;
    Ok(count)
}

/// How many live assets have no value under `key` — what turning `required` on would break.
async fn missing(conn: &mut sqlx::PgConnection, key: &str) -> Result<i64, SchemaRefusal> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assets a \
         LEFT JOIN asset_metadata m ON m.asset_id = a.id \
         WHERE a.deleted_at IS NULL \
           AND (m.values IS NULL OR NOT (m.values ? $1) OR m.values -> $1 = 'null'::jsonb)",
    )
    .bind(key)
    .fetch_one(&mut *conn)
    .await
    .map_err(crate::Error::from)?;
    Ok(count)
}

/// Whether the difference between two definitions changes what the index holds.
///
/// Three properties do. `searchable` and `facetable` decide whether the value is written at all; the kind
/// decides whether it joins the text blob, because `is_textual` is what the writer consults. A label, a
/// constraint or a requiredness flag never reaches a document.
fn changes_the_index(before: &Catalogued, after: &Catalogued) -> bool {
    before.kind != after.kind
        || before.facetable != after.facetable
        || before.searchable != after.searchable
}

/// One field's catalogue row, or [`SchemaRefusal::UnknownField`].
async fn one(conn: &mut sqlx::PgConnection, key: &str) -> Result<Catalogued, SchemaRefusal> {
    catalog(&mut *conn)
        .await
        .map_err(SchemaRefusal::Database)?
        .into_iter()
        .find(|def| def.key == key)
        .ok_or_else(|| SchemaRefusal::UnknownField(key.to_owned()))
}

async fn exists(conn: &mut sqlx::PgConnection, key: &str) -> Result<bool, SchemaRefusal> {
    let found: Option<i32> = sqlx::query_scalar("SELECT 1 FROM field_defs WHERE key = $1")
        .bind(key)
        .fetch_optional(&mut *conn)
        .await
        .map_err(crate::Error::from)?;
    Ok(found.is_some())
}

/// Whether `alias` belongs to a field other than `excluding`.
async fn alias_taken(
    conn: &mut sqlx::PgConnection,
    alias: &str,
    excluding: Option<&str>,
) -> Result<bool, SchemaRefusal> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM field_defs WHERE search_alias = $1 AND ($2::text IS NULL OR key <> $2)",
    )
    .bind(alias)
    .bind(excluding)
    .fetch_optional(&mut *conn)
    .await
    .map_err(crate::Error::from)?;
    Ok(found.is_some())
}

/// A taxonomy-bound kind needs a taxonomy, and it has to exist.
///
/// Two refusals rather than one, because the fixes differ: supply a taxonomy, versus pick one that exists.
async fn check_taxonomy(
    conn: &mut sqlx::PgConnection,
    kind: FieldKind,
    taxonomy_id: Option<Uuid>,
) -> Result<(), SchemaRefusal> {
    if kind != FieldKind::TaxonomyRef {
        return Ok(());
    }
    let Some(id) = taxonomy_id else {
        return Err(SchemaRefusal::TaxonomyRequired);
    };
    let found: Option<i32> = sqlx::query_scalar("SELECT 1 FROM taxonomies WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(crate::Error::from)?;
    if found.is_none() {
        return Err(SchemaRefusal::UnknownTaxonomy(id));
    }
    Ok(())
}
