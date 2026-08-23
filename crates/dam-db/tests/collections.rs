//! Collections: membership, ordering, and the tiering pin (2.3).
//!
//! Two properties carry this task, and both are the kind that look fine in a demo and fail in
//! production.
//!
//! **Order has to be stable.** `position` defaults to 0 with no uniqueness, so an implementation that
//! does not manage it leaves every row at 0 and the order is whatever the planner returns. A curated
//! collection is usually a presentation or a portal page; one that reshuffles between page loads looks
//! like a bug in the customer's own work.
//!
//! **`pin_hot` is a union across collections.** An asset can be in several, and it must stay pinned while
//! *any* pinned collection holds it. Getting that wrong tiers a master to Glacier while a live portal page
//! still links to it — and the symptom appears hours later, as a broken image.
//!
//! One container; the cases are functions over a borrowed pool.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::collections;
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

/// One connection out of the pool.
///
/// These functions take a connection rather than a pool because in production they run inside a tenant
/// transaction — the `search_path` that makes `collections` mean `t_acme.collections` is set on a
/// connection, and a pool would hand out a different one on the next call. The tests borrow one the same
/// way, per statement, so nothing here depends on state a pool would not preserve.
async fn held(pool: &PgPool) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
    pool.acquire().await.expect("acquire")
}

async fn collection(pool: &PgPool, key: &str, pin_hot: bool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO collections (id, key, label, pin_hot) VALUES ($1, $2, $2, $3)")
        .bind(id)
        .bind(key)
        .bind(pin_hot)
        .execute(pool)
        .await
        .expect("collection");
    id
}

