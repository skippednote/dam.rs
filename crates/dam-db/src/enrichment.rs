//! Writing what a model said, in a way somebody can undo (M5b).
//!
//! Three tables, and the reason they are one module is that a suggestion is not usable unless all three agree:
//! `asset_metadata` holds the value, `asset_metadata.provenance` says a model wrote it, and `enrichment_runs`
//! says which call it came from and what it cost. A value written without its provenance is indistinguishable
//! from something a person typed, which is the exact claim the AI Act marking obligation (G2) turns on.
//!
//! ## Provenance is per field, and a human edit removes it
//!
//! `provenance->'<key>'` carries `{source, model, model_version, confidence, at, reviewed_by}` — 0001 says so
//! and this is its first writer. The corollary matters more than the writing: when a person edits a field the
//! model wrote, the provenance for *that key* goes. Leaving it would mark a human sentence as machine output
//! forever, and a disclosure that is wrong in that direction is worse than none — it teaches people to ignore
//! the marking.
//!
//! ## Only `ai_writable` fields, enforced here
//!
//! `field_defs.ai_writable` is the tenant's answer to "may a model touch this". A caller could check it; this
//! module checks it anyway, because there will be more than one caller — the review queue, a batch backfill, a
//! re-run — and a rule enforced in the writer cannot be forgotten by the next one. What was refused comes back,
//! so a run can record that a model produced a `copyright` nobody would let it write.
//!
//! ## A tag is a suggestion until a person says otherwise
//!
//! LLM tags land `state = 'suggested'`, never `confirmed`, whatever confidence the model claimed: the number is
//! self-reported and uncalibrated, and `taxonomy_terms.ai_threshold` exists for the probe paths where it is
//! measured. Two rules follow, and both are about not overwriting a decision: a term a person has *rejected*
//! stays rejected rather than being re-proposed, and one already `confirmed` is left alone. Where two
//! generators propose the same term, `generator_votes` counts them — a tag three generators agree on is a
//! different thing from one the LLM alone invented, which is what the review queue sorts on.

use crate::Error;
use serde_json::json;
use uuid::Uuid;

/// Where a value came from. Written into `provenance->'<key>'.source`.
///
/// The strings are the vocabulary the disclosure surface reads, so they are spelled once here rather than at
/// each call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A hosted model, through `dam_ai`.
    Llm,
    /// A local model (M4).
    Local,
    /// Extracted from the file itself — EXIF, XMP, an office property.
    Embedded,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Local => "local",
            Self::Embedded => "embedded",
        }
    }
}

/// What to record about one machine-written value.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribution {
    pub source: Source,
    /// The model that answered, not the one that was asked for.
    pub model: String,
    /// The pipeline version, so "re-run everything the old prompt touched" stays expressible.
    pub model_version: String,
    /// As claimed by the model. Recorded, not trusted.
    pub confidence: f32,
}

impl Attribution {
    fn to_json(&self, at: chrono::DateTime<chrono::Utc>) -> serde_json::Value {
        json!({
            "source": self.source.as_str(),
            "model": self.model,
            "model_version": self.model_version,
            "confidence": self.confidence,
            "at": at.to_rfc3339(),
            // Explicitly null rather than absent: the field exists in 0001's contract, and a reader asking
            // "has anybody checked this" should get an answer rather than a missing key.
            "reviewed_by": serde_json::Value::Null,
        })
    }
}

/// What a metadata write actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// Keys that landed.
    pub written: Vec<String>,
    /// Keys the tenant does not allow a model to write, or does not define at all. Recorded so a run can say
    /// the model produced something nobody wanted rather than silently discarding it.
    pub refused: Vec<String>,
    /// Keys skipped because a person had already written them. A model must not overwrite a human value: the
    /// human one is the reviewed one, and a re-run would quietly undo an edit.
    pub kept_human: Vec<String>,
}

