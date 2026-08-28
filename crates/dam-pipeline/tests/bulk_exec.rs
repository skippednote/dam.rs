//! Executing bulk operations end to end.
//!
//! `dam_db::bulk`'s own suite proves the bookkeeping; this proves the *driver* — that an operation actually
//! changes assets, that the guards hold per item rather than per operation, and that the terminal state is
//! derived from what really happened. Postgres only: neither executable kind touches the object store, and a
//! SeaweedFS container here would be fourteen seconds of startup for nothing.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::TenantSlug;
use dam_db::bulk::{self, OperationSpec};
use dam_db::{TenantConn, migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    tenant: PgPool,
    slug: TenantSlug,
    tenant_id: Uuid,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let global = pg.pool().clone();
    let tenant = pg.pool_for_schema("t_acme").await.expect("tenant pool");

    let tenant_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), 'acme', 't_acme', 'Acme', 'acme/', 'active') RETURNING id",
    )
    .fetch_one(&global)
    .await
    .expect("tenant row");

    Fixture {
        _pg: pg,
        global,
        tenant,
        slug: TenantSlug::new("acme").expect("slug"),
        tenant_id,
    }
}

async fn asset(f: &Fixture, filename: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(filename.as_bytes()).to_hex().to_string())
    .bind(filename)
    .execute(&f.tenant)
    .await
    .expect("asset");
    id
}

async fn field(f: &Fixture, key: &str) {
    sqlx::query(
        "INSERT INTO field_defs (id, key, label, kind, display_order) \
         VALUES (gen_random_uuid(), $1, $1, 'text', 1) ON CONFLICT (key) DO NOTHING",
    )
    .bind(key)
    .execute(&f.tenant)
    .await
    .expect("field");
}

async fn operation(f: &Fixture, kind: &str, params: serde_json::Value, targets: &[Uuid]) -> Uuid {
    let mut conn = TenantConn::begin(&f.global, &f.slug).await.expect("conn");
    let op = bulk::create_on(
        conn.executor(),
        &OperationSpec {
            kind,
            actor_id: None,
            predicate: None,
            params,
        },
        targets,
    )
    .await
    .expect("create");
    conn.commit().await.expect("commit");
    op.id
}

async fn run(f: &Fixture, id: Uuid) -> dam_pipeline::Result<dam_pipeline::bulk_exec::Executed> {
    dam_pipeline::bulk_exec::run(&f.global, &f.slug, id, chrono::Utc::now(), async || Ok(())).await
}

// ─── delete ─────────────────────────────────────────────────────────────────

async fn a_bulk_delete_deletes_what_it_may_and_reports_the_rest(f: &Fixture) {
    let plain_one = asset(f, "del-1.jpg").await;
    let plain_two = asset(f, "del-2.jpg").await;
    let held = asset(f, "del-held.jpg").await;
    sqlx::query("UPDATE assets SET legal_hold = true WHERE id = $1")
        .bind(held)
        .execute(&f.tenant)
        .await
        .expect("hold");
    let gone_already = asset(f, "del-gone.jpg").await;
    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(gone_already)
        .execute(&f.tenant)
        .await
        .expect("pre-delete");
    let never_existed = Uuid::new_v4();

    let id = operation(
        f,
        "delete",
        serde_json::json!({}),
        &[plain_one, plain_two, held, gone_already, never_existed],
    )
    .await;
    let executed = run(f, id).await.expect("run");

    // The counters say exactly what happened: two deleted, one hard failure (the phantom id), two skips that
    // count as neither — which is why done + failed < target here, deliberately.
    assert_eq!(executed.done, 2);
    assert_eq!(executed.failed, 1);
    assert_eq!(
        executed.state, "partial",
        "one failure means partial, and a UI cannot put a green tick over it"
    );

    // The changes themselves.
    let deleted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assets WHERE id = ANY($1) AND deleted_at IS NOT NULL",
    )
    .bind(vec![plain_one, plain_two])
    .fetch_one(&f.tenant)
    .await
    .expect("count");
    assert_eq!(deleted, 2);

    let held_row: (bool, bool) =
        sqlx::query_as("SELECT legal_hold, deleted_at IS NULL FROM assets WHERE id = $1")
            .bind(held)
            .fetch_one(&f.tenant)
            .await
            .expect("held row");
    assert!(
        held_row.1,
        "the legal-held asset survives — the hold did its job"
    );

    // The per-item report: the schema's whole point is "exactly which rows did not apply".
    let items = bulk::items(&f.tenant, id, 100).await.expect("items");
    let by_id = |target: Uuid| {
        items
            .iter()
            .find(|item| item.asset_id == target)
            .expect("every target has an item")
    };
    assert_eq!(by_id(held).state, "skipped");
    assert_eq!(
        by_id(held).reason.as_deref(),
        Some("legal hold blocks deletion")
    );
    assert_eq!(by_id(gone_already).state, "skipped");
    assert_eq!(
        by_id(gone_already).reason.as_deref(),
        Some("already deleted")
    );
    assert_eq!(by_id(never_existed).state, "failed");
    assert_eq!(
        by_id(never_existed).reason.as_deref(),
        Some("no such asset")
    );

    // Only what changed is reported for re-indexing. Re-indexing a skipped asset is wasted work; *not*
    // re-indexing a deleted one leaves a ghost in every search result.
    let mut touched = executed.touched.clone();
    touched.sort_unstable();
    let mut expected = vec![plain_one, plain_two];
    expected.sort_unstable();
    assert_eq!(touched, expected);
}

