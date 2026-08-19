//! Attached documents (Q.9).
//!
//! An attachment is an ordinary `assets` row marked as belonging to another — see migration 0022 for why. So the
//! properties worth testing are the ones that make it *not* an ordinary asset:
//!
//! - **Paperwork is not in the library.** It shares the `LIBRARY_ROWS` clause with superseded versions, which is
//!   the point of them sharing one clause: two rules that have to be applied in the same four places.
//! - **It is still readable by id**, and still access-checked, because the whole reason for attaching a release is
//!   that somebody can go and read it.
//! - **The document list is filtered on the documents**, not only on the parent: an attachment can be in a
//!   different asset group, and one outside the caller's scope must not be reported as existing.
//! - **The constraints refuse incoherent rows.** Half an attachment, a document attached to itself, or paperwork
//!   about paperwork are all refused by the database rather than by convention.
//!
//! One container; cases are functions over a borrowed pool. See the note in `provenance.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::query::{Planned, Query};
use dam_db::attachments::{self, AttachmentRefusal, Kind};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

fn access(groups: &[Uuid], all: bool) -> policy::AccessPredicate {
    policy::compile(
        &Grants::from(vec![Grant {
            permissions: vec!["asset:read".to_owned()],
            asset_group_ids: groups.to_vec(),
            all_asset_groups: all,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        }]),
        Action::Read,
        Utc::now(),
    )
}

fn everything() -> policy::AccessPredicate {
    access(&[], true)
}

fn planned(predicate: policy::AccessPredicate) -> Planned {
    Planned::new(Query::All, predicate, &[]).expect("plan")
}

macro_rules! c {
    ($pool:expr) => {
        &mut *$pool.acquire().await.expect("connection")
    };
}

async fn asset(pool: &PgPool, label: &str, group: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(label.as_bytes()).to_hex().to_string())
    .bind(format!("{label}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    if let Some(group) = group {
        sqlx::query("INSERT INTO asset_group_members (asset_id, group_id) VALUES ($1, $2)")
            .bind(id)
            .bind(group)
            .execute(pool)
            .await
            .expect("membership");
    }
    id
}

async fn group(pool: &PgPool, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO asset_groups (id, key, label) VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .expect("group")
}

#[tokio::test]
async fn the_attachment_contract_holds() {
    let (_pg, pool) = db().await;

    let press = group(&pool, "press").await;
    let photo = asset(&pool, "portrait", Some(press)).await;
    let release = asset(&pool, "model-release", Some(press)).await;

    attaching_takes_it_out_of_the_library(&pool, photo, release).await;
    an_attachment_is_still_readable_by_id(&pool, release).await;
    the_document_list_is_filtered_on_the_documents(&pool, press).await;
    detaching_returns_it_to_the_library(&pool, photo, release).await;
    incoherent_attachments_are_refused_by_the_database(&pool, photo).await;
    paperwork_cannot_have_paperwork(&pool, photo, release).await;
    a_document_cannot_be_attached_twice(&pool, photo, release).await;
    a_superseded_version_is_not_a_document(&pool).await;
    which_have_answers_for_a_page_at_once(&pool, photo).await;
    neither_side_can_be_out_of_the_callers_scope(&pool).await;
}

async fn neither_side_can_be_out_of_the_callers_scope(pool: &PgPool) {
    // Both directions. Attaching is a relationship, so a caller must not be able to make one out of a row they
    // cannot see — whether it is the parent or the document. Only the document side was covered, and mutation
    // testing said the parent check could be deleted without a test noticing.
    let mine = group(pool, "attach-scope").await;
    let visible_doc = asset(pool, "scope-doc", Some(mine)).await;
    let hidden_parent = asset(pool, "scope-hidden-parent", None).await;
    let visible_parent = asset(pool, "scope-parent", Some(mine)).await;
    let hidden_doc = asset(pool, "scope-hidden-doc", None).await;

    let scoped = access(&[mine], false);

    let refusal = attachments::attach(c!(pool), hidden_parent, visible_doc, Kind::Release, &scoped)
        .await
        .expect_err("parent out of scope");
    assert!(
        matches!(refusal, AttachmentRefusal::UnknownAsset(id) if id == hidden_parent),
        "{refusal:?}"
    );

    let refusal = attachments::attach(c!(pool), visible_parent, hidden_doc, Kind::Release, &scoped)
        .await
        .expect_err("document out of scope");
    assert!(
        matches!(refusal, AttachmentRefusal::UnknownAsset(id) if id == hidden_doc),
        "{refusal:?}"
    );

    // Neither refusal attached anything.
    let attached: i64 =
        sqlx::query_scalar("SELECT count(*) FROM assets WHERE attached_to = ANY($1)")
            .bind(vec![hidden_parent, visible_parent])
            .fetch_one(pool)
            .await
            .expect("count");
    assert_eq!(attached, 0, "a refused attach wrote anyway");
}