/// Writes machine-produced values onto an asset, with their provenance.
///
/// Merges rather than replaces: an asset's metadata is one document and a model contributes part of it.
pub async fn write_values(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    values: &[(String, serde_json::Value)],
    attribution: &Attribution,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<Written, Error> {
    // The tenant's rules, read rather than assumed. A key absent from `field_defs` is refused for the same
    // reason as one marked read-only: `values` is a jsonb column, so nothing else would stop it.
    let writable: Vec<String> =
        sqlx::query_scalar("SELECT key FROM field_defs WHERE ai_writable AND NOT read_only")
            .fetch_all(&mut *conn)
            .await?;

    let (existing_values, existing_provenance): (serde_json::Value, serde_json::Value) =
        sqlx::query_as("SELECT values, provenance FROM asset_metadata WHERE asset_id = $1")
            .bind(asset_id)
            .fetch_optional(&mut *conn)
            .await?
            .unwrap_or_else(|| (json!({}), json!({})));

    let mut merged = existing_values.as_object().cloned().unwrap_or_default();
    let mut provenance = existing_provenance.as_object().cloned().unwrap_or_default();
    let mut result = Written {
        written: Vec::new(),
        refused: Vec::new(),
        kept_human: Vec::new(),
    };

    for (key, value) in values {
        if !writable.contains(key) {
            result.refused.push(key.clone());
            continue;
        }
        // A value with no provenance was written by a person — see the module note on why absence means human.
        let human = merged.contains_key(key) && !provenance.contains_key(key);
        if human {
            result.kept_human.push(key.clone());
            continue;
        }
        merged.insert(key.clone(), value.clone());
        provenance.insert(key.clone(), attribution.to_json(at));
        result.written.push(key.clone());
    }

    if result.written.is_empty() {
        return Ok(result);
    }

    sqlx::query(
        "INSERT INTO asset_metadata (asset_id, values, provenance) VALUES ($1, $2, $3) \
         ON CONFLICT (asset_id) DO UPDATE \
            SET values = excluded.values, provenance = excluded.provenance, updated_at = now()",
    )
    .bind(asset_id)
    .bind(serde_json::Value::Object(merged))
    .bind(serde_json::Value::Object(provenance))
    .execute(&mut *conn)
    .await?;

    // The asset's own `updated_at`, or the reindex queue and the connector never see the change — the same
    // reason the human metadata route moves it.
    sqlx::query("UPDATE assets SET updated_at = now() WHERE id = $1")
        .bind(asset_id)
        .execute(&mut *conn)
        .await?;

    Ok(result)
}

/// Drops the provenance for keys a person has just written.
///
/// Called from the human metadata path. Without it a field a model wrote stays marked as machine output after
/// somebody rewrites it, and every disclosure built on `provenance` is then wrong in the direction that
/// destroys trust in the marking.
pub async fn forget_provenance(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    keys: &[String],
) -> Result<(), Error> {
    if keys.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE asset_metadata SET provenance = provenance - $2::text[], updated_at = now() \
         WHERE asset_id = $1",
    )
    .bind(asset_id)
    .bind(keys)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The keys on one asset that a machine wrote, with what wrote them.
///
/// The disclosure query (G2). Ordered by key so a surface rendering it is stable.
pub async fn machine_written(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
) -> Result<Vec<(String, serde_json::Value)>, Error> {
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT key, value FROM asset_metadata, jsonb_each(provenance) AS entry(key, value) \
          WHERE asset_id = $1 ORDER BY key",
    )
    .bind(asset_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows)
}

/// What a tag write did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tagged {
    /// Terms now suggested on the asset.
    pub suggested: Vec<Uuid>,
    /// Terms another generator had already proposed; their vote count went up instead.
    pub seconded: Vec<Uuid>,
    /// Terms left alone because a person had already decided about them.
    pub decided: Vec<Uuid>,
    /// Slugs that are not terms in this tenant. Should be empty when the caller offered the vocabulary, and is
    /// worth knowing about when it is not.
    pub unknown: Vec<String>,
}