async fn re_running_a_finished_operation_changes_nothing(f: &Fixture) {
    // The queue is at-least-once, so this is the normal case rather than the odd one.
    let target = asset(f, "rerun.jpg").await;
    let id = operation(f, "delete", serde_json::json!({}), &[target]).await;

    let first = run(f, id).await.expect("first");
    assert_eq!(first.state, "completed");
    let second = run(f, id).await.expect("second");
    assert_eq!(second.state, "completed");
    assert_eq!(second.done, 1, "the counters must not move on a re-run");
    assert!(
        second.touched.is_empty(),
        "and nothing is re-touched, so nothing is pointlessly re-indexed"
    );
}

// ─── metadata_set ───────────────────────────────────────────────────────────

async fn a_bulk_metadata_set_merges_into_every_target(f: &Fixture) {
    field(f, "campaign").await;
    field(f, "caption").await;

    let fresh = asset(f, "meta-fresh.jpg").await;
    let existing = asset(f, "meta-existing.jpg").await;
    sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2)")
        .bind(existing)
        .bind(serde_json::json!({"caption": "keep me", "campaign": "old"}))
        .execute(&f.tenant)
        .await
        .expect("seed metadata");

    let id = operation(
        f,
        "metadata_set",
        serde_json::json!({"values": {"campaign": "spring-2026"}}),
        &[fresh, existing],
    )
    .await;
    let executed = run(f, id).await.expect("run");
    assert_eq!(executed.state, "completed");
    assert_eq!(executed.done, 2);

    // A merge, not a replacement: the field not named survives.
    let kept: serde_json::Value =
        sqlx::query_scalar("SELECT values FROM asset_metadata WHERE asset_id = $1")
            .bind(existing)
            .fetch_one(&f.tenant)
            .await
            .expect("values");
    assert_eq!(kept["campaign"], "spring-2026");
    assert_eq!(kept["caption"], "keep me", "an absent key is left alone");

    // An asset with no metadata row at all gains one.
    let gained: serde_json::Value =
        sqlx::query_scalar("SELECT values FROM asset_metadata WHERE asset_id = $1")
            .bind(fresh)
            .fetch_one(&f.tenant)
            .await
            .expect("values");
    assert_eq!(gained["campaign"], "spring-2026");
}

