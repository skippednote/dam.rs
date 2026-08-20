//! Describing a library that already exists (M5c).
//!
//! §8.3: "all library backfill runs here, never synchronously". Two stages, because a batch takes up to
//! twenty-four hours and nothing about it can be held in a process:
//!
//! - [`submit`] takes the next slice of undescribed assets, opens a run row for each, and posts one batch.
//! - [`collect`] polls that batch and, once it has ended, writes every result through the same code the
//!   synchronous stage uses.
//!
//! ## The run rows are the state, and there is no batch table
//!
//! Everything a collector needs is already on `enrichment_runs`: `llm_batch_id` says which batch a run belongs
//! to, `llm_custom_id` says what its answer will be called, and `state = 'running'` says it is still open. So a
//! worker that dies mid-batch loses nothing — the collect job comes back when its lease lapses, and the rows are
//! where it left them. A separate batch table would be a second thing to keep true.
//!
//! ## The image is read at submission, not at collection
//!
//! A batch carries the bytes. That is a lot of upload for a large slice, and the alternative is worse: reading
//! the proxy hours later means a re-render or a tiering change between the two halves would describe bytes that
//! are no longer the asset's, and nothing would say so.
//!
//! ## What a batch cannot do
//!
//! The OpenAI-compatible family batches through a file upload and a different polling shape, so [`submit`]
//! refuses for those credentials rather than pretending. A tenant on one of those keeps the synchronous path,
//! which is correct and dearer — and the refusal says which, because "nothing happened" is the worst answer.

use crate::enrich::{AiContext, Writeback};
use crate::{Error, Result};
use base64::Engine as _;
use dam_ai::batch::{AnthropicBatch, BatchItem, BatchOutcome};
use dam_ai::enrich::{Brief, TermOffer};
use dam_ai::model::{ModelError, Part};
use dam_db::TenantConn;
use dam_db::ai_credentials::Provider;
use dam_db::enrichment::{self, Cost, Outcome};
use dam_db::quotas;
use dam_store::{BlobStore, Key};
use std::sync::Arc;
use uuid::Uuid;

/// How many assets one batch carries by default.
///
/// Far below the provider's 100,000 limit, and deliberately: each request carries an image, so a slice is also
/// an upload — and a smaller batch means the first descriptions land in hours rather than at the end. A backfill
/// is a sequence of these, driven by the queue.
pub const DEFAULT_SLICE: i64 = 200;

/// What a submission did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submitted {
    /// A batch is in flight.
    Batch {
        batch_id: String,
        /// How many assets went into it.
        count: usize,
    },
    /// Nothing to do, and why.
    Nothing(String),
}

/// What a collection found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Collected {
    /// Still working. Carries how many requests the provider says are done, for a progress display.
    Waiting { finished: u64, total: u64 },
    /// Ended and applied.
    Applied {
        wrote: usize,
        declined: usize,
        errored: usize,
        expired: usize,
        micro_cents: i64,
    },
}