/// Proposes tags on an asset, by slug.
pub async fn suggest_tags(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    slugs: &[String],
    model_id: Uuid,
    confidence: f32,
) -> Result<Tagged, Error> {
    let mut tagged = Tagged::default();
    for slug in slugs {
        let term: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM taxonomy_terms WHERE slug = $1")
                .bind(slug)
                .fetch_optional(&mut *conn)
                .await?;
        let Some(term_id) = term else {
            tagged.unknown.push(slug.clone());
            continue;
        };

        let existing: Option<String> =
            sqlx::query_scalar("SELECT state FROM asset_tags WHERE asset_id = $1 AND term_id = $2")
                .bind(asset_id)
                .bind(term_id)
                .fetch_optional(&mut *conn)
                .await?;
        match existing.as_deref() {
            // A decision a person made. Re-proposing a rejected term would put it back in the queue they
            // cleared, and re-proposing a confirmed one would demote it.
            Some("rejected" | "confirmed") => {
                tagged.decided.push(term_id);
                continue;
            }
            Some(_) => {
                sqlx::query(
                    "UPDATE asset_tags \
                        SET generator_votes = generator_votes + 1, \
                            confidence = GREATEST(COALESCE(confidence, 0), $3), \
                            model_id = COALESCE(model_id, $4) \
                      WHERE asset_id = $1 AND term_id = $2",
                )
                .bind(asset_id)
                .bind(term_id)
                .bind(confidence)
                .bind(model_id)
                .execute(&mut *conn)
                .await?;
                tagged.seconded.push(term_id);
            }
            None => {
                sqlx::query(
                    "INSERT INTO asset_tags \
                     (asset_id, term_id, state, source, model_id, confidence, generator_votes) \
                     VALUES ($1, $2, 'suggested', 'llm', $3, $4, 1)",
                )
                .bind(asset_id)
                .bind(term_id)
                .bind(model_id)
                .bind(confidence)
                .execute(&mut *conn)
                .await?;
                tagged.suggested.push(term_id);
            }
        }
    }
    Ok(tagged)
}

/// Registers a model in `ai_models`, returning its id.
///
/// Idempotent on `(key, version)`, which is what the unique index in 0003 is for: every AI-written value points
/// at a row here, so "which assets did the old tagger touch" is a join rather than an archaeology project.
pub async fn register_model(
    conn: &mut sqlx::PgConnection,
    key: &str,
    version: &str,
    kind: &str,
    runtime: &str,
    config: &serde_json::Value,
) -> Result<Uuid, Error> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO ai_models (id, key, version, kind, runtime, config) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (key, version) DO UPDATE SET active = true \
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(key)
    .bind(version)
    .bind(kind)
    .bind(runtime)
    .bind(config)
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    /// Something was written and something was not — a description stored, tags refused.
    Partial,
    Failed,
    /// Deliberately not attempted: over a hard spend cap, no credential configured, nothing to describe.
    Skipped,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// What a call used, as the provider reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cost {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    /// Micro-cents, the unit `dam_db::quotas` charges in. Converted to the column's `numeric` here so one
    /// module owns the conversion.
    pub micro_cents: i64,
}

/// Opens a run row. Returns its id, which the caller needs before submitting a batch — `llm_custom_id` has to
/// be persisted before the request leaves, or results arrive keyed to something nothing recognises.
pub async fn start_run(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    pipeline: &str,
    pipeline_version: i32,
) -> Result<Uuid, Error> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO enrichment_runs (id, asset_id, pipeline, pipeline_version) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(asset_id)
    .bind(pipeline)
    .bind(pipeline_version)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

/// Closes a run row.
///
/// `stages` is whatever the pipeline wants a reader to know: what was written, what was refused, which words
/// the model reached for that the tenant has no term for. `used_original` should be false on every row — 0003
/// says so, and a true one means some stage is reading masters, which at library scale is a restore storm.
pub async fn finish_run(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    outcome: Outcome,
    cost: Cost,
    stages: &serde_json::Value,
    failed_stage: Option<&str>,
    used_original: bool,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE enrichment_runs \
            SET state = $2, stages = $3, failed_stage = $4, used_original = $5, \
                input_tokens = $6, output_tokens = $7, cached_tokens = $8, \
                est_cost_cents = CAST($9 AS numeric), \
                finished_at = now(), \
                duration_ms = (EXTRACT(EPOCH FROM (now() - started_at)) * 1000)::int \
          WHERE id = $1",
    )
    .bind(id)
    .bind(outcome.as_str())
    .bind(stages)
    .bind(failed_stage)
    .bind(used_original)
    .bind(cost.input_tokens)
    .bind(cost.output_tokens)
    .bind(cost.cached_tokens)
    .bind(cents(cost.micro_cents))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Micro-cents as a decimal string for `numeric(12, 4)`.