/// A bulk edit takes the field away from the model, exactly as the single-asset endpoint does.
///
/// This is the drift that motivated moving the write into `dam_db::metadata`: the two paths were separate,
/// this one never dropped the provenance, and a field a model had written stayed marked as machine output
/// after a person overwrote it in bulk. Nothing failed — the value was right and the marking was a lie, so
/// every AI disclosure built on it said a model wrote a sentence a person had replaced.
async fn a_bulk_edit_takes_the_field_back_from_the_model(f: &Fixture) {
    field(f, "alt_text").await;
    let target = asset(f, "meta-provenance.jpg").await;

    // A model wrote this one, and said so.
    sqlx::query("INSERT INTO asset_metadata (asset_id, values, provenance) VALUES ($1, $2, $3)")
        .bind(target)
        .bind(serde_json::json!({"alt_text": "what the model saw"}))
        .bind(serde_json::json!({"alt_text": {"model": "claude-test", "kind": "vision"}}))
        .execute(&f.tenant)
        .await
        .expect("seed a model-written field");

    let id = operation(
        f,
        "metadata_set",
        serde_json::json!({"values": {"alt_text": "what a person wrote instead"}}),
        &[target],
    )
    .await;
    assert_eq!(run(f, id).await.expect("run").state, "completed");

    let (values, provenance): (serde_json::Value, serde_json::Value) =
        sqlx::query_as("SELECT values, provenance FROM asset_metadata WHERE asset_id = $1")
            .bind(target)
            .fetch_one(&f.tenant)
            .await
            .expect("metadata");
    assert_eq!(values["alt_text"], "what a person wrote instead");
    assert!(
        provenance.get("alt_text").is_none(),
        "a person overwrote it in bulk, so the model's claim on it has to go: {provenance}"
    );
}

async fn a_target_whose_type_excludes_the_field_is_reported_not_written(f: &Fixture) {
    // With metadata types, "does this patch apply" stops being a property of the patch alone (Q.1): a field on
    // the image form is not on the archive form, and a selection spanning both is legitimately partial. The
    // pre-flight check still catches what is wrong for everyone; this is the per-item half.
    //
    // Reported rather than silently skipped, and reported rather than written: a bulk edit that could write
    // outside an asset's form while the single-asset endpoint refuses it would make the two paths disagree
    // about what the schema means.
    field(f, "print_dpi").await;
    let narrow = dam_db::metadata_types::define(
        &f.tenant,
        dam_db::metadata_types::NewType {
            key: "captions-only".to_owned(),
            label: "Captions only".to_owned(),
            applies_to: vec![],
            is_default: false,
            field_keys: vec!["caption".to_owned()],
        },
    )
    .await
    .expect("define type");

    let inside = asset(f, "type-ok.jpg").await;
    let outside = asset(f, "type-narrow.jpg").await;
    dam_db::metadata_types::assign(&f.tenant, outside, Some(narrow.id))
        .await
        .expect("assign");

    let id = operation(
        f,
        "metadata_set",
        serde_json::json!({ "values": { "print_dpi": "300" } }),
        &[inside, outside],
    )
    .await;
    let executed = run(f, id).await.expect("run");

    // `partial`, which is the state that exists precisely so a UI cannot put a green tick over this.
    assert_eq!(executed.state, "partial", "one applied, one refused");
    assert_eq!(executed.done, 1);
    assert_eq!(executed.failed, 1);

    // The untyped asset took the value.
    let applied: serde_json::Value =
        sqlx::query_scalar("SELECT values FROM asset_metadata WHERE asset_id = $1")
            .bind(inside)
            .fetch_one(&f.tenant)
            .await
            .expect("values");
    assert_eq!(applied["print_dpi"], "300");

    // The narrow one did not, and the reason names the field rather than saying "failed".
    let stored: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT values FROM asset_metadata WHERE asset_id = $1")
            .bind(outside)
            .fetch_optional(&f.tenant)
            .await
            .expect("query");
    assert!(
        stored.is_none() || stored.as_ref().and_then(|v| v.get("print_dpi")).is_none(),
        "a field outside the asset's type must not be written: {stored:?}"
    );
    let reason: Option<String> = sqlx::query_scalar(
        "SELECT reason FROM bulk_operation_items WHERE operation_id = $1 AND asset_id = $2",
    )
    .bind(id)
    .bind(outside)
    .fetch_one(&f.tenant)
    .await
    .expect("reason");
    let reason = reason.unwrap_or_default();
    assert!(
        reason.contains("print_dpi") && reason.contains("metadata type"),
        "the reason should name the field and why: {reason:?}"
    );

    dam_db::metadata_types::remove(&f.tenant, narrow.id)
        .await
        .expect("remove type");
}

