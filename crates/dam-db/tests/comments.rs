//! Comments on assets: public and private, routed, with statuses (Q.6).
//!
//! The storage is two ordinary tables. Every case here is about the *composition of two gates*, which is where
//! this can go wrong in ways nobody notices:
//!
//! - **The asset gate is outside the visibility gate.** A comment addressed to you on an asset you cannot see must
//!   be unreachable. The other order — find what is addressed to me, then check the assets — discloses the
//!   existence of assets through the comments hanging off them.
//! - **Being addressed is not a grant.** A recipient who loses access to the asset stops seeing the comment.
//! - **Naming somebody on a public comment does not narrow it,** and naming them on a private one does not widen
//!   what they can otherwise see.
//! - **"Not yours" is never reachable before "no such comment",** or the pair becomes an existence oracle.
//!
//! One container; cases are functions over a borrowed pool. See the note in `provenance.rs`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::query::{Planned, Query};
use dam_db::comments::{self, CommentRefusal, NewComment, Status, Visibility};
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

fn grants(groups: &[Uuid], all: bool) -> policy::AccessPredicate {
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

fn everything() -> Planned {
    Planned::new(Query::All, grants(&[], true), &[]).expect("plan")
}

fn scoped(group: Uuid) -> Planned {
    Planned::new(Query::All, grants(&[group], false), &[]).expect("plan")
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

fn note(asset_id: Uuid, author: Uuid, body: &str) -> NewComment {
    NewComment {
        asset_id,
        author_id: author,
        body: body.to_owned(),
        visibility: Visibility::Public,
        recipients: vec![],
        parent_id: None,
    }
}

#[tokio::test]
async fn the_comment_contract_holds() {
    let (_pg, pool) = db().await;

    let press = group(&pool, "press").await;
    let shared = asset(&pool, "shared", Some(press)).await;
    let embargoed = asset(&pool, "embargoed", None).await;

    let ada = Uuid::new_v4();
    let grace = Uuid::new_v4();
    let mallory = Uuid::new_v4();

    a_public_comment_is_readable_by_anyone_who_sees_the_asset(&pool, shared, ada, grace).await;
    a_private_comment_reaches_its_author_and_recipients_only(&pool, shared, ada, grace, mallory)
        .await;
    a_private_comment_with_no_recipients_is_refused(&pool, shared, ada).await;
    naming_somebody_does_not_widen_what_they_can_see(&pool, embargoed, ada, grace, press).await;
    losing_access_to_the_asset_takes_the_comment_with_it(&pool, shared, ada, grace, press).await;
    a_reply_threads_under_its_parent_and_cannot_be_replied_to(&pool, shared, ada, grace).await;
    a_reply_must_be_on_the_same_asset_as_its_parent(&pool, shared, embargoed, ada).await;
    a_status_can_be_moved_by_a_reader_and_records_who(&pool, shared, ada, grace).await;
    only_the_author_may_change_the_words(&pool, shared, ada, grace).await;
    an_unreadable_comment_is_unknown_before_it_is_not_yours(&pool, shared, ada, mallory).await;
    a_body_outside_the_bounds_is_refused(&pool, shared, ada).await;
    deleting_takes_the_replies_with_it(&pool, shared, ada, grace).await;
}

async fn a_public_comment_is_readable_by_anyone_who_sees_the_asset(
    pool: &PgPool,
    shared: Uuid,
    ada: Uuid,
    grace: Uuid,
) {
    let posted = comments::post(
        c!(pool),
        note(shared, ada, "The crop is tight"),
        &everything(),
    )
    .await
    .expect("post");
    assert_eq!(posted.visibility, Visibility::Public);
    assert_eq!(posted.status, Status::Open, "a new comment is open");
    assert!(posted.edited_at.is_none(), "and not edited");

    // Grace has read the asset, so she reads the comment. No routing needed for a public one.
    let seen = comments::on_asset(c!(pool), shared, grace, &everything())
        .await
        .expect("list");
    assert!(seen.iter().any(|c| c.id == posted.id), "{seen:?}");
}

async fn a_private_comment_reaches_its_author_and_recipients_only(
    pool: &PgPool,
    shared: Uuid,
    ada: Uuid,
    grace: Uuid,
    mallory: Uuid,
) {
    let posted = comments::post(
        c!(pool),
        NewComment {
            visibility: Visibility::Private,
            recipients: vec![grace],
            ..note(shared, ada, "Legal has not cleared this yet")
        },
        &everything(),
    )
    .await
    .expect("post");
    assert_eq!(posted.recipients, vec![grace]);

    // The author and the named recipient.
    for (who, label) in [(ada, "author"), (grace, "recipient")] {
        let seen = comments::on_asset(c!(pool), shared, who, &everything())
            .await
            .expect("list");
        assert!(
            seen.iter().any(|c| c.id == posted.id),
            "{label} cannot read it: {seen:?}"
        );
    }

    // And nobody else — even with full access to the asset, which is the whole point of "private".
    let seen = comments::on_asset(c!(pool), shared, mallory, &everything())
        .await
        .expect("list");
    assert!(
        !seen.iter().any(|c| c.id == posted.id),
        "a private comment reached somebody it was not addressed to: {seen:?}"
    );
    // Deliberately *not* "and administrators": see NEEDS-REVIEW.md. `everything()` is the widest predicate there
    // is, and it does not open a private comment.
    let refusal = comments::read(c!(pool), posted.id, mallory, &everything())
        .await
        .expect_err("not for mallory");
    assert!(
        matches!(refusal, CommentRefusal::UnknownComment(id) if id == posted.id),
        "{refusal:?}"
    );
}

async fn a_private_comment_with_no_recipients_is_refused(pool: &PgPool, shared: Uuid, ada: Uuid) {
    // A note only its author could ever read is not a private comment; it is one that failed silently.
    let refusal = comments::post(
        c!(pool),
        NewComment {
            visibility: Visibility::Private,
            recipients: vec![],
            ..note(shared, ada, "for nobody")
        },
        &everything(),
    )
    .await
    .expect_err("no recipients");
    assert!(
        matches!(refusal, CommentRefusal::PrivateWithNoRecipients),
        "{refusal:?}"
    );

    // And nothing was written by the refused post.
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM asset_comments WHERE body = 'for nobody'")
            .fetch_one(pool)
            .await
            .expect("count");
    assert_eq!(count, 0);
}

async fn naming_somebody_does_not_widen_what_they_can_see(
    pool: &PgPool,
    embargoed: Uuid,
    ada: Uuid,
    grace: Uuid,
    press: Uuid,
) {
    // Ada can see everything, so she can comment on the embargoed asset and address it to Grace.
    let posted = comments::post(
        c!(pool),
        NewComment {
            visibility: Visibility::Private,
            recipients: vec![grace],
            ..note(embargoed, ada, "Do not release before Friday")
        },
        &everything(),
    )
    .await
    .expect("post");

    // Grace is scoped to `press`, and the embargoed asset is in no group. The comment names her, and that is
    // routing — not a grant. The asset gate is outside the visibility gate, so she cannot reach it.
    let refusal = comments::read(c!(pool), posted.id, grace, &scoped(press))
        .await
        .expect_err("outside her scope");
    assert!(
        matches!(refusal, CommentRefusal::UnknownComment(id) if id == posted.id),
        "being addressed granted access to an asset: {refusal:?}"
    );

    // And she cannot list them either, which is the query that would leak the asset's existence.
    let refusal = comments::on_asset(c!(pool), embargoed, grace, &scoped(press))
        .await
        .expect_err("outside her scope");
    assert!(
        matches!(refusal, CommentRefusal::UnknownAsset(id) if id == embargoed),
        "{refusal:?}"
    );
}

async fn losing_access_to_the_asset_takes_the_comment_with_it(
    pool: &PgPool,
    shared: Uuid,
    ada: Uuid,
    grace: Uuid,
    press: Uuid,
) {
    let posted = comments::post(
        c!(pool),
        NewComment {
            visibility: Visibility::Private,
            recipients: vec![grace],
            ..note(shared, ada, "Swap the hero image")
        },
        &everything(),
    )
    .await
    .expect("post");

    // While she can see the asset, she can read it.
    comments::read(c!(pool), posted.id, grace, &scoped(press))
        .await
        .expect("in her scope");

    // Narrowed to a group the asset is not in — as an access change would do — and it is gone. The recipient row
    // still exists; what changed is the gate outside it.
    let elsewhere = group(pool, "elsewhere").await;
    let refusal = comments::read(c!(pool), posted.id, grace, &scoped(elsewhere))
        .await
        .expect_err("no longer hers to read");
    assert!(
        matches!(refusal, CommentRefusal::UnknownComment(id) if id == posted.id),
        "{refusal:?}"
    );
    let still_routed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM asset_comment_recipients WHERE comment_id = $1 AND identity_id = $2",
    )
    .bind(posted.id)
    .bind(grace)
    .fetch_one(pool)
    .await
    .expect("count");
    assert_eq!(
        still_routed, 1,
        "the routing is intact; only the gate moved"
    );
}

async fn a_reply_threads_under_its_parent_and_cannot_be_replied_to(
    pool: &PgPool,
    shared: Uuid,
    ada: Uuid,
    grace: Uuid,
) {
    let parent = comments::post(
        c!(pool),
        note(shared, ada, "Is this the final grade?"),
        &everything(),
    )
    .await
    .expect("post");
    let reply = comments::post(
        c!(pool),
        NewComment {
            parent_id: Some(parent.id),
            ..note(shared, grace, "Yes, signed off Tuesday")
        },
        &everything(),
    )
    .await
    .expect("reply");
    assert_eq!(reply.parent_id, Some(parent.id));

    // A reply to a reply is refused: arbitrary depth makes every read recursive and every screen an
    // indentation problem, and it is the shape that is hardest to remove later.
    let refusal = comments::post(
        c!(pool),
        NewComment {
            parent_id: Some(reply.id),
            ..note(shared, ada, "Thanks")
        },
        &everything(),
    )
    .await
    .expect_err("too deep");
    assert!(matches!(refusal, CommentRefusal::TooDeep), "{refusal:?}");

    // A *second* thread started between the question and its answer, so ordering by time alone would put it
    // between them. Without this comment the two orderings agree and the grouping is unobservable — the reply
    // happened to be the next thing created, so `ORDER BY created_at` looked correct. Mutation testing said so.
    let interleaved = comments::post(
        c!(pool),
        note(shared, grace, "Unrelated question"),
        &everything(),
    )
    .await
    .expect("post");
    let later = comments::post(
        c!(pool),
        NewComment {
            parent_id: Some(parent.id),
            ..note(shared, ada, "Confirmed by the studio")
        },
        &everything(),
    )
    .await
    .expect("reply");

    // Threads stay together, and within a thread the conversation reads in the order it happened.
    let listed = comments::on_asset(c!(pool), shared, ada, &everything())
        .await
        .expect("list");
    let at = |id: Uuid| listed.iter().position(|c| c.id == id).expect("present");
    assert_eq!(at(reply.id), at(parent.id) + 1, "{listed:?}");
    assert_eq!(at(later.id), at(reply.id) + 1, "{listed:?}");
    assert!(
        at(interleaved.id) > at(later.id),
        "a second thread was interleaved into the first: {listed:?}"
    );
}

async fn a_reply_must_be_on_the_same_asset_as_its_parent(
    pool: &PgPool,
    shared: Uuid,
    other: Uuid,
    ada: Uuid,
) {
    // The parent is readable — same author, same predicate — so nothing about *access* stops this. What stops it
    // is that a reply on another asset would appear in a conversation it is not part of, on a screen that reads
    // every comment as being about the asset it is attached to.
    let parent = comments::post(
        c!(pool),
        note(shared, ada, "On the shared one"),
        &everything(),
    )
    .await
    .expect("post");
    let refusal = comments::post(
        c!(pool),
        NewComment {
            parent_id: Some(parent.id),
            ..note(other, ada, "Replying from somewhere else")
        },
        &everything(),
    )
    .await
    .expect_err("different asset");
    // `UnknownComment` rather than its own refusal: from the caller's side it is the same mistake, and a more
    // specific message would confirm the id exists somewhere they were not asking about.
    assert!(
        matches!(refusal, CommentRefusal::UnknownComment(id) if id == parent.id),
        "{refusal:?}"
    );

    // And nothing was written on the other asset.
    let listed = comments::on_asset(c!(pool), other, ada, &everything())
        .await
        .expect("list");
    assert!(
        !listed
            .iter()
            .any(|c| c.body == "Replying from somewhere else"),
        "{listed:?}"
    );
}

async fn a_status_can_be_moved_by_a_reader_and_records_who(
    pool: &PgPool,
    shared: Uuid,
    ada: Uuid,
    grace: Uuid,
) {
    let posted = comments::post(
        c!(pool),
        note(shared, ada, "Needs a wider crop"),
        &everything(),
    )
    .await
    .expect("post");

    // Grace, not the author. `approved` is somebody else's verdict on what the comment asked for — a status only
    // the author could move could never mean approval.
    let after = comments::set_status(c!(pool), posted.id, grace, Status::Approved, &everything())
        .await
        .expect("status");
    assert_eq!(after.status, Status::Approved);
    // Recorded, because an approval nobody owns is worth nothing in an audit.
    assert_eq!(after.status_by, Some(grace));

    let after = comments::set_status(
        c!(pool),
        posted.id,
        ada,
        Status::ChangesRequested,
        &everything(),
    )
    .await
    .expect("status");
    assert_eq!(after.status, Status::ChangesRequested);
    assert_eq!(
        after.status_by,
        Some(ada),
        "the latest mover, not the first"
    );
}

async fn only_the_author_may_change_the_words(pool: &PgPool, shared: Uuid, ada: Uuid, grace: Uuid) {
    let posted = comments::post(
        c!(pool),
        note(shared, ada, "Lower the highlights"),
        &everything(),
    )
    .await
    .expect("post");

    let amended = comments::amend(
        c!(pool),
        posted.id,
        ada,
        "Lower the highlights a little",
        &everything(),
    )
    .await
    .expect("amend");
    assert_eq!(amended.body, "Lower the highlights a little");
    // Marked, so a screen can say "edited" rather than silently showing different words than whoever replied to
    // it actually read.
    assert!(amended.edited_at.is_some());

    // Grace can *read* it and cannot rewrite it. Status is a shared decision; the words are their author's.
    let refusal = comments::amend(
        c!(pool),
        posted.id,
        grace,
        "Actually it is fine",
        &everything(),
    )
    .await
    .expect_err("not hers");
    assert!(matches!(refusal, CommentRefusal::NotYours), "{refusal:?}");
    let unchanged = comments::read(c!(pool), posted.id, ada, &everything())
        .await
        .expect("read");
    assert_eq!(unchanged.body, "Lower the highlights a little");
}

async fn an_unreadable_comment_is_unknown_before_it_is_not_yours(
    pool: &PgPool,
    shared: Uuid,
    ada: Uuid,
    mallory: Uuid,
) {
    let private = comments::post(
        c!(pool),
        NewComment {
            visibility: Visibility::Private,
            recipients: vec![ada],
            ..note(shared, ada, "Between us")
        },
        &everything(),
    )
    .await
    .expect("post");

    // Mallory cannot read it, so every operation answers "no such comment" — never "not yours", which would
    // confirm that the id exists and that somebody else owns it.
    for label in ["amend", "delete", "status"] {
        let refusal = match label {
            "amend" => comments::amend(c!(pool), private.id, mallory, "changed", &everything())
                .await
                .expect_err("unreadable"),
            "delete" => comments::remove(c!(pool), private.id, mallory, &everything())
                .await
                .expect_err("unreadable"),
            _ => comments::set_status(
                c!(pool),
                private.id,
                mallory,
                Status::Resolved,
                &everything(),
            )
            .await
            .expect_err("unreadable"),
        };
        assert!(
            matches!(refusal, CommentRefusal::UnknownComment(id) if id == private.id),
            "{label}: {refusal:?}"
        );
    }

    // Nothing moved.
    let untouched = comments::read(c!(pool), private.id, ada, &everything())
        .await
        .expect("read");
    assert_eq!(untouched.body, "Between us");
    assert_eq!(untouched.status, Status::Open);
}

async fn a_body_outside_the_bounds_is_refused(pool: &PgPool, shared: Uuid, ada: Uuid) {
    for body in [String::new(), "x".repeat(comments::MAX_BODY_CHARS + 1)] {
        let refusal = comments::post(c!(pool), note(shared, ada, &body), &everything())
            .await
            .expect_err("bad length");
        assert!(
            matches!(refusal, CommentRefusal::BadLength(n) if n == body.chars().count()),
            "{refusal:?}"
        );
    }
    // The boundary itself is accepted, so the bound is inclusive rather than one out.
    let edge = "y".repeat(comments::MAX_BODY_CHARS);
    comments::post(c!(pool), note(shared, ada, &edge), &everything())
        .await
        .expect("the longest allowed comment");
}

async fn deleting_takes_the_replies_with_it(pool: &PgPool, shared: Uuid, ada: Uuid, grace: Uuid) {
    let parent = comments::post(
        c!(pool),
        note(shared, ada, "Which version shipped?"),
        &everything(),
    )
    .await
    .expect("post");
    let reply = comments::post(
        c!(pool),
        NewComment {
            parent_id: Some(parent.id),
            ..note(shared, grace, "The second")
        },
        &everything(),
    )
    .await
    .expect("reply");

    // Grace cannot delete Ada's comment.
    let refusal = comments::remove(c!(pool), parent.id, grace, &everything())
        .await
        .expect_err("not hers");
    assert!(matches!(refusal, CommentRefusal::NotYours), "{refusal:?}");

    comments::remove(c!(pool), parent.id, ada, &everything())
        .await
        .expect("delete");

    // The reply goes with it. A reply to a question that no longer exists is a fragment of a conversation with
    // the question removed, which reads as corruption rather than as a deletion.
    let refusal = comments::read(c!(pool), reply.id, grace, &everything())
        .await
        .expect_err("gone with its parent");
    assert!(
        matches!(refusal, CommentRefusal::UnknownComment(id) if id == reply.id),
        "{refusal:?}"
    );
}