///
/// Integer arithmetic and a string rather than a float: the column has four decimal places, a cheap call costs
/// a fraction of a cent, and `f64` on the way in is how a cost ledger acquires rounding nobody can explain.
fn cents(micro: i64) -> String {
    let negative = micro < 0;
    let micro = micro.unsigned_abs();
    // Four decimal places, so hundredths of a micro-cent round rather than truncate.
    let ten_thousandths = micro / 100 + u64::from(micro % 100 >= 50);
    format!(
        "{}{}.{:04}",
        if negative { "-" } else { "" },
        ten_thousandths / 10_000,
        ten_thousandths % 10_000
    )
}

/// What a tenant wants the enrichment pipeline to do.
///
/// One row (0028). `is_enabled` is the important field: it is false until somebody turns it on, because this is
/// the first pipeline in damrs that bills per asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub is_enabled: bool,
    /// The tenant's own instructions. The cacheable half of every request — see `dam_ai::enrich::Brief`.
    pub guidance: String,
    pub language: String,
    /// Overrides the credential's default model for this pipeline.
    pub model: Option<String>,
    /// Where the alt text lands, or `None` to write none.
    pub alt_text_field: Option<String>,
    pub description_field: Option<String>,
    pub suggest_tags: bool,
    /// Whether a question in the search box may be turned into a query by a model (0029).
    ///
    /// Separate from [`Self::is_enabled`] because they are different decisions: one describes the library at a
    /// cost per asset, the other answers questions at a cost per question, and a tenant may want either alone.
    pub natural_language_search: bool,
}

impl Default for Settings {
    /// What a tenant gets before anybody configures anything: nothing runs.
    fn default() -> Self {
        Self {
            is_enabled: false,
            guidance: String::new(),
            language: "English".to_owned(),
            model: None,
            alt_text_field: Some("alt_text".to_owned()),
            description_field: Some("description".to_owned()),
            suggest_tags: true,
            natural_language_search: false,
        }
    }
}

/// The settings row as the columns come back. Named because the tuple is wide enough that clippy is right about
/// it — and because a seventh column added in the wrong position would otherwise map silently.
type SettingsRow = (
    bool,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
    bool,
);

/// Reads the settings.
///
/// A missing row is [`Settings::default`] — disabled. A tenant whose schema predates 0028, or whose row somebody
/// deleted, must not silently start spending; failing closed here is the only default that cannot cost money.
pub async fn settings(conn: &mut sqlx::PgConnection) -> Result<Settings, Error> {
    let row: Option<SettingsRow> = sqlx::query_as(
        "SELECT is_enabled, guidance, language, model, alt_text_field, description_field, suggest_tags, \
                natural_language_search \
           FROM enrichment_settings WHERE id",
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map_or_else(Settings::default, |row| Settings {
        is_enabled: row.0,
        guidance: row.1,
        language: row.2,
        model: row.3,
        alt_text_field: row.4,
        description_field: row.5,
        suggest_tags: row.6,
        natural_language_search: row.7,
    }))
}

/// Replaces the settings.
pub async fn save_settings(
    conn: &mut sqlx::PgConnection,
    wanted: &Settings,
) -> Result<Settings, Error> {
    sqlx::query(
        "INSERT INTO enrichment_settings \
            (id, is_enabled, guidance, language, model, alt_text_field, description_field, \
             suggest_tags, natural_language_search, updated_at) \
         VALUES (true, $1, $2, $3, $4, $5, $6, $7, $8, now()) \
         ON CONFLICT (id) DO UPDATE \
            SET is_enabled = excluded.is_enabled, guidance = excluded.guidance, \
                language = excluded.language, model = excluded.model, \
                alt_text_field = excluded.alt_text_field, \
                description_field = excluded.description_field, \
                suggest_tags = excluded.suggest_tags, \
                natural_language_search = excluded.natural_language_search, updated_at = now()",
    )
    .bind(wanted.is_enabled)
    .bind(&wanted.guidance)
    .bind(wanted.language.trim())
    .bind(wanted.model.as_deref().map(str::trim))
    .bind(wanted.alt_text_field.as_deref().map(str::trim))
    .bind(wanted.description_field.as_deref().map(str::trim))
    .bind(wanted.suggest_tags)
    .bind(wanted.natural_language_search)
    .execute(&mut *conn)
    .await?;
    // Read back rather than echoed: the trims and the column constraints are the specification, and a caller
    // shown what it sent would not learn that `language` came back trimmed.
    settings(conn).await
}

