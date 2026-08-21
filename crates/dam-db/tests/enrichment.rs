//! Machine-written values, and the rules that make them undoable (M5b).
//!
//! The unit tests cover the arithmetic. What needs a database is every rule that is a *relationship* between
//! rows:
//!
//! - **A model writes only what the tenant allows**, and what it refused is reported rather than dropped.
//! - **A model never overwrites a person.** A field with no provenance was typed by somebody, and a re-run must
//!   not undo their edit.
//! - **A person's edit removes the marking**, or the disclosure claims a human sentence is machine output.
//! - **A tag is a suggestion, and a decided tag stays decided** — a rejected term must not reappear in the queue
//!   somebody just cleared.
//! - **The cost lands on the run**, in a `numeric` that a fraction of a cent survives.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::enrichment::{self, Attribution, Cost, Outcome, Source};
use dam_db::{migrate, testing::PostgresHarness};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    // The fields a model may write, and one it may not. `copyright` is the realistic case: a tenant that lets a
    // model draft a description will not let it invent a rights holder.
    for (key, kind, ai_writable) in [
        ("alt_text", "text", true),
        ("description", "textarea", true),
        ("copyright", "text", false),
    ] {
        sqlx::query(
            "INSERT INTO field_defs (id, key, label, kind, ai_writable) VALUES ($1, $2, $2, $3, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(key)
        .bind(kind)
        .bind(ai_writable)
        .execute(&pool)
        .await
        .expect("field def");
    }
    (pg, pool)
}

macro_rules! c {
    ($pool:expr) => {
        &mut *$pool.acquire().await.expect("connection")
    };
}

