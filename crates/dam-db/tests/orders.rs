//! Orders: request, decision, pickup (Q.13a).
//!
//! The storage is two tables and a state machine. What is worth defending is the set of rules that make an order
//! an audit trail rather than a form:
//!
//! - **An order grants nothing.** Approval is a decision; the bytes go through a share link created at
//!   fulfilment. `ready` and `approved` are different states for exactly that reason, and the database refuses a
//!   `ready` order with no share.
//! - **You cannot order what you cannot see**, and a partly-visible request narrows rather than refusing — the
//!   requester keeps the nine they may have, and is not told which of the ten was invisible.
//! - **An approver cannot approve what they cannot see.** Agreeing to hand over an asset you cannot inspect is a
//!   signature on a blank page.
//! - **A decision cannot be erased by the person who asked for it.** Cancelling is for before a decision; after
//!   one, the trail is somebody else's recorded act.
//! - **`expired` is derived, never stored**, so it cannot be wrong between sweeps.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{Duration, Utc};
use dam_core::policy::{self, Action, Grant, Grants};
use dam_db::orders::{self, NewOrder, OrderRefusal};
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

macro_rules! c {
    ($pool:expr) => {
        &mut *$pool.acquire().await.expect("connection")
    };
}

fn predicate(groups: &[Uuid], all: bool) -> policy::AccessPredicate {
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

fn order_for(requester: Uuid, ids: &[Uuid]) -> NewOrder {
    NewOrder {
        requested_by: requester,
        purpose: "The spring brochure, print run of 20,000.".to_owned(),
        channel: Some("print".to_owned()),
        territory: Some("GB".to_owned()),
        conversion_key: Some("print-full".to_owned()),
        include_metadata: true,
        recipients: vec!["agency@example.com".to_owned()],
        asset_ids: ids.to_vec(),
    }
}

#[tokio::test]
async fn the_order_lifecycle_holds() {
    let (_pg, pool) = db().await;

    an_order_records_the_ask_and_the_reason(&pool).await;
    a_partly_visible_order_narrows_rather_than_refusing(&pool).await;
    an_order_for_nothing_visible_is_refused(&pool).await;
    an_approver_cannot_judge_what_they_cannot_see(&pool).await;
    a_decision_can_only_be_made_once(&pool).await;
    only_the_requester_cancels_and_only_before_a_decision(&pool).await;
    approved_is_not_ready_until_a_share_exists(&pool).await;
    expiry_is_derived_from_the_decision(&pool).await;
    the_queue_is_oldest_first_and_mine_is_newest_first(&pool).await;
}

async fn an_order_records_the_ask_and_the_reason(pool: &PgPool) {
    let ada = Uuid::new_v4();
    let one = asset(pool, "harbour", None).await;
    let two = asset(pool, "quay", None).await;

    let order = orders::place(
        c!(pool),
        &order_for(ada, &[one, two]),
        &predicate(&[], true),
    )
    .await
    .expect("place");

    // A reference somebody can read aloud, because people talk about orders on the phone.
    assert!(order.reference.starts_with("ORD-"), "{}", order.reference);
    assert_eq!(order.state, "submitted");
    assert_eq!(order.items.len(), 2);
    // The reason travels: it is the entire question an approver answers.
    assert!(order.purpose.contains("spring brochure"));
    // And the two answers the rest of the system wants — the intended use (Q.12) and the format (Q.11).
    assert_eq!(order.channel.as_deref(), Some("print"));
    assert_eq!(order.conversion_key.as_deref(), Some("print-full"));
    assert!(order.include_metadata);
    assert_eq!(order.recipients, vec!["agency@example.com".to_owned()]);
    // Nothing decided, and the constraint holds it that way.
    assert!(order.decided_by.is_none() && order.decided_at.is_none());
    assert!(order.share_link_id.is_none());

    // The filename is copied in, so the order still reads after a rename.
    sqlx::query("UPDATE assets SET filename = 'renamed.jpg' WHERE id = $1")
        .bind(one)
        .execute(pool)
        .await
        .expect("rename");
    let reread = orders::read(c!(pool), order.id)
        .await
        .expect("read")
        .expect("present");
    assert!(
        reread
            .items
            .iter()
            .any(|item| item.filename == "harbour.jpg"),
        "the order shows the new name, so it no longer says what was asked for: {:?}",
        reread.items
    );
}

async fn a_partly_visible_order_narrows_rather_than_refusing(pool: &PgPool) {
    // Nine assets they may have are not lost because of one they may not — and they are not told which one was
    // invisible, which is the enumeration the filter exists to prevent.
    let bob = Uuid::new_v4();
    let visible_group = group(pool, "visible").await;
    let mine = asset(pool, "mine", Some(visible_group)).await;
    let theirs = asset(pool, "theirs", None).await;

    let order = orders::place(
        c!(pool),
        &order_for(bob, &[mine, theirs]),
        &predicate(&[visible_group], false),
    )
    .await
    .expect("place");

    assert_eq!(order.items.len(), 1, "{:?}", order.items);
    assert_eq!(order.items[0].asset_id, mine);
}

async fn an_order_for_nothing_visible_is_refused(pool: &PgPool) {
    let carol = Uuid::new_v4();
    let elsewhere = group(pool, "elsewhere").await;
    let hidden = asset(pool, "hidden", None).await;

    // Distinct from an empty order: this is a request for things that, to them, do not exist.
    let refusal = orders::place(
        c!(pool),
        &order_for(carol, &[hidden]),
        &predicate(&[elsewhere], false),
    )
    .await
    .expect_err("nothing visible");
    assert!(
        matches!(refusal, OrderRefusal::NothingVisible),
        "{refusal:?}"
    );

    let empty = orders::place(c!(pool), &order_for(carol, &[]), &predicate(&[], true))
        .await
        .expect_err("empty");
    assert!(matches!(empty, OrderRefusal::Empty), "{empty:?}");
}

async fn an_approver_cannot_judge_what_they_cannot_see(pool: &PgPool) {
    let dee = Uuid::new_v4();
    let approver = Uuid::new_v4();
    let restricted = group(pool, "restricted").await;
    let open = asset(pool, "open", None).await;
    let closed = asset(pool, "closed", Some(restricted)).await;

    let order = orders::place(
        c!(pool),
        &order_for(dee, &[open, closed]),
        &predicate(&[], true),
    )
    .await
    .expect("place");
    assert_eq!(order.items.len(), 2);

    // An approver who can see only one of the two is agreeing to hand over something they cannot inspect.
    let narrow = group(pool, "narrow-approver").await;
    let refusal = orders::approve(
        c!(pool),
        order.id,
        approver,
        None,
        &predicate(&[narrow], false),
        14,
        Utc::now(),
    )
    .await
    .expect_err("unjudgeable");
    assert!(
        matches!(refusal, OrderRefusal::Unjudgeable(2)),
        "the refusal does not say how many are out of reach: {refusal:?}"
    );

    // Rejection needs no such visibility: saying no to something you cannot see is a defensible answer, and
    // requiring it would leave orders nobody can close.
    let rejected = orders::reject(
        c!(pool),
        order.id,
        approver,
        Some("Not for external use."),
        Utc::now(),
    )
    .await
    .expect("reject");
    assert_eq!(rejected.state, "rejected");
    assert_eq!(rejected.decided_by, Some(approver));
    assert!(rejected.decided_at.is_some());
    assert_eq!(
        rejected.decision_note.as_deref(),
        Some("Not for external use.")
    );
}

async fn a_decision_can_only_be_made_once(pool: &PgPool) {
    // Two approvers open the same queue, which is the commonest way this is reached. The second is told what the
    // order *is* rather than merely refused, because "cannot approve" leaves them refreshing a screen.
    let eve = Uuid::new_v4();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let one = asset(pool, "contested", None).await;
    let order = orders::place(c!(pool), &order_for(eve, &[one]), &predicate(&[], true))
        .await
        .expect("place");

    orders::approve(
        c!(pool),
        order.id,
        first,
        None,
        &predicate(&[], true),
        14,
        Utc::now(),
    )
    .await
    .expect("approve");

    let refusal = orders::approve(
        c!(pool),
        order.id,
        second,
        None,
        &predicate(&[], true),
        14,
        Utc::now(),
    )
    .await
    .expect_err("already decided");
    assert!(
        matches!(&refusal, OrderRefusal::WrongState(reference, state, "approved")
            if reference == &order.reference && state == "approved"),
        "{refusal:?}"
    );
    // And rejecting a decided order is refused the same way, so a race cannot flip a decision.
    assert!(matches!(
        orders::reject(c!(pool), order.id, second, None, Utc::now()).await,
        Err(OrderRefusal::WrongState(_, _, "rejected"))
    ));
}

async fn only_the_requester_cancels_and_only_before_a_decision(pool: &PgPool) {
    let frank = Uuid::new_v4();
    let somebody = Uuid::new_v4();
    let one = asset(pool, "withdrawn", None).await;
    let order = orders::place(c!(pool), &order_for(frank, &[one]), &predicate(&[], true))
        .await
        .expect("place");

    assert!(matches!(
        orders::cancel(c!(pool), order.id, somebody).await,
        Err(OrderRefusal::NotYours)
    ));
    let cancelled = orders::cancel(c!(pool), order.id, frank)
        .await
        .expect("cancel");
    assert_eq!(cancelled.state, "cancelled");
    // Cancelling keeps the decision columns empty, which the constraint requires: a cancelled order was never
    // decided by anybody.
    assert!(cancelled.decided_by.is_none());

    // After a decision there is nothing to cancel: an approval is somebody else's recorded act, and letting the
    // requester erase it would remove the trail the order exists to keep.
    let two = asset(pool, "already-approved", None).await;
    let decided = orders::place(c!(pool), &order_for(frank, &[two]), &predicate(&[], true))
        .await
        .expect("place");
    orders::approve(
        c!(pool),
        decided.id,
        somebody,
        None,
        &predicate(&[], true),
        14,
        Utc::now(),
    )
    .await
    .expect("approve");
    assert!(matches!(
        orders::cancel(c!(pool), decided.id, frank).await,
        Err(OrderRefusal::WrongState(_, _, "cancelled"))
    ));
}

async fn approved_is_not_ready_until_a_share_exists(pool: &PgPool) {
    let gail = Uuid::new_v4();
    let approver = Uuid::new_v4();
    let one = asset(pool, "pickup", None).await;
    let order = orders::place(c!(pool), &order_for(gail, &[one]), &predicate(&[], true))
        .await
        .expect("place");
    let approved = orders::approve(
        c!(pool),
        order.id,
        approver,
        None,
        &predicate(&[], true),
        14,
        Utc::now(),
    )
    .await
    .expect("approve");

    // Approved, and there is nothing to collect yet. That gap is the whole reason approval and fulfilment are
    // separate: a decision stands while packaging is retried.
    assert_eq!(approved.state, "approved");
    assert!(approved.share_link_id.is_none());
    assert!(!approved.self_approved());

    // The database refuses a `ready` order with no share, which is what stops anything other than fulfilment
    // from setting it.
    let forced = sqlx::query("UPDATE orders SET state = 'ready' WHERE id = $1")
        .bind(order.id)
        .execute(pool)
        .await;
    assert!(
        forced.is_err(),
        "a pickup was marked ready with nothing to pick up from"
    );

    let share = dam_db::shares::create_on(
        c!(pool),
        &dam_db::shares::ShareSpec {
            kind: "collection",
            target_id: None,
            search_query: None,
            passcode: None,
            expires_at: approved.expires_at,
            max_downloads: None,
            allow_original: false,
            requires_eula: false,
            created_by: Some(approver),
        },
    )
    .await
    .expect("share");

    let ready = orders::mark_ready(c!(pool), order.id, share.id)
        .await
        .expect("ready");
    assert_eq!(ready.state, "ready");
    assert_eq!(ready.share_link_id, Some(share.id));

    // And the pickup path can find the order from the share it arrived on.
    let found = orders::for_share(c!(pool), share.id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.id, order.id);

    // Collecting is idempotent: a recipient downloading twice is one collection, and the share's own cap is
    // what limits how much they may take.
    orders::mark_collected(c!(pool), order.id)
        .await
        .expect("collect");
    orders::mark_collected(c!(pool), order.id)
        .await
        .expect("collecting twice is not an error");
    let collected = orders::read(c!(pool), order.id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(collected.state, "collected");
}

async fn expiry_is_derived_from_the_decision(pool: &PgPool) {
    // From the decision rather than the request: an order approved three weeks after it was asked for should
    // give its recipients the full window.
    let hana = Uuid::new_v4();
    let approver = Uuid::new_v4();
    let one = asset(pool, "window", None).await;
    let order = orders::place(c!(pool), &order_for(hana, &[one]), &predicate(&[], true))
        .await
        .expect("place");

    let decided_at = Utc::now() + Duration::days(21);
    let approved = orders::approve(
        c!(pool),
        order.id,
        approver,
        None,
        &predicate(&[], true),
        7,
        decided_at,
    )
    .await
    .expect("approve");

    let expires = approved.expires_at.expect("a window");
    assert!(
        (expires - decided_at).num_days() == 7,
        "the window runs from the request rather than the decision: {expires} vs {decided_at}"
    );

    // Derived, never stored. The state column still says what somebody did; whether the window has closed is a
    // question about the clock.
    assert!(!approved.is_expired(decided_at));
    assert!(approved.is_expired(decided_at + Duration::days(8)));
    // A clamp, so a caller asking for a decade gets a year rather than an error about a constant they cannot
    // see — and one asking for nothing gets a day.
    let absurd = orders::place(c!(pool), &order_for(hana, &[one]), &predicate(&[], true))
        .await
        .expect("place");
    let clamped = orders::approve(
        c!(pool),
        absurd.id,
        approver,
        None,
        &predicate(&[], true),
        100_000,
        decided_at,
    )
    .await
    .expect("approve");
    assert_eq!(
        (clamped.expires_at.expect("a window") - decided_at).num_days(),
        365
    );
}

async fn the_queue_is_oldest_first_and_mine_is_newest_first(pool: &PgPool) {
    // A queue is worked through, so the thing that has waited longest is the thing to do next. A person's own
    // list is a history, so the newest is at the top.
    let ivan = Uuid::new_v4();
    let older = asset(pool, "older", None).await;
    let newer = asset(pool, "newer", None).await;
    let first = orders::place(c!(pool), &order_for(ivan, &[older]), &predicate(&[], true))
        .await
        .expect("place");
    let second = orders::place(c!(pool), &order_for(ivan, &[newer]), &predicate(&[], true))
        .await
        .expect("place");

    let mine = orders::placed_by(c!(pool), ivan, 50).await.expect("mine");
    assert_eq!(mine.len(), 2, "{mine:?}");
    assert_eq!(mine[0].id, second.id, "my list is oldest first");
    // Items travel with a list, in one query rather than one per order.
    assert!(mine.iter().all(|order| !order.items.is_empty()));

    let queue = orders::awaiting_decision(c!(pool), 50)
        .await
        .expect("queue");
    let positions: Vec<usize> = [first.id, second.id]
        .iter()
        .filter_map(|id| queue.iter().position(|order| &order.id == id))
        .collect();
    assert_eq!(positions.len(), 2, "{queue:?}");
    assert!(
        positions[0] < positions[1],
        "the queue is newest first, so the longest wait is at the bottom"
    );
    // Decided orders leave the queue, or it never empties.
    assert!(
        queue.iter().all(|order| order.state == "submitted"),
        "{queue:?}"
    );
}