/// Submits the next slice of undescribed assets as one batch.
pub async fn submit(
    global: &sqlx::PgPool,
    store: &dyn BlobStore,
    ai: &AiContext,
    slug: &dam_core::TenantSlug,
    tenant_id: Uuid,
    slice: i64,
) -> Result<Submitted> {
    let mut conn = TenantConn::begin(global, slug).await?;
    let settings = enrichment::settings(conn.executor()).await?;
    let credential = dam_db::ai_credentials::current(conn.executor()).await?;
    // The total is not needed at submission: what a run records about a truncated vocabulary is written when the
    // answer is applied, and by then the taxonomy may have changed anyway.
    let (vocabulary, _) =
        enrichment::vocabulary(conn.executor(), crate::enrich::VOCABULARY_LIMIT).await?;
    let candidates = enrichment::needing_description(
        conn.executor(),
        dam_ai::enrich::PIPELINE,
        dam_ai::enrich::PIPELINE_VERSION,
        slice,
    )
    .await?;
    conn.commit().await?;

    if !settings.is_enabled {
        return Ok(Submitted::Nothing(
            "enrichment is switched off for this tenant".to_owned(),
        ));
    }
    let Some(credential) = credential else {
        return Ok(Submitted::Nothing(
            "no model credential is configured".to_owned(),
        ));
    };
    if credential.provider() != Some(Provider::Anthropic) {
        // Refused rather than silently falling back: a tenant who asked for a backfill and got a synchronous run
        // at twice the price would find out from the invoice.
        return Ok(Submitted::Nothing(format!(
            "batch backfill needs an Anthropic credential; `{}` speaks a different batch protocol, so this \
             library would have to be described one asset at a time and at full price",
            credential.provider
        )));
    }

    // Before the batch, because the whole batch is one commitment: a hard cap reached halfway through is not a
    // thing the provider offers.
    let period = quotas::month_start(chrono::Utc::now());
    let verdict = {
        let mut global_conn = global.acquire().await.map_err(dam_db::Error::from)?;
        quotas::check(&mut global_conn, tenant_id, quotas::AI_SPEND, period).await?
    };
    if !verdict.allowed() {
        return Ok(Submitted::Nothing(
            "the tenant is over its hard AI spend cap for this month".to_owned(),
        ));
    }

    let describable: Vec<enrichment::Candidate> = candidates
        .into_iter()
        .filter(|candidate| crate::enrich::describable(&candidate.proxy_mime))
        .collect();
    if describable.is_empty() {
        return Ok(Submitted::Nothing(
            "every asset with a describable proxy already has a description".to_owned(),
        ));
    }

    let brief = Brief {
        guidance: settings.guidance.clone(),
        vocabulary: vocabulary
            .into_iter()
            .map(|(slug, label, synonyms)| TermOffer {
                slug,
                label,
                synonyms,
            })
            .collect(),
        language: settings.language.clone(),
    };

    // One run row per asset, opened before the batch is posted. The custom_id is the run id: results come back
    // unordered, and the id has to be something a later process can resolve on its own.
    let mut items = Vec::with_capacity(describable.len());
    let mut run_ids = Vec::with_capacity(describable.len());
    for candidate in &describable {
        let key = Key::new(candidate.object_key.clone())?;
        let bytes = store.get(&key, None).await?.into_bytes(&key)?;

        let mut conn = TenantConn::begin(global, slug).await?;
        let run_id = enrichment::start_run(
            conn.executor(),
            candidate.asset_id,
            dam_ai::enrich::PIPELINE,
            dam_ai::enrich::PIPELINE_VERSION,
        )
        .await?;
        enrichment::mark_batched(conn.executor(), run_id, &run_id.to_string()).await?;
        conn.commit().await?;

        items.push(BatchItem {
            custom_id: run_id.to_string(),
            ask: dam_ai::enrich::ask_for(
                &brief,
                Part::Image {
                    media_type: candidate.proxy_mime.clone(),
                    base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                },
                &candidate.filename,
            ),
        });
        run_ids.push(run_id);
    }

    let key = ai
        .keyring
        .open(
            &credential.sealed_key,
            &credential.associated_data(slug.as_str()),
        )
        .map_err(|error| {
            Error::Permanent(format!(
                "the tenant's model credential is unusable: {error}"
            ))
        })?;
    let batch = AnthropicBatch::new(
        Arc::clone(&ai.transport),
        key,
        credential.base_url.as_deref(),
        settings
            .model
            .clone()
            .unwrap_or_else(|| credential.default_model.clone()),
    );

    match batch.submit(&items).await {
        Ok(batch_id) => {
            let mut conn = TenantConn::begin(global, slug).await?;
            enrichment::attach_batch(conn.executor(), &run_ids, &batch_id).await?;
            conn.commit().await?;
            Ok(Submitted::Batch {
                batch_id,
                count: items.len(),
            })
        }
        Err(error) => {
            // Every run opened for this batch has to be closed, or the work list thinks they are in flight
            // forever and the library never finishes.
            let transient = error.is_transient();
            let mut conn = TenantConn::begin(global, slug).await?;
            for run_id in &run_ids {
                enrichment::finish_run(
                    conn.executor(),
                    *run_id,
                    Outcome::Failed,
                    Cost::default(),
                    &serde_json::json!({"submit": {"state": "failed", "error": error.to_string()}}),
                    Some("submit"),
                    false,
                )
                .await?;
            }
            conn.commit().await?;
            Err(if transient {
                Error::Transient(error.to_string())
            } else {
                Error::Permanent(error.to_string())
            })
        }
    }
}