async fn asset(pool: &PgPool, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(format!("blake3:{label}"))
    .bind(format!("{label}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

async fn order(pool: &PgPool, collection_id: Uuid) -> Vec<Uuid> {
    collections::items(&mut *held(pool).await, collection_id)
        .await
        .expect("items")
        .into_iter()
        .map(|item| item.asset_id)
        .collect()
}

/// The positions, to assert density independently of order.
async fn positions(pool: &PgPool, collection_id: Uuid) -> Vec<i32> {
    collections::items(&mut *held(pool).await, collection_id)
        .await
        .expect("items")
        .into_iter()
        .map(|item| item.position)
        .collect()
}

// ─── ordering ───────────────────────────────────────────────────────────────

async fn assets_keep_the_order_they_were_added_in(pool: &PgPool) {
    let deck = collection(pool, "deck1", false).await;
    let a = asset(pool, "a1").await;
    let b = asset(pool, "b1").await;
    let c = asset(pool, "c1").await;
    for id in [a, b, c] {
        collections::add(&mut *held(pool).await, deck, id, None)
            .await
            .expect("add");
    }

    assert_eq!(order(pool, deck).await, vec![a, b, c]);
    assert_eq!(
        positions(pool, deck).await,
        vec![0, 1, 2],
        "positions must be dense from zero, or 'is this collection well-ordered' is unanswerable"
    );
}

async fn adding_the_same_asset_twice_does_not_move_it(pool: &PgPool) {
    // A retried request must not silently reorder somebody's curation, and the primary key means the
    // insert cannot duplicate — so the failure mode to avoid is an upsert that resets the position.
    let deck = collection(pool, "deck2", false).await;
    let a = asset(pool, "a2").await;
    let b = asset(pool, "b2").await;
    collections::add(&mut *held(pool).await, deck, a, None)
        .await
        .expect("add a");
    collections::add(&mut *held(pool).await, deck, b, None)
        .await
        .expect("add b");

    collections::add(&mut *held(pool).await, deck, a, None)
        .await
        .expect("re-adding must be idempotent");
    assert_eq!(order(pool, deck).await, vec![a, b]);
    assert_eq!(positions(pool, deck).await, vec![0, 1]);
}

async fn removing_an_asset_closes_the_gap(pool: &PgPool) {
    // Leaving a hole would be cheaper and would make density unstateable — so the next bug here would
    // have nothing to assert against.
    let deck = collection(pool, "deck3", false).await;
    let a = asset(pool, "a3").await;
    let b = asset(pool, "b3").await;
    let c = asset(pool, "c3").await;
    for id in [a, b, c] {
        collections::add(&mut *held(pool).await, deck, id, None)
            .await
            .expect("add");
    }

    assert!(
        collections::remove(&mut *held(pool).await, deck, b)
            .await
            .expect("remove"),
        "removing a present asset reports true"
    );
    assert_eq!(order(pool, deck).await, vec![a, c]);
    assert_eq!(positions(pool, deck).await, vec![0, 1]);

    assert!(
        !collections::remove(&mut *held(pool).await, deck, b)
            .await
            .expect("remove"),
        "removing an absent asset is not an error, and reports false"
    );
}

async fn moving_an_asset_up_shifts_the_ones_it_passed(pool: &PgPool) {
    let deck = collection(pool, "deck4", false).await;
    let ids: Vec<Uuid> = {
        let mut out = Vec::new();
        for n in 0..5 {
            let id = asset(pool, &format!("a4-{n}")).await;
            collections::add(&mut *held(pool).await, deck, id, None)
                .await
                .expect("add");
            out.push(id);
        }
        out
    };

    // The fourth item becomes the second.
    collections::move_item(&mut *held(pool).await, deck, ids[3], 1)
        .await
        .expect("move");
    assert_eq!(
        order(pool, deck).await,
        vec![ids[0], ids[3], ids[1], ids[2], ids[4]]
    );
    assert_eq!(positions(pool, deck).await, vec![0, 1, 2, 3, 4]);
}

async fn moving_an_asset_down_shifts_the_ones_it_passed(pool: &PgPool) {
    let deck = collection(pool, "deck5", false).await;
    let mut ids = Vec::new();
    for n in 0..4 {
        let id = asset(pool, &format!("a5-{n}")).await;
        collections::add(&mut *held(pool).await, deck, id, None)
            .await
            .expect("add");
        ids.push(id);
    }

    collections::move_item(&mut *held(pool).await, deck, ids[0], 2)
        .await
        .expect("move");
    assert_eq!(
        order(pool, deck).await,
        vec![ids[1], ids[2], ids[0], ids[3]]
    );
    assert_eq!(positions(pool, deck).await, vec![0, 1, 2, 3]);
}

async fn a_move_past_the_end_is_clamped_rather_than_refused(pool: &PgPool) {
    // A drag-and-drop UI reporting "position 47 of 30" is a rounding difference between what the client
    // and the server think the list is. Refusing the drop loses the user's action over an off-by-one;
    // clamping does the obvious thing.
    let deck = collection(pool, "deck6", false).await;
    let a = asset(pool, "a6").await;
    let b = asset(pool, "b6").await;
    collections::add(&mut *held(pool).await, deck, a, None)
        .await
        .expect("add");
    collections::add(&mut *held(pool).await, deck, b, None)
        .await
        .expect("add");

    collections::move_item(&mut *held(pool).await, deck, a, 99)
        .await
        .expect("move");
    assert_eq!(order(pool, deck).await, vec![b, a]);

    collections::move_item(&mut *held(pool).await, deck, a, -5)
        .await
        .expect("move");
    assert_eq!(order(pool, deck).await, vec![a, b]);
    assert_eq!(positions(pool, deck).await, vec![0, 1]);
}

async fn moving_an_asset_that_is_not_a_member_is_not_found(pool: &PgPool) {
    // Reordering something a concurrent request just removed is ordinary, so the caller needs a 404
    // rather than a 500.
    let deck = collection(pool, "deck7", false).await;
    let stranger = asset(pool, "a7").await;
    assert!(
        collections::move_item(&mut *held(pool).await, deck, stranger, 0)
            .await
            .is_err()
    );
}

async fn two_collections_order_independently(pool: &PgPool) {
    // The positions are per collection, so the same asset holds a different slot in each. A missing
    // `collection_id` in any of the position statements would silently couple them.
    let first = collection(pool, "deck8a", false).await;
    let second = collection(pool, "deck8b", false).await;
    let a = asset(pool, "a8").await;
    let b = asset(pool, "b8").await;

    collections::add(&mut *held(pool).await, first, a, None)
        .await
        .expect("add");
    collections::add(&mut *held(pool).await, first, b, None)
        .await
        .expect("add");
    collections::add(&mut *held(pool).await, second, b, None)
        .await
        .expect("add");
    collections::add(&mut *held(pool).await, second, a, None)
        .await
        .expect("add");

    assert_eq!(order(pool, first).await, vec![a, b]);
    assert_eq!(order(pool, second).await, vec![b, a]);

    collections::move_item(&mut *held(pool).await, first, b, 0)
        .await
        .expect("move");
    assert_eq!(order(pool, first).await, vec![b, a]);
    assert_eq!(
        order(pool, second).await,
        vec![b, a],
        "the other collection's order must be untouched"
    );
}

// ─── the tiering pin ────────────────────────────────────────────────────────

async fn membership_of_a_pinned_collection_blocks_tiering(pool: &PgPool) {
    // §6.4. The reason is not storage cost but breakage: a portal page linking a master that has gone to
    // Glacier is a broken image for hours, and the customer's page is what breaks, not ours.
    let pinned = collection(pool, "portal9", true).await;
    let a = asset(pool, "a9").await;
    let unpinned_asset = asset(pool, "b9").await;
    collections::add(&mut *held(pool).await, pinned, a, None)
        .await
        .expect("add");

    let pins = collections::pins(&mut *held(pool).await, &[a, unpinned_asset])
        .await
        .expect("pins");
    let pin = pins.get(&a).expect("the pinned asset must be reported");
    assert_eq!(pin.collections, vec!["portal9"]);
    assert!(
        pin.reason().contains("portal9"),
        "the reason must name the collection, or a skipped tiering plan is unactionable: {}",
        pin.reason()
    );
    assert!(
        !pins.contains_key(&unpinned_asset),
        "an asset in no pinned collection must not appear"
    );
}

async fn an_unpinned_collection_does_not_pin(pool: &PgPool) {
    let ordinary = collection(pool, "ordinary10", false).await;
    let a = asset(pool, "a10").await;
    collections::add(&mut *held(pool).await, ordinary, a, None)
        .await
        .expect("add");
    assert!(
        collections::pins(&mut *held(pool).await, &[a])
            .await
            .expect("pins")
            .is_empty()
    );
}

async fn removal_from_one_pinned_collection_leaves_the_other_pin_standing(pool: &PgPool) {
    // The bug this module exists to avoid. Computing the pin per collection and letting the last writer
    // win unpins an asset that two collections hold — and the master silently tiers while a live page
    // still links it.
    let first = collection(pool, "portal11a", true).await;
    let second = collection(pool, "portal11b", true).await;
    let a = asset(pool, "a11").await;
    collections::add(&mut *held(pool).await, first, a, None)
        .await
        .expect("add");
    collections::add(&mut *held(pool).await, second, a, None)
        .await
        .expect("add");

    let pin = collections::pins(&mut *held(pool).await, &[a])
        .await
        .expect("pins")
        .remove(&a)
        .expect("pinned");
    assert_eq!(
        pin.collections,
        vec!["portal11a", "portal11b"],
        "every pinning collection must be reported, so an operator knows how many to deal with"
    );

    collections::remove(&mut *held(pool).await, first, a)
        .await
        .expect("remove");
    let still = collections::pins(&mut *held(pool).await, &[a])
        .await
        .expect("pins")
        .remove(&a)
        .expect("must still be pinned by the second collection");
    assert_eq!(still.collections, vec!["portal11b"]);

    collections::remove(&mut *held(pool).await, second, a)
        .await
        .expect("remove");
    assert!(
        collections::pins(&mut *held(pool).await, &[a])
            .await
            .expect("pins")
            .is_empty(),
        "with no pinning collection left, the asset is tierable"
    );
}

async fn clearing_pin_hot_releases_the_assets(pool: &PgPool) {
    let deck = collection(pool, "portal12", true).await;
    let a = asset(pool, "a12").await;
    collections::add(&mut *held(pool).await, deck, a, None)
        .await
        .expect("add");
    assert!(
        !collections::pins(&mut *held(pool).await, &[a])
            .await
            .expect("pins")
            .is_empty()
    );

    sqlx::query("UPDATE collections SET pin_hot = false WHERE id = $1")
        .bind(deck)
        .execute(pool)
        .await
        .expect("unpin");
    assert!(
        collections::pins(&mut *held(pool).await, &[a])
            .await
            .expect("pins")
            .is_empty(),
        "clearing pin_hot must release its assets without touching membership"
    );
    assert_eq!(order(pool, deck).await, vec![a]);
}

async fn deleting_a_collection_releases_the_pin_but_keeps_the_asset(pool: &PgPool) {
    // The cascade is on membership only. A collection is a view of the library, so deleting one must
    // never delete the assets — and it must not leave a pin behind that nothing can now explain.
    let deck = collection(pool, "portal13", true).await;
    let a = asset(pool, "a13").await;
    collections::add(&mut *held(pool).await, deck, a, None)
        .await
        .expect("add");

    sqlx::query("DELETE FROM collections WHERE id = $1")
        .bind(deck)
        .execute(pool)
        .await
        .expect("delete");

    assert!(
        collections::pins(&mut *held(pool).await, &[a])
            .await
            .expect("pins")
            .is_empty()
    );
    let survives: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM assets WHERE id = $1)")
        .bind(a)
        .fetch_one(pool)
        .await
        .expect("asset");
    assert!(survives, "deleting a collection must not delete its assets");
}

async fn a_deleted_asset_is_not_pinned_by_collection_membership(pool: &PgPool) {
    // A judgement call, made this way deliberately: the pin exists to keep something reachable for
    // people, and nobody is reaching a deleted asset — paying hot-storage rates for it until somebody
    // remembers to tidy the collection is the wrong default. Legal hold is a separate mechanism and still
    // blocks tiering *and* purge.
    let deck = collection(pool, "portal14", true).await;
    let a = asset(pool, "a14").await;
    collections::add(&mut *held(pool).await, deck, a, None)
        .await
        .expect("add");

    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(a)
        .execute(pool)
        .await
        .expect("soft delete");
    assert!(
        collections::pins(&mut *held(pool).await, &[a])
            .await
            .expect("pins")
            .is_empty(),
        "a deleted asset is not kept hot by a stale collection membership"
    );
}

async fn pins_of_an_empty_batch_does_not_query(pool: &PgPool) {
    // The lifecycle worker's last page is often empty, and `= ANY('{}')` is a pointless round trip on
    // every pass.
    assert!(
        collections::pins(&mut *held(pool).await, &[])
            .await
            .expect("pins")
            .is_empty()
    );
}

// ─── the collection itself ──────────────────────────────────────────────────

async fn a_new_collection_is_listed_with_a_count_of_zero(pool: &PgPool) {
    let id = collections::create(
        &mut *held(pool).await,
        &collections::NewCollection {
            key: "spring",
            label: "Spring",
            description: Some("campaign"),
            visibility: "private",
            pin_hot: true,
            owner_id: None,
        },
    )
    .await
    .expect("create");

    let listed = collections::all(&mut *held(pool).await).await.expect("all");
    let made = listed
        .iter()
        .find(|one| one.id == id)
        .expect("a newly created collection appears in the list before anything is in it");
    assert_eq!(made.key, "spring");
    assert_eq!(made.description.as_deref(), Some("campaign"));
    assert!(made.pin_hot);
    assert_eq!(
        made.item_count, 0,
        "an empty collection counts zero rather than vanishing"
    );

    let found = collections::by_key(&mut *held(pool).await, "spring")
        .await
        .expect("by_key")
        .expect("the key a portal would reference resolves");
    assert_eq!(found.id, id);
    assert!(
        collections::by_key(&mut *held(pool).await, "no-such-key")
            .await
            .expect("by_key")
            .is_none()
    );
}

async fn a_taken_key_is_refused_by_name(pool: &PgPool) {
    let new = collections::NewCollection {
        key: "twice",
        label: "Twice",
        description: None,
        visibility: "private",
        pin_hot: false,
        owner_id: None,
    };
    collections::create(&mut *held(pool).await, &new)
        .await
        .expect("first");
    let again = collections::create(&mut *held(pool).await, &new).await;
    let message = match again {
        Err(dam_db::Error::Unsupported(message)) => message,
        other => panic!("a taken key should be refused by name, got {other:?}"),
    };
    assert!(
        message.contains("twice"),
        "the refusal names the key the person typed, so they can fix it: {message}"
    );
}

async fn an_invented_visibility_is_refused_before_the_insert(pool: &PgPool) {
    // Checked in Rust as well as by the CHECK constraint, so the caller gets a sentence naming the three
    // valid values rather than a 500 carrying a constraint name.
    let refused = collections::create(
        &mut *held(pool).await,
        &collections::NewCollection {
            key: "invented",
            label: "Invented",
            description: None,
            visibility: "world-readable",
            pin_hot: false,
            owner_id: None,
        },
    )
    .await;
    assert!(matches!(refused, Err(dam_db::Error::Unsupported(_))));
    assert!(
        collections::by_key(&mut *held(pool).await, "invented")
            .await
            .expect("by_key")
            .is_none(),
        "a refused visibility inserts nothing"
    );
}

async fn rename_changes_the_label_and_never_the_key(pool: &PgPool) {
    let id = collections::create(
        &mut *held(pool).await,
        &collections::NewCollection {
            key: "stable-key",
            label: "Before",
            description: None,
            visibility: "private",
            pin_hot: false,
            owner_id: None,
        },
    )
    .await
    .expect("create");

    assert!(
        collections::rename(
            &mut *held(pool).await,
            id,
            "After",
            Some("now described"),
            "public",
            true
        )
        .await
        .expect("rename")
    );

    let after = collections::by_key(&mut *held(pool).await, "stable-key")
        .await
        .expect("by_key")
        .expect("the key is the same key");
    assert_eq!(after.label, "After");
    assert_eq!(after.description.as_deref(), Some("now described"));
    assert_eq!(after.visibility, "public");
    assert!(
        after.pin_hot,
        "pinning is part of the same form, so it saves with it"
    );

    assert!(
        !collections::rename(
            &mut *held(pool).await,
            Uuid::new_v4(),
            "Nobody",
            None,
            "private",
            false
        )
        .await
        .expect("rename"),
        "renaming a collection that is not there reports false rather than inventing one"
    );
}

async fn turning_on_pinning_pins_what_is_already_inside(pool: &PgPool) {
    // The union in `pins` reads `collections.pin_hot` live, so an existing collection that becomes pinned
    // protects its existing members — this is how somebody rescues a set that is about to tier.
    let id = collections::create(
        &mut *held(pool).await,
        &collections::NewCollection {
            key: "late-pin",
            label: "Late pin",
            description: None,
            visibility: "private",
            pin_hot: false,
            owner_id: None,
        },
    )
    .await
    .expect("create");
    let a = asset(pool, "latepin").await;
    collections::add(&mut *held(pool).await, id, a, None)
        .await
        .expect("add");
    assert!(
        collections::pins(&mut *held(pool).await, &[a])
            .await
            .expect("pins")
            .is_empty()
    );

    collections::rename(
        &mut *held(pool).await,
        id,
        "Late pin",
        None,
        "private",
        true,
    )
    .await
    .expect("pin it");
    let pins = collections::pins(&mut *held(pool).await, &[a])
        .await
        .expect("pins");
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[&a].collections, vec!["late-pin".to_owned()]);
}

async fn a_published_collection_cannot_be_deleted(pool: &PgPool) {
    let id = collections::create(
        &mut *held(pool).await,
        &collections::NewCollection {
            key: "published",
            label: "Published",
            description: None,
            visibility: "public",
            pin_hot: true,
            owner_id: None,
        },
    )
    .await
    .expect("create");

    sqlx::query(
        "INSERT INTO portals (id, key, title, kind, collection_id) \
         VALUES ($1, 'a-portal', 'A portal', 'standard', $2)",
    )
    .bind(Uuid::new_v4())
    .bind(id)
    .execute(pool)
    .await
    .expect("portal");

    let refused = collections::delete(&mut *held(pool).await, id).await;
    let message = match refused {
        Err(dam_db::Error::Unsupported(message)) => message,
        other => panic!("deleting a published collection should be refused, got {other:?}"),
    };
    assert!(
        message.contains('1') && message.contains("portal"),
        "the refusal says how many portals and what to do about them: {message}"
    );

    // Still there, and still usable: the refusal is a guard, not a half-delete.
    assert!(
        collections::by_key(&mut *held(pool).await, "published")
            .await
            .expect("by_key")
            .is_some()
    );

    // A retired portal does not hold the collection hostage — that is what `retired_at IS NULL` is for, and
    // the first version of the guard read `deleted_at`, a column portals do not have.
    sqlx::query("UPDATE portals SET retired_at = now() WHERE key = 'a-portal'")
        .execute(pool)
        .await
        .expect("retire");
    assert!(
        collections::delete(&mut *held(pool).await, id)
            .await
            .expect("delete")
    );
    assert!(
        !collections::delete(&mut *held(pool).await, id)
            .await
            .expect("delete"),
        "deleting it twice reports false rather than erroring"
    );
}

async fn the_delete_guard_leaves_the_transaction_usable(pool: &PgPool) {
    // The regression that motivates this: the guard used to swallow its own query error with `unwrap_or(0)`.
    // Inside the caller's transaction a failed statement aborts the whole transaction, so the damage showed
    // up on the *next* statement as "current transaction is aborted" — far from the line that caused it.
    // Here the guard refuses legitimately; what is asserted is that the connection still works afterwards.
    let id = collections::create(
        &mut *held(pool).await,
        &collections::NewCollection {
            key: "guarded",
            label: "Guarded",
            description: None,
            visibility: "private",
            pin_hot: false,
            owner_id: None,
        },
    )
    .await
    .expect("create");
    sqlx::query(
        "INSERT INTO portals (id, key, title, kind, collection_id) \
         VALUES ($1, 'guarded-portal', 'Guarded', 'brand', $2)",
    )
    .bind(Uuid::new_v4())
    .bind(id)
    .execute(pool)
    .await
    .expect("portal");

    let mut tx = pool.begin().await.expect("begin");
    assert!(collections::delete(&mut tx, id).await.is_err());
    let still: i64 = sqlx::query_scalar("SELECT count(*) FROM collections WHERE id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .expect("the transaction is still usable after a refused delete");
    assert_eq!(still, 1);
    tx.commit().await.expect("commit");
}

async fn items_carry_what_a_screen_needs_to_draw_them(pool: &PgPool) {
    let id = collections::create(
        &mut *held(pool).await,
        &collections::NewCollection {
            key: "drawable",
            label: "Drawable",
            description: None,
            visibility: "private",
            pin_hot: false,
            owner_id: None,
        },
    )
    .await
    .expect("create");
    let a = asset(pool, "drawme").await;
    collections::add(&mut *held(pool).await, id, a, None)
        .await
        .expect("add");

    let items = collections::items(&mut *held(pool).await, id)
        .await
        .expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].filename, "drawme.jpg");
    assert_eq!(items[0].mime, "image/jpeg");

    // A member whose asset was deleted is not a member anybody can act on, and a portal must not publish it.
    sqlx::query("UPDATE assets SET status = 'deleted' WHERE id = $1")
        .bind(a)
        .execute(pool)
        .await
        .expect("soft delete");
    assert!(
        collections::items(&mut *held(pool).await, id)
            .await
            .expect("items")
            .is_empty(),
        "a deleted asset drops out of the curated order rather than appearing as a hole"
    );
}