async fn asset(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

fn attribution() -> Attribution {
    Attribution {
        source: Source::Llm,
        model: "claude-opus-5-20260601".to_owned(),
        model_version: "llm_describe/1".to_owned(),
        confidence: 0.72,
    }
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

async fn values(pool: &PgPool, asset_id: Uuid) -> serde_json::Value {
    sqlx::query_scalar("SELECT values FROM asset_metadata WHERE asset_id = $1")
        .bind(asset_id)
        .fetch_one(pool)
        .await
        .expect("values")
}

async fn provenance(pool: &PgPool, asset_id: Uuid) -> serde_json::Value {
    sqlx::query_scalar("SELECT provenance FROM asset_metadata WHERE asset_id = $1")
        .bind(asset_id)
        .fetch_one(pool)
        .await
        .expect("provenance")
}

#[tokio::test]
async fn a_written_value_carries_everything_needed_to_undo_it() {
    let (_pg, pool) = db().await;
    let id = asset(&pool, "one").await;
    let at = now();

    let written = enrichment::write_values(
        c!(pool),
        id,
        &[
            ("alt_text".to_owned(), json!("A runner on a wet path")),
            ("description".to_owned(), json!("Two sentences about it.")),
        ],
        &attribution(),
        at,
    )
    .await
    .expect("write");
    assert_eq!(written.written, vec!["alt_text", "description"]);
    assert!(written.refused.is_empty());

    assert_eq!(
        values(&pool, id).await["alt_text"],
        "A runner on a wet path"
    );
    let marking = provenance(&pool, id).await;
    // 0001's contract, in full. A partial record is not undoable: without the model you cannot scope a
    // regression, and without the version you cannot tell which prompt produced it.
    assert_eq!(marking["alt_text"]["source"], "llm");
    assert_eq!(marking["alt_text"]["model"], "claude-opus-5-20260601");
    assert_eq!(marking["alt_text"]["model_version"], "llm_describe/1");
    assert!(
        (marking["alt_text"]["confidence"]
            .as_f64()
            .expect("confidence")
            - 0.72)
            .abs()
            < 0.001
    );
    assert!(
        marking["alt_text"]["reviewed_by"].is_null(),
        "nobody has yet"
    );
    assert_eq!(
        marking["alt_text"]["at"].as_str().expect("at"),
        at.to_rfc3339()
    );

    // And the asset itself moved, or nothing watching it reindexes.
    let updated: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM assets WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("updated_at");
    assert!(updated >= at - chrono::Duration::seconds(5));
}

#[tokio::test]
async fn a_field_the_tenant_withholds_is_refused_and_reported() {
    let (_pg, pool) = db().await;
    let id = asset(&pool, "two").await;
    let written = enrichment::write_values(
        c!(pool),
        id,
        &[
            ("copyright".to_owned(), json!("© Somebody")),
            ("nonexistent".to_owned(), json!("x")),
            ("alt_text".to_owned(), json!("Something")),
        ],
        &attribution(),
        now(),
    )
    .await
    .expect("write");

    assert_eq!(written.written, vec!["alt_text"]);
    assert_eq!(written.refused, vec!["copyright", "nonexistent"]);
    // Not stored at all, not stored-and-flagged: `values` is a jsonb column, so nothing downstream would ever
    // have stopped it.
    assert!(values(&pool, id).await.get("copyright").is_none());
    assert!(provenance(&pool, id).await.get("copyright").is_none());
}

#[tokio::test]
async fn a_model_does_not_overwrite_a_person() {
    let (_pg, pool) = db().await;
    let id = asset(&pool, "three").await;
    // A human value: present in `values`, absent from `provenance`. That absence is the signal.
    sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2)")
        .bind(id)
        .bind(json!({"alt_text": "What a person wrote"}))
        .execute(&pool)
        .await
        .expect("human value");

    let written = enrichment::write_values(
        c!(pool),
        id,
        &[
            ("alt_text".to_owned(), json!("What the model wrote")),
            ("description".to_owned(), json!("A new field")),
        ],
        &attribution(),
        now(),
    )
    .await
    .expect("write");

    assert_eq!(written.kept_human, vec!["alt_text"]);
    assert_eq!(written.written, vec!["description"]);
    assert_eq!(
        values(&pool, id).await["alt_text"],
        "What a person wrote",
        "a re-run must not undo an edit"
    );
    assert!(provenance(&pool, id).await.get("alt_text").is_none());
}

#[tokio::test]
async fn a_model_may_replace_its_own_earlier_answer() {
    let (_pg, pool) = db().await;
    let id = asset(&pool, "four").await;
    enrichment::write_values(
        c!(pool),
        id,
        &[("alt_text".to_owned(), json!("First attempt"))],
        &attribution(),
        now(),
    )
    .await
    .expect("first");

    // A better prompt, a newer model. Its own previous answer is not a decision anybody made, so it goes.
    let second = Attribution {
        model_version: "llm_describe/2".to_owned(),
        ..attribution()
    };
    let written = enrichment::write_values(
        c!(pool),
        id,
        &[("alt_text".to_owned(), json!("Second attempt"))],
        &second,
        now(),
    )
    .await
    .expect("second");

    assert_eq!(written.written, vec!["alt_text"]);
    assert_eq!(values(&pool, id).await["alt_text"], "Second attempt");
    assert_eq!(
        provenance(&pool, id).await["alt_text"]["model_version"],
        "llm_describe/2"
    );
}

#[tokio::test]
async fn a_persons_edit_removes_the_marking() {
    let (_pg, pool) = db().await;
    let id = asset(&pool, "five").await;
    enrichment::write_values(
        c!(pool),
        id,
        &[
            ("alt_text".to_owned(), json!("Machine text")),
            ("description".to_owned(), json!("Machine description")),
        ],
        &attribution(),
        now(),
    )
    .await
    .expect("write");

    enrichment::forget_provenance(c!(pool), id, &["alt_text".to_owned()])
        .await
        .expect("forget");

    let marking = provenance(&pool, id).await;
    assert!(
        marking.get("alt_text").is_none(),
        "a rewritten field is not machine output any more"
    );
    assert!(
        marking.get("description").is_some(),
        "and the fields nobody touched keep theirs"
    );

    // The disclosure query sees exactly one machine-written field now.
    let disclosed = enrichment::machine_written(c!(pool), id)
        .await
        .expect("disclosure");
    assert_eq!(disclosed.len(), 1);
    assert_eq!(disclosed[0].0, "description");
    assert_eq!(disclosed[0].1["source"], "llm");
}

#[tokio::test]
async fn nothing_written_writes_nothing() {
    let (_pg, pool) = db().await;
    let id = asset(&pool, "six").await;
    let written = enrichment::write_values(
        c!(pool),
        id,
        &[("copyright".to_owned(), json!("© Nobody"))],
        &attribution(),
        now(),
    )
    .await
    .expect("write");
    assert!(written.written.is_empty());
    // No metadata row at all: an empty row with an `updated_at` would make an asset look edited by a call that
    // changed nothing.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_metadata WHERE asset_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(rows, 0);
}

/// A taxonomy with two terms, returning their ids by slug.
async fn taxonomy(pool: &PgPool) -> (Uuid, Uuid) {
    let taxonomy_id = Uuid::now_v7();
    sqlx::query("INSERT INTO taxonomies (id, key, label) VALUES ($1, 'subject', 'Subject')")
        .bind(taxonomy_id)
        .execute(pool)
        .await
        .expect("taxonomy");
    let mut ids = Vec::new();
    for slug in ["footwear", "outdoor"] {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO taxonomy_terms (id, taxonomy_id, path, slug, label) \
             VALUES ($1, $2, text2ltree($3), $3, $3)",
        )
        .bind(id)
        .bind(taxonomy_id)
        .bind(slug)
        .execute(pool)
        .await
        .expect("term");
        ids.push(id);
    }
    (ids[0], ids[1])
}