/// The vocabulary to offer a model: every term that is not deprecated, as `(slug, label, synonyms)`.
///
/// Ordered by slug so the instruction text is byte-identical between assets — prompt caching matches on bytes,
/// and a set iterated in a different order is a cache miss on every call.
///
/// `limit` bounds the prefix: a taxonomy of fifty thousand terms is not a prompt, it is a bill. A tenant past
/// the limit needs the embedding-shortlist path (M4), and the caller can see it was truncated because it asked
/// for the count too.
pub async fn vocabulary(
    conn: &mut sqlx::PgConnection,
    limit: i64,
) -> Result<(Vec<(String, String, Vec<String>)>, i64), Error> {
    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM taxonomy_terms WHERE deprecated_at IS NULL")
            .fetch_one(&mut *conn)
            .await?;
    let rows: Vec<(String, String, Vec<String>)> = sqlx::query_as(
        "SELECT slug, label, synonyms FROM taxonomy_terms           WHERE deprecated_at IS NULL ORDER BY slug LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    Ok((rows, total))
}

/// Records a disclosure for one machine-written field (G2, Article 50).
///
/// `metadata_only` for a description or tags on an untouched photograph: 0006's grading is deliberate, because
/// marking such an asset "AI generated" would be both wrong and commercially damaging. `prompt_digest` is a
/// hash — the prompt itself may carry a tenant's confidential guidance.
///
/// Idempotent per `(asset, field)`: a re-run replaces the row rather than growing a list, because the question a
/// reader asks is "what wrote the value that is there now".
pub async fn disclose(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    field_key: &str,
    model_id: Uuid,
    model_name: &str,
    prompt_digest: &str,
) -> Result<(), Error> {
    sqlx::query("DELETE FROM ai_disclosures WHERE asset_id = $1 AND field_key = $2")
        .bind(asset_id)
        .bind(field_key)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO ai_disclosures             (id, asset_id, field_key, disclosure_kind, model_id, model_name, prompt_digest,              human_oversight, human_visible)          VALUES ($1, $2, $3, 'metadata_only', $4, $5, $6, 'none', false)",
    )
    .bind(Uuid::now_v7())
    .bind(asset_id)
    .bind(field_key)
    .bind(model_id)
    .bind(model_name)
    .bind(prompt_digest)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Records that a person looked at a machine-written field.
///
/// `edited` rather than `reviewed` when they changed it, which is the distinction the c2pa.ai-disclosure
/// oversight field draws and the one an auditor asks about. Writes both halves — the disclosure row and the
/// provenance's `reviewed_by` — because a reader may arrive at either.
pub async fn record_oversight(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    field_key: &str,
    oversight: &str,
    actor: Option<Uuid>,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE ai_disclosures SET human_oversight = $3, reviewed_by = $4, reviewed_at = now()           WHERE asset_id = $1 AND field_key = $2",
    )
    .bind(asset_id)
    .bind(field_key)
    .bind(oversight)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE asset_metadata             SET provenance = jsonb_set(provenance, ARRAY[$2],                     COALESCE(provenance -> $2, '{}'::jsonb) || jsonb_build_object('reviewed_by', $3::text)),                 updated_at = now()           WHERE asset_id = $1 AND provenance ? $2",
    )
    .bind(asset_id)
    .bind(field_key)
    .bind(actor.map(|id| id.to_string()))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// One asset waiting for somebody to look at what a model said about it.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewItem {
    pub asset_id: Uuid,
    pub filename: String,
    pub mime: String,
    /// Tags the model proposed and nobody has decided about, strongest first.
    pub suggested: Vec<SuggestedTag>,
    /// Machine-written fields on this asset that nobody has reviewed, with the value as it stands.
    pub fields: Vec<MachineField>,
}

