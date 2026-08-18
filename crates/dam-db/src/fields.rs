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
            serde_json::Value,
        ),
    >(
        "SELECT key, kind, taxonomy_id, multivalued, required, read_only, ai_writable, validation \
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
                    constraints: Constraints::from_json(&validation),
                })
            },
        )
        .collect()
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
    let accepted = dam_core::fields::validate(&defs, payload, mode, writer)
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