async fn attaching_takes_it_out_of_the_library(pool: &PgPool, photo: Uuid, release: Uuid) {
    // Both are ordinary assets to begin with, which is what makes the change observable.
    let before = dam_db::assets::page(pool, &everything(), dam_db::assets::Order::Newest, 0, 50)
        .await
        .expect("page");
    let names: Vec<&str> = before.items.iter().map(|i| i.filename.as_str()).collect();
    assert!(names.contains(&"model-release.jpg"), "{names:?}");

    let attached = attachments::attach(c!(pool), photo, release, Kind::Release, &everything())
        .await
        .expect("attach");
    assert_eq!(attached.len(), 1);
    assert_eq!(attached[0].asset_id, release);
    assert_eq!(attached[0].kind, Kind::Release);

    // Out of the grid, out of the search page, and out of the dashboard's count — the three places that describe
    // the library. Nobody browses to a release form.
    let after = dam_db::assets::page(pool, &everything(), dam_db::assets::Order::Newest, 0, 50)
        .await
        .expect("page");
    let names: Vec<&str> = after.items.iter().map(|i| i.filename.as_str()).collect();
    assert!(
        !names.contains(&"model-release.jpg"),
        "paperwork is in the library: {names:?}"
    );
    assert!(names.contains(&"portrait.jpg"), "{names:?}");
    assert_eq!(after.total, before.total - 1);

    let searched = dam_db::assets::page_matching(
        pool,
        &planned(everything()),
        dam_db::assets::Order::Newest,
        0,
        50,
    )
    .await
    .expect("page_matching");
    let searched_names: Vec<&str> = searched.items.iter().map(|i| i.filename.as_str()).collect();
    assert!(
        !searched_names.contains(&"model-release.jpg"),
        "the search path shows paperwork: {searched_names:?}"
    );

    let summary = dam_db::events::summary(c!(pool), &planned(everything()))
        .await
        .expect("summary");
    assert_eq!(summary.assets, after.total, "{summary:?}");
}

async fn an_attachment_is_still_readable_by_id(pool: &PgPool, release: Uuid) {
    // The entire reason for attaching a release is that somebody can go and read it. Hidden from listings is not
    // hidden from the person checking whether they may use the photograph.
    let found = dam_db::assets::detail(pool, &everything(), release)
        .await
        .expect("detail");
    assert!(found.is_some(), "an attachment is unreadable by id");

    // And still access-checked, so "readable by id" is not "readable by anybody".
    let elsewhere = group(pool, "elsewhere").await;
    let refused = dam_db::assets::detail(pool, &access(&[elsewhere], false), release)
        .await
        .expect("query");
    assert!(refused.is_none(), "an attachment escaped the predicate");
}

async fn the_document_list_is_filtered_on_the_documents(pool: &PgPool, press: Uuid) {
    // A parent in the caller's scope with a document outside it. The predicate has to apply to the *document*, or
    // listing the parent's paperwork would report that something exists which the caller may not see.
    let parent = asset(pool, "campaign-shot", Some(press)).await;
    let secret = asset(pool, "secret-contract", None).await;
    attachments::attach(c!(pool), parent, secret, Kind::Contract, &everything())
        .await
        .expect("attach");

    let wide = attachments::on_asset(c!(pool), parent, &everything())
        .await
        .expect("list");
    assert_eq!(wide.len(), 1);

    let narrow = attachments::on_asset(c!(pool), parent, &access(&[press], false))
        .await
        .expect("list");
    assert!(
        narrow.is_empty(),
        "a document outside the caller's scope was listed: {narrow:?}"
    );
}

async fn detaching_returns_it_to_the_library(pool: &PgPool, photo: Uuid, release: Uuid) {
    attachments::detach(c!(pool), release, &everything())
        .await
        .expect("detach");

    // Not a delete: the row and its bytes stay, and it is an ordinary asset again. Somebody correcting a
    // mis-attachment does not want a destructive verb.
    let page = dam_db::assets::page(pool, &everything(), dam_db::assets::Order::Newest, 0, 50)
        .await
        .expect("page");
    let names: Vec<&str> = page.items.iter().map(|i| i.filename.as_str()).collect();
    assert!(names.contains(&"model-release.jpg"), "{names:?}");

    let remaining = attachments::on_asset(c!(pool), photo, &everything())
        .await
        .expect("list");
    assert!(remaining.is_empty(), "{remaining:?}");

    // Re-attach for the cases that follow.
    attachments::attach(c!(pool), photo, release, Kind::Release, &everything())
        .await
        .expect("re-attach");
}