/// A tag awaiting a decision.
#[derive(Debug, Clone, PartialEq)]
pub struct SuggestedTag {
    pub term_id: Uuid,
    pub slug: String,
    pub label: String,
    pub confidence: Option<f32>,
    /// How many independent generators proposed it. The queue's real sort key: a tag three generators agree on
    /// is a different proposition from one the LLM alone invented.
    pub votes: i16,
    pub source: String,
}

/// A machine-written value awaiting a look.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineField {
    pub key: String,
    pub value: serde_json::Value,
    pub model: String,
    pub confidence: Option<f64>,
    /// Whether anybody has recorded a look at it yet.
    pub reviewed: bool,
}

/// The review queue: assets a model has touched and a person has not.
///
/// Rendered under the caller's predicate, like every other list — §7 is explicit that counts disclose, and a
/// review queue that showed assets outside somebody's scope would be an enumeration of the library through the
/// side door.
///
/// Ordered by the strongest evidence first: votes, then confidence. A reviewer working down the list is
/// confirming the easy ones early, which is what makes a queue of thousands tractable.
pub async fn review_queue(
    conn: &mut sqlx::PgConnection,
    predicate: &dam_core::policy::AccessPredicate,
    limit: i64,
) -> Result<Vec<ReviewItem>, Error> {
    use sqlx::{Postgres, QueryBuilder};

    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT assets.id, assets.filename, assets.mime, \
                COALESCE(max(asset_tags.generator_votes), 0)::smallint AS votes, \
                COALESCE(max(asset_tags.confidence), 0)::real AS confidence \
           FROM assets \
           LEFT JOIN asset_tags ON asset_tags.asset_id = assets.id AND asset_tags.state = 'suggested' \
           LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id \
          WHERE ",
    );
    crate::access::push_asset_filter(&mut builder, predicate)?;
    builder.push(crate::versions::LIBRARY_ROWS);
    // Something to look at: either an undecided tag or a machine-written field. An asset with neither is not in
    // a queue, and `provenance <> '{}'` is the cheap half of that test.
    builder
        .push(" AND (asset_tags.term_id IS NOT NULL OR asset_metadata.provenance <> '{}'::jsonb) ");
    builder.push(" GROUP BY assets.id, assets.filename, assets.mime ");
    builder.push(" ORDER BY votes DESC, confidence DESC, assets.created_at DESC LIMIT ");
    builder.push_bind(limit);

    let rows: Vec<(Uuid, String, String, i16, f32)> =
        builder.build_query_as().fetch_all(&mut *conn).await?;

    let mut items = Vec::with_capacity(rows.len());
    for (asset_id, filename, mime, _votes, _confidence) in rows {
        let suggested: Vec<SuggestedTag> =
            sqlx::query_as::<_, (Uuid, String, String, Option<f32>, i16, String)>(
                "SELECT t.id, t.slug, t.label, a.confidence, a.generator_votes, a.source \
               FROM asset_tags a JOIN taxonomy_terms t ON t.id = a.term_id \
              WHERE a.asset_id = $1 AND a.state = 'suggested' \
              ORDER BY a.generator_votes DESC, a.confidence DESC NULLS LAST, t.slug",
            )
            .bind(asset_id)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(
                |(term_id, slug, label, confidence, votes, source)| SuggestedTag {
                    term_id,
                    slug,
                    label,
                    confidence,
                    votes,
                    source,
                },
            )
            .collect();

        let fields: Vec<MachineField> =
            sqlx::query_as::<_, (String, serde_json::Value, serde_json::Value)>(
                "SELECT entry.key, COALESCE(values -> entry.key, 'null'::jsonb), entry.value \
               FROM asset_metadata, jsonb_each(provenance) AS entry(key, value) \
              WHERE asset_id = $1 ORDER BY entry.key",
            )
            .bind(asset_id)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|(key, value, marking)| MachineField {
                key,
                value,
                model: marking
                    .get("model")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown")
                    .to_owned(),
                confidence: marking
                    .get("confidence")
                    .and_then(serde_json::Value::as_f64),
                reviewed: marking
                    .get("reviewed_by")
                    .is_some_and(|value| !value.is_null()),
            })
            .collect();

        items.push(ReviewItem {
            asset_id,
            filename,
            mime,
            suggested,
            fields,
        });
    }
    Ok(items)
}