/// Polls one batch and, if it has ended, writes every result.
pub async fn collect(
    global: &sqlx::PgPool,
    ai: &AiContext,
    slug: &dam_core::TenantSlug,
    tenant_id: Uuid,
    batch_id: &str,
) -> Result<Collected> {
    let mut conn = TenantConn::begin(global, slug).await?;
    let settings = enrichment::settings(conn.executor()).await?;
    let credential = dam_db::ai_credentials::current(conn.executor()).await?;
    let (vocabulary, term_count) =
        enrichment::vocabulary(conn.executor(), crate::enrich::VOCABULARY_LIMIT).await?;
    let open = enrichment::runs_in_batch(conn.executor(), batch_id).await?;
    conn.commit().await?;

    let Some(credential) = credential else {
        // The credential was withdrawn while a batch was in flight. Permanent: without a key the results cannot
        // be fetched at all, and the runs have to be closed rather than left open forever.
        close_open(
            global,
            slug,
            &open,
            "the credential was withdrawn while the batch was in flight",
        )
        .await?;
        return Err(Error::Permanent(
            "no model credential is configured; the batch's results cannot be read".to_owned(),
        ));
    };

    let brief = Brief {
        guidance: settings.guidance.clone(),
        vocabulary: vocabulary
            .into_iter()
            .map(|(slug, label, synonyms)| TermOffer {
                slug,
                label,
                synonyms,
            })
            .collect(),
        language: settings.language.clone(),
    };
    let prompt_digest = blake3::hash(brief.instructions().as_bytes())
        .to_hex()
        .to_string();

    let key = ai
        .keyring
        .open(
            &credential.sealed_key,
            &credential.associated_data(slug.as_str()),
        )
        .map_err(|error| {
            Error::Permanent(format!(
                "the tenant's model credential is unusable: {error}"
            ))
        })?;
    let batch = AnthropicBatch::new(
        Arc::clone(&ai.transport),
        key,
        credential.base_url.as_deref(),
        settings
            .model
            .clone()
            .unwrap_or_else(|| credential.default_model.clone()),
    );

    let status = batch.poll(batch_id).await.map_err(from_model_error)?;
    if !status.is_ended() {
        return Ok(Collected::Waiting {
            finished: status.finished(),
            total: status.finished() + status.processing,
        });
    }

    let results = batch.results(batch_id).await.map_err(from_model_error)?;
    let by_custom_id: std::collections::HashMap<&str, &BatchOutcome> = results
        .iter()
        .map(|result| (result.custom_id.as_str(), &result.outcome))
        .collect();

    let period = quotas::month_start(chrono::Utc::now());
    let mut applied = Collected::Applied {
        wrote: 0,
        declined: 0,
        errored: 0,
        expired: 0,
        micro_cents: 0,
    };
    let Collected::Applied {
        wrote,
        declined,
        errored,
        expired,
        micro_cents,
    } = &mut applied
    else {
        unreachable!("just constructed as Applied")
    };

    for (run_id, asset_id, custom_id) in &open {
        let outcome = by_custom_id.get(custom_id.as_str());
        match outcome {
            Some(BatchOutcome::Answered(completion)) => {
                // The same reader the synchronous path uses, against the vocabulary as it stands *now*: terms
                // may have been added or deprecated while the batch was in flight, and matching against today's
                // taxonomy is what a reviewer would expect.
                match dam_ai::enrich::read(completion.clone(), &brief) {
                    Ok(suggestion) => {
                        let cost = ai
                            .prices
                            .estimate_batched(&suggestion.model, &suggestion.usage);
                        crate::enrich::apply(
                            global,
                            slug,
                            *asset_id,
                            *run_id,
                            &suggestion,
                            &Writeback {
                                settings: &settings,
                                prompt_digest: &prompt_digest,
                                micro_cents: cost,
                                vocabulary_offered: brief.vocabulary.len(),
                                vocabulary_total: term_count,
                            },
                        )
                        .await?;
                        *micro_cents += cost;
                        *wrote += 1;
                    }
                    Err(error) => {
                        // The call was made and billed, so the cost is recorded even though nothing was written.
                        let cost = ai
                            .prices
                            .estimate_batched(&completion.model, &completion.usage);
                        finish(
                            global,
                            slug,
                            *run_id,
                            Outcome::Failed,
                            cost,
                            &serde_json::json!({"describe": {"state": "unreadable", "error": error.to_string()}}),
                            Some("describe"),
                        )
                        .await?;
                        *micro_cents += cost;
                        *errored += 1;
                    }
                }
            }
            Some(BatchOutcome::Declined(why)) => {
                finish(
                    global,
                    slug,
                    *run_id,
                    Outcome::Skipped,
                    0,
                    &serde_json::json!({"describe": {"state": "declined", "reason": why}}),
                    None,
                )
                .await?;
                *declined += 1;
            }
            Some(BatchOutcome::Errored(why)) => {
                finish(
                    global,
                    slug,
                    *run_id,
                    Outcome::Failed,
                    0,
                    &serde_json::json!({"describe": {"state": "errored", "error": why}}),
                    Some("describe"),
                )
                .await?;
                *errored += 1;
            }
            // Expired and cancelled are *not* failures of the asset: the request never ran and was never
            // billed, so the run is skipped and the asset becomes a candidate again on the next slice.
            Some(BatchOutcome::Expired) => {
                finish(
                    global,
                    slug,
                    *run_id,
                    Outcome::Skipped,
                    0,
                    &serde_json::json!({"describe": {"state": "expired"}}),
                    None,
                )
                .await?;
                *expired += 1;
            }
            Some(BatchOutcome::Canceled) => {
                finish(
                    global,
                    slug,
                    *run_id,
                    Outcome::Skipped,
                    0,
                    &serde_json::json!({"describe": {"state": "canceled"}}),
                    None,
                )
                .await?;
                *expired += 1;
            }
            None => {
                // The batch ended and this request is not in the results. Closed as failed rather than left
                // running: an open run makes the asset invisible to the work list forever, which is the one
                // outcome a backfill must never produce.
                finish(
                    global,
                    slug,
                    *run_id,
                    Outcome::Failed,
                    0,
                    &serde_json::json!({"describe": {"state": "missing_from_results"}}),
                    Some("describe"),
                )
                .await?;
                *errored += 1;
            }
        }
    }

    let spend = *micro_cents;
    crate::enrich::charge(global, tenant_id, period, spend).await?;
    Ok(applied)
}