async fn an_invalid_patch_fails_before_any_asset_is_touched(f: &Fixture) {
    // The patch is identical for every target, so a bad one fails all 40,000 identically. Saying it once —
    // permanently, with the field named — beats recording the same failure per item.
    let target = asset(f, "meta-invalid.jpg").await;
    let id = operation(
        f,
        "metadata_set",
        serde_json::json!({"values": {"not_a_field": "x"}}),
        &[target],
    )
    .await;

    let error = run(f, id)
        .await
        .expect_err("an undefined field cannot be set in bulk");
    assert!(
        !error.is_transient(),
        "retrying will not define the field: {error}"
    );
    assert!(
        format!("{error}").contains("no asset was touched"),
        "the failure says the operation never started: {error}"
    );

    // And that claim is checked, not just stated.
    let items = bulk::items(&f.tenant, id, 10).await.expect("items");
    assert!(items.iter().all(|item| item.state == "pending"));
}

// ─── refusals ───────────────────────────────────────────────────────────────

async fn an_unimplemented_kind_is_refused_by_name(f: &Fixture) {
    // The schema's vocabulary is wider than what is executable. "Completing" while doing nothing would put a
    // success in the history for work that never happened — the named refusal is the honest gap.
    let target = asset(f, "unimpl.jpg").await;
    let id = operation(f, "download_zip", serde_json::json!({}), &[target]).await;

    let error = run(f, id)
        .await
        .expect_err("download_zip has no executor yet");
    assert!(!error.is_transient());
    assert!(format!("{error}").contains("download_zip"), "{error}");
}

async fn a_vanished_operation_is_permanent(f: &Fixture) {
    let error = run(f, Uuid::new_v4()).await.expect_err("no such operation");
    assert!(!error.is_transient(), "{error}");
}

// ─── through the worker ─────────────────────────────────────────────────────

async fn the_worker_runs_it_and_queues_the_reindex(f: &Fixture) {
    let dir = tempfile::tempdir().expect("tempdir");
    let context = dam_pipeline::worker::Context {
        // No hosted-model context: these suites are about the queue and the render stages.
        ai: None,
        scanner: None,
        signing_identity: None,
        global: f.global.clone(),
        store: std::sync::Arc::new(dam_store::FakeS3Store::with_test_clock().0),
        indexes: std::sync::Arc::new(dam_search::IndexPool::new(dam_search::PoolConfig::new(
            dir.path(),
        ))),
        worker: "bulk-test-worker".to_owned(),
        // No webhook subscriptions in these fixtures, so nothing is ever dispatched. A default client
        // rather than a builder, because what these suites exercise is unrelated to how it is configured.
        http: reqwest::Client::new(),
    };

    let target = asset(f, "via-worker.jpg").await;
    let untouchable = asset(f, "via-worker-held.jpg").await;
    sqlx::query("UPDATE assets SET legal_hold = true WHERE id = $1")
        .bind(untouchable)
        .execute(&f.tenant)
        .await
        .expect("hold");

    let op = operation(f, "delete", serde_json::json!({}), &[target, untouchable]).await;
    let job_id = dam_pipeline::worker::enqueue_bulk(&f.global, f.tenant_id, op)
        .await
        .expect("enqueue");

    let claimed = dam_db::jobs::claim(
        &f.global,
        "bulk-test-worker",
        dam_db::jobs::ClaimOptions::default(),
    )
    .await
    .expect("claim");
    let job = claimed
        .iter()
        .find(|j| j.id == job_id)
        .expect("the bulk job");
    assert_eq!(job.kind, dam_pipeline::worker::kind::BULK);
    dam_pipeline::worker::handle(&context, job)
        .await
        .expect("handle");
    dam_db::jobs::complete(&f.global, job_id)
        .await
        .expect("complete");

    // The deleted asset — and only it — has an index job waiting, so search stops returning a ghost.
    let queued: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT kind, payload FROM dam_global.jobs WHERE state = 'queued' AND kind = 'index'",
    )
    .fetch_all(&f.global)
    .await
    .expect("queued");
    assert_eq!(queued.len(), 1, "one reindex, for the one changed asset");
    assert_eq!(
        queued[0].1["asset_id"].as_str().expect("an id"),
        target.to_string()
    );
}