/// What a person decided about a suggested tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    Reject,
}

impl Verdict {
    fn state(self) -> &'static str {
        match self {
            Self::Accept => "confirmed",
            Self::Reject => "rejected",
        }
    }

    fn feedback(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Reject => "reject",
        }
    }
}

/// Records a decision about one suggested tag.
///
/// Two writes, and both matter. `asset_tags` gets the state — which is what search and the asset page read — and
/// `tag_feedback` gets an append-only row, because that table is the *training set*: 0003's note says an edit
/// history that loses the rejections loses the signal that matters most.
///
/// Returns false if there was nothing suggested to decide, which is not an error: two reviewers can open the
/// same queue, and the second one clicking is an ordinary race rather than a fault.
pub async fn decide_tag(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    term_id: Uuid,
    verdict: Verdict,
    actor: Option<Uuid>,
) -> Result<bool, Error> {
    let existing: Option<(String, Option<Uuid>, Option<f32>)> = sqlx::query_as(
        "SELECT source, model_id, confidence FROM asset_tags \
          WHERE asset_id = $1 AND term_id = $2 AND state = 'suggested'",
    )
    .bind(asset_id)
    .bind(term_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((source, model_id, confidence)) = existing else {
        return Ok(false);
    };

    sqlx::query(
        "UPDATE asset_tags SET state = $3, reviewed_by = $4, reviewed_at = now() \
          WHERE asset_id = $1 AND term_id = $2",
    )
    .bind(asset_id)
    .bind(term_id)
    .bind(verdict.state())
    .bind(actor)
    .execute(&mut *conn)
    .await?;

    // `proposed_by` only takes the three generator names, so a tag a person added by hand contributes a row with
    // no proposer rather than a lie about which model suggested it.
    let proposed_by = match source.as_str() {
        generator @ ("zero_shot" | "probe" | "llm") => Some(generator),
        _ => None,
    };
    sqlx::query(
        "INSERT INTO tag_feedback (id, asset_id, term_id, verdict, proposed_by, model_id, confidence, actor_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(Uuid::now_v7())
    .bind(asset_id)
    .bind(term_id)
    .bind(verdict.feedback())
    .bind(proposed_by)
    .bind(model_id)
    .bind(confidence)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(true)
}

/// One asset a backfill could describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub asset_id: Uuid,
    pub filename: String,
    /// The proxy's object key and mime — the bytes a describe reads, never the original.
    pub object_key: String,
    pub proxy_mime: String,
}

/// Assets with a proxy and nothing a model has written yet.
///
/// The backfill's work list, and the definition of "yet" is deliberate: an asset is a candidate when no
/// `enrichment_runs` row for this pipeline version has *reached* it — succeeded, partial, or is still running.
/// Failed and skipped rows do not disqualify an asset, because a failure was a provider having a bad day and a
/// skip was a setting that has since changed. That is what makes running a backfill twice safe and useful
/// rather than either a no-op or a second bill for the same work.
///
/// `pipeline_version` is part of the test, so bumping the prompt makes the whole library a candidate again —
/// which is the operation §8.3's "re-run everything the old prompt touched" describes, and the reason the
/// version is on the row at all.
pub async fn needing_description(
    conn: &mut sqlx::PgConnection,
    pipeline: &str,
    pipeline_version: i32,
    limit: i64,
) -> Result<Vec<Candidate>, Error> {
    let rows: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT assets.id, assets.filename, d.object_key, d.mime \
           FROM assets \
           JOIN derivatives d ON d.asset_id = assets.id AND d.role = 'proxy' \
          WHERE assets.deleted_at IS NULL \
            AND assets.is_current AND assets.attached_to IS NULL \
            AND NOT EXISTS ( \
                SELECT 1 FROM enrichment_runs r \
                 WHERE r.asset_id = assets.id \
                   AND r.pipeline = $1 AND r.pipeline_version = $2 \
                   AND r.state IN ('succeeded', 'partial', 'running')) \
          ORDER BY assets.created_at \
          LIMIT $3",
    )
    .bind(pipeline)
    .bind(pipeline_version)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(asset_id, filename, object_key, proxy_mime)| Candidate {
            asset_id,
            filename,
            object_key,
            proxy_mime,
        })
        .collect())
}