/// Closes a run with a cost and a story.
async fn finish(
    global: &sqlx::PgPool,
    slug: &dam_core::TenantSlug,
    run_id: Uuid,
    outcome: Outcome,
    micro_cents: i64,
    stages: &serde_json::Value,
    failed_stage: Option<&str>,
) -> Result<()> {
    let mut conn = TenantConn::begin(global, slug).await?;
    enrichment::finish_run(
        conn.executor(),
        run_id,
        outcome,
        Cost {
            micro_cents,
            ..Cost::default()
        },
        stages,
        failed_stage,
        false,
    )
    .await?;
    conn.commit().await?;
    Ok(())
}

/// Closes every open run in a batch that can no longer be read.
async fn close_open(
    global: &sqlx::PgPool,
    slug: &dam_core::TenantSlug,
    open: &[(Uuid, Uuid, String)],
    why: &str,
) -> Result<()> {
    for (run_id, _, _) in open {
        finish(
            global,
            slug,
            *run_id,
            Outcome::Failed,
            0,
            &serde_json::json!({"describe": {"state": "abandoned", "reason": why}}),
            Some("describe"),
        )
        .await?;
    }
    Ok(())
}

/// A provider error, as the queue reads it.
fn from_model_error(error: ModelError) -> Error {
    if error.is_transient() {
        Error::Transient(error.to_string())
    } else {
        Error::Permanent(error.to_string())
    }
}