#[tokio::test]
async fn the_collection_invariants_hold() {
    let (_pg, pool) = db().await;

    assets_keep_the_order_they_were_added_in(&pool).await;
    adding_the_same_asset_twice_does_not_move_it(&pool).await;
    removing_an_asset_closes_the_gap(&pool).await;
    moving_an_asset_up_shifts_the_ones_it_passed(&pool).await;
    moving_an_asset_down_shifts_the_ones_it_passed(&pool).await;
    a_move_past_the_end_is_clamped_rather_than_refused(&pool).await;
    moving_an_asset_that_is_not_a_member_is_not_found(&pool).await;
    two_collections_order_independently(&pool).await;

    membership_of_a_pinned_collection_blocks_tiering(&pool).await;
    an_unpinned_collection_does_not_pin(&pool).await;
    removal_from_one_pinned_collection_leaves_the_other_pin_standing(&pool).await;
    clearing_pin_hot_releases_the_assets(&pool).await;
    deleting_a_collection_releases_the_pin_but_keeps_the_asset(&pool).await;
    a_deleted_asset_is_not_pinned_by_collection_membership(&pool).await;
    pins_of_an_empty_batch_does_not_query(&pool).await;

    a_new_collection_is_listed_with_a_count_of_zero(&pool).await;
    a_taken_key_is_refused_by_name(&pool).await;
    an_invented_visibility_is_refused_before_the_insert(&pool).await;
    rename_changes_the_label_and_never_the_key(&pool).await;
    turning_on_pinning_pins_what_is_already_inside(&pool).await;
    a_published_collection_cannot_be_deleted(&pool).await;
    the_delete_guard_leaves_the_transaction_usable(&pool).await;
    items_carry_what_a_screen_needs_to_draw_them(&pool).await;
}