#[tokio::test]
async fn bulk_execution_holds() {
    let f = fixture().await;
    a_bulk_delete_deletes_what_it_may_and_reports_the_rest(&f).await;
    re_running_a_finished_operation_changes_nothing(&f).await;
    a_bulk_metadata_set_merges_into_every_target(&f).await;
    a_bulk_edit_takes_the_field_back_from_the_model(&f).await;
    a_target_whose_type_excludes_the_field_is_reported_not_written(&f).await;
    an_invalid_patch_fails_before_any_asset_is_touched(&f).await;
    an_unimplemented_kind_is_refused_by_name(&f).await;
    a_vanished_operation_is_permanent(&f).await;
    the_worker_runs_it_and_queues_the_reindex(&f).await;
    publishing_stamps_once_and_unpublishing_clears_it(&f).await;
    // Last, and in this order: the first leaves a subscription behind that the second removes.
    a_bulk_change_lands_in_the_outbox(&f).await;
    no_subscription_means_no_queue(&f).await;
    archiving_moves_only_what_is_active_and_says_why_not(&f).await;
}

// ─── archive / unarchive (§6.4's curation half) ─────────────────────────────

/// The curation status, which is not the storage tier.
///
/// `status = 'archived'` means out of circulation and still instantly fetchable; `storage_class = 'GLACIER'`
/// means cheap and slow. A library archives what it has finished with and tiers what nobody reads, and those
/// are frequently different sets — so they are separate columns, changed by separate machinery, and this case
/// only touches the first.
///
/// Added because a mutation sweep pointed out that dropping the `WHERE` guards entirely broke no test: the
/// three new kinds had been wired through `bulk_exec` with no coverage at all, so `archive` was free to
/// overwrite `deleted` and to archive an asset that was still mid-pipeline.
async fn archiving_moves_only_what_is_active_and_says_why_not(f: &Fixture) {
    let active = asset(f, "arch-active.jpg").await;
    let already = asset(f, "arch-already.jpg").await;
    sqlx::query("UPDATE assets SET status = 'archived' WHERE id = $1")
        .bind(already)
        .execute(&f.tenant)
        .await
        .expect("pre-archive");
    let processing = asset(f, "arch-processing.jpg").await;
    sqlx::query("UPDATE assets SET status = 'processing' WHERE id = $1")
        .bind(processing)
        .execute(&f.tenant)
        .await
        .expect("mid-pipeline");
    let deleted = asset(f, "arch-deleted.jpg").await;
    sqlx::query("UPDATE assets SET deleted_at = now(), status = 'deleted' WHERE id = $1")
        .bind(deleted)
        .execute(&f.tenant)
        .await
        .expect("pre-delete");

    let id = operation(
        f,
        "archive",
        serde_json::json!({}),
        &[active, already, processing, deleted],
    )
    .await;
    let executed = run(f, id).await.expect("run");
    assert_eq!(executed.done, 1, "only the active one moves");
    assert_eq!(executed.failed, 0, "and none of the others is a *failure*");

    let statuses = |ids: Vec<Uuid>| async move {
        sqlx::query_as::<_, (Uuid, String)>("SELECT id, status FROM assets WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(&f.tenant)
            .await
            .expect("statuses")
    };
    let rows = statuses(vec![active, processing, deleted]).await;
    let status_of = |target: Uuid| {
        rows.iter()
            .find(|(id, _)| *id == target)
            .map(|(_, status)| status.as_str())
            .expect("a row")
    };
    assert_eq!(status_of(active), "archived");
    assert_eq!(
        status_of(processing),
        "processing",
        "archiving something mid-pipeline would strand the job working on it",
    );
    assert_eq!(
        status_of(deleted),
        "deleted",
        "and a deleted asset is already out of circulation; saying `archived` over it would be a status \
         nobody asked for",
    );

    // Each skip says which of the three it was, because a run over a mixed selection is expected to skip and
    // "3 skipped" with no reasons is a number nobody can account for.
    let items = bulk::items(&f.tenant, id, 100).await.expect("items");
    let reason = |target: Uuid| {
        items
            .iter()
            .find(|item| item.asset_id == target)
            .expect("every target has an item")
            .reason
            .clone()
    };
    assert_eq!(reason(already).as_deref(), Some("already archived"));
    assert_eq!(reason(processing).as_deref(), Some("still processing"));
    assert_eq!(reason(deleted).as_deref(), Some("deleted"));

    // And back again, which is the same action with a direction.
    let back = operation(f, "unarchive", serde_json::json!({}), &[active, already]).await;
    let executed = run(f, back).await.expect("run");
    assert_eq!(executed.done, 2, "both were archived, so both come back");
    assert_eq!(statuses(vec![active]).await[0].1, "active",);
}

// ─── the webhook outbox (Q.20c) ─────────────────────────────────────────────

/// Every state change a consumer cares about lands in the outbox, in the transaction that made it.
///
/// The point of testing this *here* rather than against `enqueue` directly: a webhook system whose producers
/// are never called is a schema with no code, which is what the outbox was for its entire life before Q.20c.
/// So this drives the real bulk operations and reads the queue afterwards.
///
/// And it asserts the *absence* that matters: a no-op emits nothing. A consumer invalidating a cache on every
/// re-publication of an already-published asset is a consumer doing our idempotence for us.
async fn a_bulk_change_lands_in_the_outbox(f: &Fixture) {
    let subscription: Uuid = sqlx::query_scalar(
        "INSERT INTO webhook_subscriptions (id, url, secret) \
         VALUES (gen_random_uuid(), 'https://example.test/hook', 'k') RETURNING id",
    )
    .fetch_one(&f.tenant)
    .await
    .expect("subscription");

    let queued = |kind: &'static str| {
        let pool = f.tenant.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM webhook_deliveries WHERE event_kind = $1",
            )
            .bind(kind)
            .fetch_one(&pool)
            .await
            .expect("count")
        }
    };

    let one = asset(f, "outbox-one.jpg").await;
    let two = asset(f, "outbox-two.jpg").await;

    let id = operation(f, "publish", serde_json::json!({}), &[one, two]).await;
    run(f, id).await.expect("run");
    assert_eq!(
        queued("asset.published").await,
        2,
        "one event per asset that changed"
    );

    // The no-op. `one` is already published, so publishing it again changes nothing and must announce nothing.
    let again = operation(f, "publish", serde_json::json!({}), &[one]).await;
    run(f, again).await.expect("run");
    assert_eq!(
        queued("asset.published").await,
        2,
        "a re-publication that changed nothing must not emit an event"
    );

    let off = operation(f, "unpublish", serde_json::json!({}), &[one]).await;
    run(f, off).await.expect("run");
    assert_eq!(queued("asset.unpublished").await, 1);

    let archived = operation(f, "archive", serde_json::json!({}), &[two]).await;
    run(f, archived).await.expect("run");
    assert_eq!(queued("asset.status_changed").await, 1);

    let edited = operation(
        f,
        "metadata_set",
        serde_json::json!({"values": {"caption": "a harbour at dawn"}}),
        &[one],
    )
    .await;
    run(f, edited).await.expect("run");
    assert_eq!(queued("asset.metadata_updated").await, 1);
    // The keys, never the values: a tenant's metadata has no business in a delivery log or in whatever the
    // receiver writes its request bodies to.
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM webhook_deliveries WHERE event_kind = 'asset.metadata_updated'",
    )
    .fetch_one(&f.tenant)
    .await
    .expect("payload");
    assert_eq!(payload["detail"]["fields"], serde_json::json!(["caption"]));
    let rendered = payload.to_string();
    assert!(
        !rendered.contains("harbour at dawn"),
        "the value must not travel: {rendered}"
    );

    let removed = operation(f, "delete", serde_json::json!({}), &[one]).await;
    run(f, removed).await.expect("run");
    assert_eq!(queued("asset.deleted").await, 1);

    // Every event went to the one subscription, and each carries the asset it is about — which is what the
    // per-asset ordering rule keys on.
    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries WHERE subscription_id = $1")
            .bind(subscription)
            .fetch_one(&f.tenant)
            .await
            .expect("count");
    assert_eq!(total, 6);
    let without_asset: i64 =
        sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries WHERE asset_id IS NULL")
            .fetch_one(&f.tenant)
            .await
            .expect("count");
    assert_eq!(without_asset, 0, "an asset event names its asset");
}

