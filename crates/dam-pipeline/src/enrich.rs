//! One asset, one model call, and everything that has to be true around it (M5b).
//!
//! The call itself is three lines. What this module is actually about is the seven things that must happen in
//! the right order, because each of them is a way for a paid pipeline to go wrong quietly:
//!
//! 1. **Is it switched on?** `enrichment_settings.is_enabled` defaults to false. A tenant who has not decided
//!    does not get a bill.
//! 2. **Is there room in the budget?** Checked *before* the call, because the cost is only known after it. A
//!    hard cap that was checked afterwards is a cap that is discovered by exceeding it.
//! 3. **Is there a credential?** No key is not a failure — it is a tenant who has not finished setting up, and
//!    a dead-lettered job would be the wrong way to say so.
//! 4. **Read the proxy, never the original.** `enrichment_runs.used_original` exists to catch this, because at
//!    library scale reading masters is a restore storm rather than a slow job.
//! 5. **Ask.** A refusal is not a failure and must not be retried; a throttle is and must be.
//! 6. **Write with provenance, and only where the tenant allows.** `dam_db::enrichment` owns those rules.
//! 7. **Charge, and record.** The token counts as reported, the cost as estimated, and the run row either way —
//!    including for a refusal, which is a call that was paid for.
//!
//! ## The run row is opened before the call and closed after it
//!
//! So a worker killed mid-call leaves a `running` row rather than nothing at all: `enrichment_runs_state_idx`
//! exists for exactly that, and the alternative — writing the row only on success — makes a crash indistinguishable
//! from a job that never ran.

use crate::{Error, Result};
use base64::Engine as _;
use dam_ai::enrich::{Brief, TermOffer};
use dam_ai::model::{ModelError, Part};
use dam_db::TenantConn;
use dam_db::enrichment::{self, Attribution, Cost, Outcome, Settings, Source};
use dam_db::quotas;
use dam_store::{BlobStore, Key};
use std::sync::Arc;
use uuid::Uuid;

/// How many terms to put in the prompt.
///
/// A taxonomy is not a prompt: at some size the vocabulary costs more than the answer is worth, and past this
/// the right answer is an embedding shortlist (M4) rather than a longer prefix. Cached, so the marginal cost of
/// the prefix is a tenth — but a tenth of something large is still large.
pub const VOCABULARY_LIMIT: i64 = 400;

/// What one enrichment did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enriched {
    pub asset_id: Uuid,
    /// The run row, so a caller can point at it.
    pub run_id: Uuid,
    pub outcome: EnrichOutcome,
}

/// The shape of what happened, for the log and for a test to assert on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrichOutcome {
    /// Values written and tags suggested.
    Wrote {
        fields: Vec<String>,
        tags: usize,
        /// Words the model reached for that the tenant has no term for. The vocabulary-gap signal.
        unknown_tags: Vec<String>,
        /// Micro-cents, as estimated from the reported tokens.
        micro_cents: i64,
    },
    /// Nothing was asked, and why. Not a failure: every one of these is a tenant's own configuration or a
    /// deliberate stop.
    Skipped(String),
    /// The provider declined. Recorded, charged nothing, and never retried.
    Declined(Option<String>),
}

/// Everything the stage needs that is not in the database.
///
/// Held by the worker's `Context` and passed through, so a test can drive the whole stage against a recorded
/// transport — which is the only way to test it, since the alternative needs a key and a network.
#[derive(Clone)]
pub struct AiContext {
    pub keyring: dam_core::sealed::SealingKeyring,
    pub prices: dam_ai::pricing::Prices,
    pub transport: Arc<dyn dam_ai::model::Transport>,
}

impl std::fmt::Debug for AiContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiContext").finish_non_exhaustive()
    }
}