/// How many assets a backfill still has to do, and how many it has done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillProgress {
    pub outstanding: i64,
    pub described: i64,
    /// Runs waiting on a batch that has not ended.
    pub in_flight: i64,
}

/// The counts a progress display needs, in one round trip.
pub async fn backfill_progress(
    conn: &mut sqlx::PgConnection,
    pipeline: &str,
    pipeline_version: i32,
) -> Result<BackfillProgress, Error> {
    let row: (i64, i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM assets \
               JOIN derivatives d ON d.asset_id = assets.id AND d.role = 'proxy' \
              WHERE assets.deleted_at IS NULL AND assets.is_current AND assets.attached_to IS NULL \
                AND NOT EXISTS (SELECT 1 FROM enrichment_runs r \
                                 WHERE r.asset_id = assets.id AND r.pipeline = $1 \
                                   AND r.pipeline_version = $2 \
                                   AND r.state IN ('succeeded', 'partial', 'running'))), \
            (SELECT count(DISTINCT asset_id) FROM enrichment_runs \
              WHERE pipeline = $1 AND pipeline_version = $2 AND state IN ('succeeded', 'partial')), \
            (SELECT count(*) FROM enrichment_runs \
              WHERE pipeline = $1 AND pipeline_version = $2 AND state = 'running' \
                AND llm_batch_id IS NOT NULL)",
    )
    .bind(pipeline)
    .bind(pipeline_version)
    .fetch_one(&mut *conn)
    .await?;
    Ok(BackfillProgress {
        outstanding: row.0,
        described: row.1,
        in_flight: row.2,
    })
}

/// Records that a run was submitted as part of a batch.
///
/// Written *before* the batch is submitted, because results come back keyed by `custom_id` and unordered: a
/// mapping held only in memory would be lost to any restart, leaving a paid batch nobody could read.
pub async fn mark_batched(
    conn: &mut sqlx::PgConnection,
    run_id: Uuid,
    custom_id: &str,
) -> Result<(), Error> {
    sqlx::query("UPDATE enrichment_runs SET llm_custom_id = $2 WHERE id = $1")
        .bind(run_id)
        .bind(custom_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Stamps the provider's batch id onto every run in a batch, once it has one.
pub async fn attach_batch(
    conn: &mut sqlx::PgConnection,
    run_ids: &[Uuid],
    batch_id: &str,
) -> Result<(), Error> {
    sqlx::query("UPDATE enrichment_runs SET llm_batch_id = $2 WHERE id = ANY($1)")
        .bind(run_ids)
        .bind(batch_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The runs belonging to one batch that are still open, as `(run_id, asset_id, custom_id)`.
pub async fn runs_in_batch(
    conn: &mut sqlx::PgConnection,
    batch_id: &str,
) -> Result<Vec<(Uuid, Uuid, String)>, Error> {
    Ok(sqlx::query_as(
        "SELECT id, asset_id, COALESCE(llm_custom_id, id::text) FROM enrichment_runs \
          WHERE llm_batch_id = $1 AND state = 'running'",
    )
    .bind(batch_id)
    .fetch_all(&mut *conn)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn micro_cents_become_the_columns_four_decimal_places() {
        // §8.3's per-asset shape on Opus 5: 2.25 cents.
        assert_eq!(cents(2_250_000), "2.2500");
        // A small model's call, which whole cents would have rounded to nothing.
        assert_eq!(cents(450_000), "0.4500");
        assert_eq!(cents(0), "0.0000");
        // Below the column's resolution: rounded, not truncated to zero, so a very cheap call still registers.
        assert_eq!(cents(50), "0.0001");
        assert_eq!(cents(49), "0.0000");
        // And a large backfill total stays exact rather than drifting through a float.
        assert_eq!(cents(20_000_000_000_000), "20000000.0000");
    }
}