async fn incoherent_attachments_are_refused_by_the_database(pool: &PgPool, photo: Uuid) {
    // Half an attachment: attached to something with no kind, or a kind with nothing to be attached to. A row like
    // that is one no screen can render honestly, so the column constraint refuses it rather than a convention.
    let orphan = asset(pool, "half-attached", None).await;
    let refused = sqlx::query("UPDATE assets SET attached_to = $2 WHERE id = $1")
        .bind(orphan)
        .bind(photo)
        .execute(pool)
        .await;
    assert!(refused.is_err(), "an attachment with no kind was accepted");

    let refused = sqlx::query("UPDATE assets SET attachment_kind = 'release' WHERE id = $1")
        .bind(orphan)
        .execute(pool)
        .await;
    assert!(refused.is_err(), "a kind with no parent was accepted");

    // And attached to itself, which would make "not in the library" a self-referential question.
    let refused =
        sqlx::query("UPDATE assets SET attached_to = id, attachment_kind = 'other' WHERE id = $1")
            .bind(orphan)
            .execute(pool)
            .await;
    assert!(
        refused.is_err(),
        "a document attached to itself was accepted"
    );
}

async fn paperwork_cannot_have_paperwork(pool: &PgPool, photo: Uuid, release: Uuid) {
    let second = asset(pool, "release-appendix", None).await;
    // `release` is already paperwork for `photo`. Attaching to it would make the library-exclusion rule a chain to
    // walk rather than a column to check.
    let refusal = attachments::attach(c!(pool), release, second, Kind::Other, &everything())
        .await
        .expect_err("parent is an attachment");
    assert!(
        matches!(refusal, AttachmentRefusal::ParentIsAttachment),
        "{refusal:?}"
    );
    let _ = photo;
}

async fn a_document_cannot_be_attached_twice(pool: &PgPool, photo: Uuid, release: Uuid) {
    let other_photo = asset(pool, "second-portrait", None).await;
    // A release form attached to two assets by accident is a rights mistake. Refused by name rather than silently
    // moved, because moving it would hide the first attachment.
    let refusal = attachments::attach(c!(pool), other_photo, release, Kind::Release, &everything())
        .await
        .expect_err("already attached");
    assert!(
        matches!(refusal, AttachmentRefusal::AlreadyAttached(id) if id == release),
        "{refusal:?}"
    );

    // And the original attachment is untouched.
    let still = attachments::on_asset(c!(pool), photo, &everything())
        .await
        .expect("list");
    assert_eq!(still.len(), 1, "{still:?}");

    // Re-attaching to the *same* parent is not an error: it is how the kind gets corrected.
    let again = attachments::attach(c!(pool), photo, release, Kind::Permit, &everything())
        .await
        .expect("same parent");
    assert_eq!(again[0].kind, Kind::Permit);
}

async fn a_superseded_version_is_not_a_document(pool: &PgPool) {
    let first = asset(pool, "flyer-v1", None).await;
    let second = asset(pool, "flyer-v2", None).await;
    dam_db::versions::add(c!(pool), first, second, &everything())
        .await
        .expect("version");

    let parent = asset(pool, "flyer-parent", None).await;
    // `first` is now a superseded version. A row cannot be both that and a release form: the two say different
    // things about why it is absent from the library, and a screen would have to guess which.
    let refusal = attachments::attach(c!(pool), parent, first, Kind::Other, &everything())
        .await
        .expect_err("a version is not a document");
    assert!(
        matches!(refusal, AttachmentRefusal::IsAVersion(id) if id == first),
        "{refusal:?}"
    );
}

async fn which_have_answers_for_a_page_at_once(pool: &PgPool, photo: Uuid) {
    let bare = asset(pool, "no-paperwork", None).await;
    let found = attachments::which_have(c!(pool), &[photo, bare], &everything())
        .await
        .expect("which_have");
    assert!(found.contains(&photo), "{found:?}");
    assert!(!found.contains(&bare), "{found:?}");

    // Scoped: an asset whose only paperwork is outside the caller's scope has none as far as they can tell.
    let elsewhere = group(pool, "which-have-scope").await;
    let hidden_parent = asset(pool, "scoped-parent", Some(elsewhere)).await;
    let hidden_doc = asset(pool, "scoped-doc", None).await;
    attachments::attach(
        c!(pool),
        hidden_parent,
        hidden_doc,
        Kind::Licence,
        &everything(),
    )
    .await
    .expect("attach");
    let scoped = attachments::which_have(c!(pool), &[hidden_parent], &access(&[elsewhere], false))
        .await
        .expect("which_have");
    assert!(
        scoped.is_empty(),
        "an out-of-scope document was reported as existing: {scoped:?}"
    );

    // And an empty request is an empty answer rather than a query.
    let none = attachments::which_have(c!(pool), &[], &everything())
        .await
        .expect("which_have");
    assert!(none.is_empty());
}