/// Describes one asset with a hosted model.
pub async fn asset(
    global: &sqlx::PgPool,
    store: &dyn BlobStore,
    ai: &AiContext,
    slug: &dam_core::TenantSlug,
    tenant_id: Uuid,
    asset_id: Uuid,
) -> Result<Enriched> {
    let mut conn = TenantConn::begin(global, slug).await?;
    let settings = enrichment::settings(conn.executor()).await?;
    let asset_row = sqlx::query_as::<_, (String, String)>(
        "SELECT filename, mime FROM assets WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(asset_id)
    .fetch_optional(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;
    let credential = dam_db::ai_credentials::current(conn.executor()).await?;
    let proxy = dam_db::derivatives::current_proxy(conn.executor(), asset_id).await?;
    let (vocabulary, term_count) =
        enrichment::vocabulary(conn.executor(), VOCABULARY_LIMIT).await?;
    conn.commit().await?;

    let Some((filename, _mime)) = asset_row else {
        // Deleted between queueing and claiming. Ordinary, and permanent.
        return Err(Error::Permanent(format!(
            "asset {asset_id} does not exist or was deleted"
        )));
    };

    // Opened here: everything from this point can fail, and a run row is how a reader learns that it did.
    let mut conn = TenantConn::begin(global, slug).await?;
    let run_id = enrichment::start_run(
        conn.executor(),
        asset_id,
        dam_ai::enrich::PIPELINE,
        dam_ai::enrich::PIPELINE_VERSION,
    )
    .await?;
    conn.commit().await?;

    if !settings.is_enabled {
        return skip(
            global,
            slug,
            asset_id,
            run_id,
            "enrichment is switched off for this tenant",
        )
        .await;
    }
    let Some(credential) = credential else {
        // Not a failure. A tenant part-way through setting up should see "no credential", not a dead letter.
        return skip(
            global,
            slug,
            asset_id,
            run_id,
            "no model credential is configured",
        )
        .await;
    };

    // Before the call, because a call's cost is reported rather than predicted. The consequence — a cap can be
    // overshot by whatever is already in flight — is recorded in DECISIONS.md.
    let period = quotas::month_start(chrono::Utc::now());
    let verdict = {
        let mut global_conn = global.acquire().await.map_err(dam_db::Error::from)?;
        quotas::check(&mut global_conn, tenant_id, quotas::AI_SPEND, period).await?
    };
    if !verdict.allowed() {
        return skip(
            global,
            slug,
            asset_id,
            run_id,
            "the tenant is over its hard AI spend cap for this month",
        )
        .await;
    }

    let Some(proxy) = proxy else {
        // Transient on purpose: a proxy appears when the derive job finishes, and the enrich job is normally
        // chained after it. Retrying is right, and the attempt budget is what stops it forever.
        return Err(Error::Transient(format!(
            "asset {asset_id} has no proxy to describe yet"
        )));
    };

    if !describable(&proxy.mime) {
        // A text file or an archive has a proxy in name only. Skipped rather than failed: a library is full of
        // files nothing can look at, and that is not a broken asset.
        return skip(
            global,
            slug,
            asset_id,
            run_id,
            &format!("the proxy is {}, which no image block accepts", proxy.mime),
        )
        .await;
    }

    // The proxy's stored key, not one recomputed from the profile: a redefined profile changes the op hash, and
    // recomputing here would fetch an object that may not exist while the row points at one that does.
    let key = Key::new(proxy.object_key.clone())?;
    let bytes = store.get(&key, None).await?.into_bytes(&key)?;
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
    // A hash of the instructions, for `ai_disclosures.prompt_digest`: the prompt itself carries a tenant's own
    // guidance and 0006 says to store the digest rather than the text.
    let instructions = brief.instructions();
    let prompt_digest = blake3::hash(instructions.as_bytes()).to_hex().to_string();

    let model = dam_ai::credential::open(
        &credential,
        slug.as_str(),
        &ai.keyring,
        Arc::clone(&ai.transport),
        settings.model.as_deref(),
    )
    .map_err(|error| {
        // A credential that cannot be opened or has no endpoint is a configuration fault, not something a
        // retry fixes — and it is the same for every asset, so retrying would burn the whole queue against it.
        Error::Permanent(format!(
            "the tenant's model credential is unusable: {error}"
        ))
    })?;

    let suggestion = match dam_ai::enrich::describe(
        model.as_ref(),
        &brief,
        Part::Image {
            media_type: proxy.mime.clone(),
            base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        },
        &filename,
    )
    .await
    {
        Ok(suggestion) => suggestion,
        Err(ModelError::Declined(why)) => {
            // A refusal is an answer. Recorded as skipped rather than failed, so the queue does not spend four
            // more attempts on an asset that was never going to work.
            let mut conn = TenantConn::begin(global, slug).await?;
            enrichment::finish_run(
                conn.executor(),
                run_id,
                Outcome::Skipped,
                Cost::default(),
                &serde_json::json!({"describe": {"state": "declined", "reason": why}}),
                None,
                false,
            )
            .await?;
            conn.commit().await?;
            return Ok(Enriched {
                asset_id,
                run_id,
                outcome: EnrichOutcome::Declined(why),
            });
        }
        Err(error) => {
            let transient = error.is_transient();
            let mut conn = TenantConn::begin(global, slug).await?;
            enrichment::finish_run(
                conn.executor(),
                run_id,
                Outcome::Failed,
                Cost::default(),
                &serde_json::json!({"describe": {"state": "failed", "error": error.to_string()}}),
                Some("describe"),
                false,
            )
            .await?;
            conn.commit().await?;
            return Err(if transient {
                Error::Transient(error.to_string())
            } else {
                Error::Permanent(error.to_string())
            });
        }
    };

    let micro_cents = ai.prices.estimate(&suggestion.model, &suggestion.usage);
    let wrote = apply(
        global,
        slug,
        asset_id,
        run_id,
        &suggestion,
        &Writeback {
            settings: &settings,
            prompt_digest: &prompt_digest,
            micro_cents,
            vocabulary_offered: brief.vocabulary.len(),
            vocabulary_total: term_count,
        },
    )
    .await?;

    // Charged after the call and after the writes, so a crash between them costs the tenant nothing they cannot
    // see: the run row is what says the call happened.
    charge(global, tenant_id, period, micro_cents).await?;

    Ok(Enriched {
        asset_id,
        run_id,
        outcome: wrote,
    })
}

/// Everything the write-back needs that is not the suggestion itself.
///
/// A struct rather than eight parameters, because the synchronous stage and the batch collector both pass it and
/// a positional mistake between two `usize`s would be a silently wrong `vocabulary_truncated`.
#[derive(Debug, Clone, Copy)]
pub struct Writeback<'a> {
    pub settings: &'a Settings,
    /// A hash of the instructions, for `ai_disclosures.prompt_digest`.
    pub prompt_digest: &'a str,
    /// What the call cost, in micro-cents. Halved already for a batched run — see
    /// `dam_ai::pricing::Prices::estimate_batched`.
    pub micro_cents: i64,
    pub vocabulary_offered: usize,
    pub vocabulary_total: i64,
}

/// Writes one suggestion onto one asset and closes its run.
///
/// Shared by the synchronous stage and the batch collector. Sharing it is the point: a backfill that wrote
/// values a different way would drift from the live path in exactly the places that matter — which field a
/// value lands in, whether provenance is attached, whether a person's edit survives — and the drift would only
/// show up months later in somebody's audit.
pub async fn apply(
    global: &sqlx::PgPool,
    slug: &dam_core::TenantSlug,
    asset_id: Uuid,
    run_id: Uuid,
    suggestion: &dam_ai::enrich::Suggestion,
    writeback: &Writeback<'_>,
) -> Result<EnrichOutcome> {
    let attribution = Attribution {
        source: Source::Llm,
        model: suggestion.model.clone(),
        model_version: format!(
            "{}/{}",
            dam_ai::enrich::PIPELINE,
            dam_ai::enrich::PIPELINE_VERSION
        ),
        confidence: suggestion.confidence,
    };
    let settings = writeback.settings;

    let mut values: Vec<(String, serde_json::Value)> = Vec::with_capacity(2);
    // An empty answer is not written: a field set to "" would look like a description somebody had deleted,
    // and it would carry provenance saying a model wrote the emptiness.
    if let Some(field) = settings
        .alt_text_field
        .as_ref()
        .filter(|_| !suggestion.alt_text.is_empty())
    {
        values.push((field.clone(), serde_json::json!(suggestion.alt_text)));
    }
    if let Some(field) = settings
        .description_field
        .as_ref()
        .filter(|_| !suggestion.description.is_empty())
    {
        values.push((field.clone(), serde_json::json!(suggestion.description)));
    }

    let mut conn = TenantConn::begin(global, slug).await?;
    let written = enrichment::write_values(
        conn.executor(),
        asset_id,
        &values,
        &attribution,
        chrono::Utc::now(),
    )
    .await?;
    let model_id = enrichment::register_model(
        conn.executor(),
        &suggestion.model,
        &attribution.model_version,
        "llm",
        "api",
        &serde_json::json!({
            "pipeline": dam_ai::enrich::PIPELINE,
            "vocabulary_offered": writeback.vocabulary_offered,
            "vocabulary_total": writeback.vocabulary_total,
        }),
    )
    .await?;
    // One disclosure row per written field (G2). `metadata_only`: the picture is untouched and only its
    // description is machine-written, which is the distinction 0006 exists to draw.
    for field in &written.written {
        enrichment::disclose(
            conn.executor(),
            asset_id,
            field,
            model_id,
            &suggestion.model,
            writeback.prompt_digest,
        )
        .await?;
    }
    let tagged = if settings.suggest_tags && !suggestion.tags.is_empty() {
        enrichment::suggest_tags(
            conn.executor(),
            asset_id,
            &suggestion.tags,
            model_id,
            suggestion.confidence,
        )
        .await?
    } else {
        enrichment::Tagged::default()
    };

    // Partial when the model produced something the tenant would not take: a description stored and a
    // `copyright` refused is not a success, and a reader of the run needs to see which.
    let outcome = if written.refused.is_empty() && written.kept_human.is_empty() {
        Outcome::Succeeded
    } else {
        Outcome::Partial
    };
    enrichment::finish_run(
        conn.executor(),
        run_id,
        outcome,
        Cost {
            input_tokens: i64::try_from(suggestion.usage.input_tokens).unwrap_or(i64::MAX),
            output_tokens: i64::try_from(suggestion.usage.output_tokens).unwrap_or(i64::MAX),
            cached_tokens: i64::try_from(suggestion.usage.cached_input_tokens).unwrap_or(i64::MAX),
            micro_cents: writeback.micro_cents,
        },
        &serde_json::json!({
            "describe": {"state": "ok"},
            "written": written.written,
            "refused": written.refused,
            "kept_human": written.kept_human,
            "tags_suggested": tagged.suggested.len(),
            "tags_seconded": tagged.seconded.len(),
            "tags_already_decided": tagged.decided.len(),
            "unknown_tags": suggestion.unknown_tags,
            "vocabulary_truncated": writeback.vocabulary_total > writeback.vocabulary_offered as i64,
        }),
        None,
        // The proxy, always. A true here means some stage started reading masters.
        false,
    )
    .await?;
    conn.commit().await?;

    Ok(EnrichOutcome::Wrote {
        fields: written.written,
        tags: tagged.suggested.len() + tagged.seconded.len(),
        unknown_tags: suggestion.unknown_tags.clone(),
        micro_cents: writeback.micro_cents,
    })
}

/// Adds a call's cost to the tenant's monthly spend.
pub async fn charge(
    global: &sqlx::PgPool,
    tenant_id: Uuid,
    period: chrono::NaiveDate,
    micro_cents: i64,
) -> Result<()> {
    if micro_cents == 0 {
        return Ok(());
    }
    let mut conn = global.acquire().await.map_err(dam_db::Error::from)?;
    quotas::charge(&mut conn, tenant_id, quotas::AI_SPEND, period, micro_cents).await?;
    Ok(())
}

/// Closes a run that never made a call.
async fn skip(
    global: &sqlx::PgPool,
    slug: &dam_core::TenantSlug,
    asset_id: Uuid,
    run_id: Uuid,
    why: &str,
) -> Result<Enriched> {
    let mut conn = TenantConn::begin(global, slug).await?;
    enrichment::finish_run(
        conn.executor(),
        run_id,
        Outcome::Skipped,
        Cost::default(),
        &serde_json::json!({"describe": {"state": "skipped", "reason": why}}),
        None,
        false,
    )
    .await?;
    conn.commit().await?;
    Ok(Enriched {
        asset_id,
        run_id,
        outcome: EnrichOutcome::Skipped(why.to_owned()),
    })
}

/// Whether a mime type is one this pipeline can describe.
///
/// Both hosted families take JPEG, PNG, GIF and WebP as image blocks and nothing else — a TIFF or a PDF sent as
/// one is a 400 for a reason no caller can act on. The *proxy* is always one of these, which is why this checks
/// the derivative's mime rather than the asset's: a RAW file with a JPEG proxy is describable, and the asset's
/// own mime would say otherwise.
pub fn describable(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    )
}

/// Settings a caller can hand to a tenant that has none, for a first run.
pub fn starter_settings() -> Settings {
    Settings {
        is_enabled: true,
        ..Settings::default()
    }
}