async fn model_id(pool: &PgPool) -> Uuid {
    enrichment::register_model(
        c!(pool),
        "claude-opus-5",
        "llm_describe/1",
        "llm",
        "api",
        &json!({"pipeline": "llm_describe"}),
    )
    .await
    .expect("model")
}

#[tokio::test]
async fn a_registered_model_is_one_row_however_often_it_is_registered() {
    let (_pg, pool) = db().await;
    let first = model_id(&pool).await;
    let second = model_id(&pool).await;
    assert_eq!(
        first, second,
        "the unique index on (key, version) is the point"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_models")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn an_llm_tag_is_a_suggestion_and_a_decision_is_left_alone() {
    let (_pg, pool) = db().await;
    let id = asset(&pool, "tags").await;
    let (footwear, outdoor) = taxonomy(&pool).await;
    let model = model_id(&pool).await;

    let tagged = enrichment::suggest_tags(
        c!(pool),
        id,
        &[
            "footwear".to_owned(),
            "outdoor".to_owned(),
            "invented".to_owned(),
        ],
        model,
        0.8,
    )
    .await
    .expect("suggest");
    assert_eq!(tagged.suggested, vec![footwear, outdoor]);
    assert_eq!(tagged.unknown, vec!["invented"]);

    let (state, source, votes): (String, String, i16) = sqlx::query_as(
        "SELECT state, source, generator_votes FROM asset_tags WHERE asset_id = $1 AND term_id = $2",
    )
    .bind(id)
    .bind(footwear)
    .fetch_one(&pool)
    .await
    .expect("tag");
    assert_eq!(state, "suggested", "never confirmed, whatever it claimed");
    assert_eq!(source, "llm");
    assert_eq!(votes, 1);

    // A person rejects one and confirms the other.
    sqlx::query("UPDATE asset_tags SET state = 'rejected' WHERE asset_id = $1 AND term_id = $2")
        .bind(id)
        .bind(footwear)
        .execute(&pool)
        .await
        .expect("reject");
    sqlx::query("UPDATE asset_tags SET state = 'confirmed' WHERE asset_id = $1 AND term_id = $2")
        .bind(id)
        .bind(outdoor)
        .execute(&pool)
        .await
        .expect("confirm");

    // A re-run proposes both again. Neither decision moves: the rejected one must not reappear in a queue
    // somebody cleared, and the confirmed one must not be demoted back to a suggestion.
    let again = enrichment::suggest_tags(
        c!(pool),
        id,
        &["footwear".to_owned(), "outdoor".to_owned()],
        model,
        0.9,
    )
    .await
    .expect("again");
    assert_eq!(again.decided.len(), 2);
    assert!(again.suggested.is_empty());
    let states: Vec<String> =
        sqlx::query_scalar("SELECT state FROM asset_tags WHERE asset_id = $1 ORDER BY term_id")
            .bind(id)
            .fetch_all(&pool)
            .await
            .expect("states");
    assert_eq!(states.len(), 2);
    assert!(states.contains(&"rejected".to_owned()));
    assert!(states.contains(&"confirmed".to_owned()));
}

#[tokio::test]
async fn a_second_generator_seconds_a_suggestion_rather_than_duplicating_it() {
    let (_pg, pool) = db().await;
    let id = asset(&pool, "votes").await;
    let (footwear, _) = taxonomy(&pool).await;
    let model = model_id(&pool).await;

    enrichment::suggest_tags(c!(pool), id, &["footwear".to_owned()], model, 0.4)
        .await
        .expect("first");
    let seconded = enrichment::suggest_tags(c!(pool), id, &["footwear".to_owned()], model, 0.9)
        .await
        .expect("second");
    assert_eq!(seconded.seconded, vec![footwear]);

    let (votes, confidence): (i16, f32) = sqlx::query_as(
        "SELECT generator_votes, confidence FROM asset_tags WHERE asset_id = $1 AND term_id = $2",
    )
    .bind(id)
    .bind(footwear)
    .fetch_one(&pool)
    .await
    .expect("tag");
    // Two votes, one row — the review queue sorts on agreement, and a duplicate row would be a second decision
    // to make about the same tag.
    assert_eq!(votes, 2);
    assert!(
        (confidence - 0.9).abs() < 0.001,
        "the stronger claim is kept"
    );

    // And in the other order: a weaker third proposal must not overwrite what the strongest one claimed. Only
    // this direction catches a plain assignment, which is why it is here rather than implied by the above.
    enrichment::suggest_tags(c!(pool), id, &["footwear".to_owned()], model, 0.2)
        .await
        .expect("third");
    let confidence: f32 = sqlx::query_scalar(
        "SELECT confidence FROM asset_tags WHERE asset_id = $1 AND term_id = $2",
    )
    .bind(id)
    .bind(footwear)
    .fetch_one(&pool)
    .await
    .expect("confidence");
    assert!(
        (confidence - 0.9).abs() < 0.001,
        "a weaker claim overwrote a stronger one: {confidence}"
    );
}

#[tokio::test]
async fn a_run_records_what_it_cost_to_four_decimal_places() {
    let (_pg, pool) = db().await;
    let id = asset(&pool, "run").await;
    let run = enrichment::start_run(c!(pool), id, "llm_describe", 1)
        .await
        .expect("start");

    // The state before it finishes matters: a crashed worker leaves `running`, which is what
    // `enrichment_runs_state_idx` is for.
    let state: String = sqlx::query_scalar("SELECT state FROM enrichment_runs WHERE id = $1")
        .bind(run)
        .fetch_one(&pool)
        .await
        .expect("state");
    assert_eq!(state, "running");

    enrichment::finish_run(
        c!(pool),
        run,
        Outcome::Partial,
        Cost {
            input_tokens: 900,
            output_tokens: 120,
            cached_tokens: 800,
            micro_cents: 450_000,
        },
        &json!({"describe": {"state": "ok"}, "refused": ["copyright"]}),
        None,
        false,
    )
    .await
    .expect("finish");

    let (state, input, cached, cost, used_original, duration): (
        String,
        i64,
        i64,
        String,
        bool,
        Option<i32>,
    ) = sqlx::query_as(
        "SELECT state, input_tokens, cached_tokens, est_cost_cents::text, used_original, duration_ms \
           FROM enrichment_runs WHERE id = $1",
    )
    .bind(run)
    .fetch_one(&pool)
    .await
    .expect("run");
    assert_eq!(state, "partial");
    assert_eq!(input, 900);
    // The field that says prompt caching is working. A run without it cannot be told from a full-price call.
    assert_eq!(cached, 800);
    assert_eq!(cost, "0.4500", "a fraction of a cent survives the column");
    assert!(!used_original, "a stage reading masters is a restore storm");
    assert!(duration.is_some());
}

#[tokio::test]
async fn the_review_queue_orders_by_agreement_then_confidence() {
    let (_pg, pool) = db().await;
    let id = asset(&pool, "queued").await;
    let (footwear, outdoor) = taxonomy(&pool).await;
    let model = model_id(&pool).await;
    // Deliberately at odds: the twice-proposed tag claims *less* than the once-proposed one, so a queue that
    // sorted on the claim would put outdoor first. Two generators agreeing is the stronger evidence, and this
    // is the only arrangement of numbers that can tell the two orderings apart.
    enrichment::suggest_tags(c!(pool), id, &["footwear".to_owned()], model, 0.3)
        .await
        .expect("first");
    enrichment::suggest_tags(c!(pool), id, &["footwear".to_owned()], model, 0.4)
        .await
        .expect("seconded");
    enrichment::suggest_tags(c!(pool), id, &["outdoor".to_owned()], model, 0.9)
        .await
        .expect("outdoor");
    enrichment::write_values(
        c!(pool),
        id,
        &[("alt_text".to_owned(), json!("Machine text"))],
        &attribution(),
        now(),
    )
    .await
    .expect("value");

    // An administrator's predicate: every group, so the queue's own filtering is what is under test rather
    // than the group membership.
    let predicate = dam_core::policy::compile(
        &dam_core::policy::Grants::from(vec![dam_core::policy::Grant {
            permissions: vec!["asset:manage".to_owned()],
            asset_group_ids: Vec::new(),
            all_asset_groups: true,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        }]),
        dam_core::policy::Action::Manage,
        chrono::Utc::now(),
    );
    let queue = enrichment::review_queue(c!(pool), &predicate, 50)
        .await
        .expect("queue");
    assert_eq!(queue.len(), 1);
    let item = &queue[0];
    assert_eq!(item.asset_id, id);
    // Two votes before one, whatever the confidence says: footwear claims 0.4 and outdoor claims 0.9.
    assert_eq!(item.suggested[0].term_id, footwear);
    assert_eq!(item.suggested[0].votes, 2);
    assert_eq!(item.suggested[1].term_id, outdoor);
    assert_eq!(item.fields.len(), 1);
    assert_eq!(item.fields[0].key, "alt_text");
    assert!(!item.fields[0].reviewed);
}

/// Closing a run moves the asset's own state, which is what makes "awaiting review" answerable.
///
/// Found by describing real photographs and looking at the rows: every asset ever enriched still said
/// `pending`, the same as one nobody had touched. The column's migration says it exists so a screen can show
/// "awaiting review" without a join and so the review queue is an index scan — and nothing wrote it, so the
/// index was over a column with a single value in it.
#[tokio::test]
async fn closing_a_run_moves_the_assets_own_state() {
    let (_pg, pool) = db().await;

    async fn state_after(pool: &PgPool, name: &str, outcome: Outcome) -> String {
        let id = asset(pool, name).await;
        let run = enrichment::start_run(c!(pool), id, "llm_describe", 1)
            .await
            .expect("start");
        enrichment::finish_run(
            c!(pool),
            run,
            outcome,
            Cost {
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: 0,
                micro_cents: 1_000,
            },
            &json!({"describe": {"state": "ok"}}),
            None,
            false,
        )
        .await
        .expect("finish");
        sqlx::query_scalar("SELECT enrichment_state FROM assets WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("state")
    }

    // Written cleanly, nothing to decide.
    assert_eq!(state_after(&pool, "s-ok", Outcome::Succeeded).await, "done");
    // Something refused or a human value kept: a person has to look, which is the queue.
    assert_eq!(
        state_after(&pool, "s-partial", Outcome::Partial).await,
        "needs_review"
    );
    // Tried and failed, which a retry sweep must be able to tell from never tried.
    assert_eq!(
        state_after(&pool, "s-failed", Outcome::Failed).await,
        "failed"
    );
    // A skip has enriched nothing — over a cap, no credential — so the asset is still waiting. Marking it
    // anything else would hide work that still has to happen.
    assert_eq!(
        state_after(&pool, "s-skipped", Outcome::Skipped).await,
        "pending"
    );
}
