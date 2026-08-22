//! Relevance judgements, for the search eval harness (2.9, GAPS G8).
//!
//! `dam_core::eval` computes nDCG and MRR. This loads the judgements they score against, so a ranking change
//! **reports its effect instead of being argued about**.
//!
//! ## The corpus is per tenant and that is a feature
//!
//! `relevance_judgements` lives in the tenant schema, so a customer's own judgements measure their own library.
//! A shared fixture corpus would measure how well the ranking works on somebody else's vocabulary, which is the
//! question nobody is asking.

use crate::Error;
use dam_core::eval::Judgements;
use uuid::Uuid;

/// Records or updates a judgement.
///
/// `ON CONFLICT` updates, because judging is iterative: a reviewer revisits a query and changes their mind, and
/// the second opinion is the one that counts. A conflict error would make the UI's obvious action fail.
pub async fn record(
    pool: &sqlx::PgPool,
    query_text: &str,
    asset_id: Uuid,
    grade: u8,
    judged_by: Option<Uuid>,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO relevance_judgements (id, query_text, asset_id, grade, judged_by) \
         VALUES (gen_random_uuid(), $1, $2, $3, $4) \
         ON CONFLICT (query_text, asset_id) DO UPDATE SET \
             grade = excluded.grade, judged_by = excluded.judged_by, judged_at = now()",
    )
    .bind(query_text)
    .bind(asset_id)
    .bind(i16::from(grade))
    .bind(judged_by)
    .execute(pool)
    .await?;
    Ok(())
}

/// Every judged query, with its grades.
///
/// Loaded whole rather than per query, because an eval run scores all of them and one round trip per query turns
/// a hundred-query corpus into a hundred queries against the database before any searching happens.
pub async fn corpus(pool: &sqlx::PgPool) -> Result<Vec<Judgements>, Error> {
    let rows = sqlx::query_as::<_, (String, Uuid, i16)>(
        "SELECT j.query_text, j.asset_id, j.grade FROM relevance_judgements j \
         JOIN assets a ON a.id = j.asset_id \
         WHERE a.deleted_at IS NULL \
         ORDER BY j.query_text, j.asset_id",
    )
    .fetch_all(pool)
    .await?;

    // Grouped in Rust rather than with a `jsonb_object_agg`, so the shape the metrics take is built once and the
    // SQL stays something a reviewer can read.
    let mut corpus: Vec<Judgements> = Vec::new();
    for (query_text, asset_id, grade) in rows {
        let grade = u8::try_from(grade.max(0)).unwrap_or(0);
        match corpus.last_mut() {
            // The ORDER BY groups them, so only the last entry can match.
            Some(current) if current.query_text == query_text => {
                current.grades.insert(asset_id, grade);
            }
            _ => {
                corpus.push(Judgements::new(query_text).with(asset_id, grade));
            }
        }
    }
    Ok(corpus)
}

/// Judgements for one query.
pub async fn for_query(pool: &sqlx::PgPool, query_text: &str) -> Result<Option<Judgements>, Error> {
    let rows = sqlx::query_as::<_, (Uuid, i16)>(
        "SELECT j.asset_id, j.grade FROM relevance_judgements j \
         JOIN assets a ON a.id = j.asset_id \
         WHERE j.query_text = $1 AND a.deleted_at IS NULL",
    )
    .bind(query_text)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        // No judgements is `None`, not an empty set. An empty `Judgements` would be *scoreable* — see
        // `dam_core::eval` — and would report every ranking as perfect.
        return Ok(None);
    }
    let mut judged = Judgements::new(query_text);
    for (asset_id, grade) in rows {
        judged
            .grades
            .insert(asset_id, u8::try_from(grade.max(0)).unwrap_or(0));
    }
    Ok(Some(judged))
}

/// Removes a judgement.
pub async fn remove(pool: &sqlx::PgPool, query_text: &str, asset_id: Uuid) -> Result<bool, Error> {
    let deleted =
        sqlx::query("DELETE FROM relevance_judgements WHERE query_text = $1 AND asset_id = $2")
            .bind(query_text)
            .bind(asset_id)
            .execute(pool)
            .await?
            .rows_affected();
    Ok(deleted > 0)
}