/// With no subscription there is no queue, and the operations still work.
///
/// The common case — most deployments have no webhooks — and the one where a bug would be a per-asset insert
/// into a table nobody reads. `enqueue` is a single `INSERT … SELECT` over the subscriptions precisely so that
/// this costs one statement that matches nothing rather than a read plus a write per asset.
async fn no_subscription_means_no_queue(f: &Fixture) {
    sqlx::query("DELETE FROM webhook_subscriptions")
        .execute(&f.tenant)
        .await
        .expect("clear");
    sqlx::query("DELETE FROM webhook_deliveries")
        .execute(&f.tenant)
        .await
        .expect("clear");

    let one = asset(f, "unwatched.jpg").await;
    let id = operation(f, "publish", serde_json::json!({}), &[one]).await;
    let executed = run(f, id).await.expect("run");
    assert_eq!(executed.state, "completed");
    assert_eq!(
        executed.done, 1,
        "the operation is unaffected by nobody listening"
    );

    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries")
        .fetch_one(&f.tenant)
        .await
        .expect("count");
    assert_eq!(queued, 0);
}

// ─── publish / unpublish (Q.14) ─────────────────────────────────────────────

/// Publication is the act a live-query portal rests on, so the instant it happened is the audit answer.
///
/// Re-publishing an already-published asset is a **skip** rather than a success: restamping would lose the
/// instant somebody decided, and "since when has this been public" is the question the column exists to
/// answer.
async fn publishing_stamps_once_and_unpublishing_clears_it(f: &Fixture) {
    let one = asset(f, "publish-one.jpg").await;
    let two = asset(f, "publish-two.jpg").await;

    let id = operation(f, "publish", serde_json::json!({}), &[one, two]).await;
    let executed = run(f, id).await.expect("run");
    assert_eq!(executed.state, "completed");
    assert_eq!(executed.done, 2);

    let first: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT published_at FROM assets WHERE id = $1")
            .bind(one)
            .fetch_one(&f.tenant)
            .await
            .expect("published_at");
    assert!(first.is_some(), "the asset was not published");

    // Again, over one already-published asset and one that is not.
    let three = asset(f, "publish-three.jpg").await;
    let again = operation(f, "publish", serde_json::json!({}), &[one, three]).await;
    let executed = run(f, again).await.expect("run");
    assert_eq!(
        executed.state, "completed",
        "a skip is not a failure: {executed:?}"
    );
    let unchanged: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT published_at FROM assets WHERE id = $1")
            .bind(one)
            .fetch_one(&f.tenant)
            .await
            .expect("published_at");
    assert_eq!(unchanged, first, "an already-published asset was restamped");

    // And unpublishing clears it over a mixed selection — including an asset nobody ever published — without
    // reporting a partial failure. "Not on a public page" is what the caller asked for, and an asset that was
    // already off it satisfies that; failing there would make an unpublish over a grid selection look broken
    // for doing exactly what it was told.
    let never = asset(f, "publish-never.jpg").await;
    let off = operation(f, "unpublish", serde_json::json!({}), &[two, three, never]).await;
    let executed = run(f, off).await.expect("run");
    assert_eq!(
        executed.state, "completed",
        "an asset that was already unpublished must not fail: {executed:?}"
    );
    assert_eq!(executed.done, 3, "{executed:?}");
    let cleared: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assets WHERE id = ANY($1) AND published_at IS NOT NULL",
    )
    .bind(vec![two, three])
    .fetch_one(&f.tenant)
    .await
    .expect("count");
    assert_eq!(cleared, 0, "unpublish left one published");
}
