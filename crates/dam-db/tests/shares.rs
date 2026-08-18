//! Share links (3.3).
//!
//! The requirement TASKS.md names is the one that constrains the design: **revocation that takes effect on
//! an already-issued URL.** Resolving a share token per request makes revoking the share immediate — but a
//! share mints delivery tokens, and one of those is valid for its own TTL. So the delivery claim carries the
//! share's id and delivery re-checks it, which is the same shape as D12's rights check. That end-to-end
//! property is tested in `dam-api/tests/delivery.rs`; this suite covers the share itself.
//!
//! Two things here are easy to get wrong in ways that only show up in production:
//!
//! - **The download limit must be checked and incremented in one statement.** Read-compare-increment lets
//!   two concurrent downloads both take the last slot, and on `max_downloads = 1` the asset goes out twice.
//! - **The two secrets need opposite hashes.** A 256-bit token has no dictionary, so argon2 would cost
//!   ~100 ms per view for nothing. A human's passcode *is* in a dictionary, so BLAKE3 would make an offline
//!   attack on a leaked digest trivial.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_db::shares::{self, ShareRefusal, ShareSpec};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap()
}

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

fn spec<'a>() -> ShareSpec<'a> {
    ShareSpec {
        kind: "asset",
        target_id: Some(Uuid::new_v4()),
        search_query: None,
        passcode: None,
        expires_at: None,
        max_downloads: None,
        allow_original: false,
        requires_eula: false,
        created_by: None,
    }
}

// ─── the token ──────────────────────────────────────────────────────────────

async fn a_share_resolves_by_its_token(pool: &PgPool) {
    let created = shares::create(pool, &spec()).await.expect("create");
    let resolved = shares::resolve(pool, created.token(), now())
        .await
        .expect("resolve");
    assert_eq!(resolved.id, created.id);
    assert!(resolved.is_live(now()));
}

async fn the_plaintext_token_is_never_stored(pool: &PgPool) {
    // A share token in the database is every live share link in the database. Storing a digest means a leak
    // does not hand them over — the same reasoning as `auth::ApiKey`, and the reason this column named
    // `token` holds a hash.
    let created = shares::create(pool, &spec()).await.expect("create");
    let stored: Option<String> = sqlx::query_scalar("SELECT token FROM share_links WHERE id = $1")
        .bind(created.id)
        .fetch_optional(pool)
        .await
        .expect("read");
    let stored = stored.expect("a row");
    assert_ne!(stored, created.token(), "the plaintext must not be stored");
    assert_eq!(stored, shares::token_digest(created.token()));
}

async fn an_unknown_token_is_not_found(pool: &PgPool) {
    assert_eq!(
        shares::resolve(pool, "deadbeef", now()).await.unwrap_err(),
        ShareRefusal::NotFound
    );
}

async fn the_debug_impl_does_not_print_the_token(pool: &PgPool) {
    // A share token in a log is a share link that has to be revoked.
    let created = shares::create(pool, &spec()).await.expect("create");
    let rendered = format!("{created:?}");
    assert!(!rendered.contains(created.token()), "got {rendered}");
    assert!(rendered.contains("REDACTED"));
}

// ─── expiry, limits, revocation ─────────────────────────────────────────────

async fn an_expired_share_is_refused_and_says_so(pool: &PgPool) {
    // Distinguished from `NotFound` on purpose. A recipient told "expired" asks for a new link; one told
    // "not found" goes and checks the URL. The token is 256 random bits so nobody can enumerate one, and the
    // recipient is the person who needs the answer.
    let created = shares::create(
        pool,
        &ShareSpec {
            expires_at: Some(now() - Duration::seconds(1)),
            ..spec()
        },
    )
    .await
    .expect("create");
    assert_eq!(
        shares::resolve(pool, created.token(), now())
            .await
            .unwrap_err(),
        ShareRefusal::Expired
    );
}

async fn an_expired_share_cannot_have_a_download_consumed(pool: &PgPool) {
    // `resolve` refusing an expired share is covered above; `consume_download` is a *different* query with its
    // own copy of the expiry condition, and nothing asserted it. A mutation inverting the comparison there
    // passed the whole suite — which would mean an expired share still spending downloads while a live one
    // could not.
    //
    // It has its own copy deliberately: the decrement has to be atomic with the check, or two concurrent
    // downloads both pass a separate check and both succeed on the last one. So the condition is duplicated,
    // and a duplicated condition needs its own test.
    let live = shares::create(
        pool,
        &ShareSpec {
            expires_at: Some(now() + Duration::hours(1)),
            max_downloads: Some(3),
            ..spec()
        },
    )
    .await
    .expect("create");
    assert_eq!(
        shares::consume_download(pool, live.id, now())
            .await
            .expect("a live share spends a download"),
        1
    );

    let expired = shares::create(
        pool,
        &ShareSpec {
            expires_at: Some(now() - Duration::seconds(1)),
            max_downloads: Some(3),
            ..spec()
        },
    )
    .await
    .expect("create");
    assert_eq!(
        shares::consume_download(pool, expired.id, now())
            .await
            .unwrap_err(),
        ShareRefusal::Exhausted,
        "an expired share must not spend a download, however many it has left"
    );

    // And the counter did not move, because the refusal is the UPDATE matching nothing rather than a check
    // before it.
    let spent: i32 = sqlx::query_scalar("SELECT download_count FROM share_links WHERE id = $1")
        .bind(expired.id)
        .fetch_one(pool)
        .await
        .expect("count");
    assert_eq!(spent, 0);
}

async fn a_revoked_share_is_refused_before_expiry_is_even_considered(pool: &PgPool) {
    // Revocation is the most absolute reason, and the one a recipient most needs stated plainly. A share that
    // is both revoked and expired should say revoked.
    let created = shares::create(
        pool,
        &ShareSpec {
            expires_at: Some(now() - Duration::days(1)),
            ..spec()
        },
    )
    .await
    .expect("create");
    assert!(
        shares::revoke(pool, created.id, now())
            .await
            .expect("revoke"),
        "the first revoke reports that it acted"
    );
    assert_eq!(
        shares::resolve(pool, created.token(), now())
            .await
            .unwrap_err(),
        ShareRefusal::Revoked
    );
}

async fn revoking_twice_reports_only_the_first(pool: &PgPool) {
    // So an audit entry is written once rather than on every retry.
    let created = shares::create(pool, &spec()).await.expect("create");
    assert!(
        shares::revoke(pool, created.id, now())
            .await
            .expect("first")
    );
    assert!(
        !shares::revoke(pool, created.id, now())
            .await
            .expect("second"),
        "a repeat revoke must be idempotent and say it did nothing"
    );
}

async fn a_download_limit_is_enforced_and_reported(pool: &PgPool) {
    let created = shares::create(
        pool,
        &ShareSpec {
            max_downloads: Some(2),
            ..spec()
        },
    )
    .await
    .expect("create");

    assert_eq!(
        shares::consume_download(pool, created.id, now())
            .await
            .expect("first"),
        1
    );
    assert_eq!(
        shares::consume_download(pool, created.id, now())
            .await
            .expect("second"),
        2
    );
    assert_eq!(
        shares::consume_download(pool, created.id, now())
            .await
            .unwrap_err(),
        ShareRefusal::Exhausted
    );
    // And resolving now reports the same thing, so a caller that never calls `consume_download` still cannot
    // present an exhausted link as usable.
    assert_eq!(
        shares::resolve(pool, created.token(), now())
            .await
            .unwrap_err(),
        ShareRefusal::Exhausted
    );
}

async fn concurrent_downloads_cannot_both_take_the_last_slot(pool: &PgPool) {
    // The race that matters. Read-compare-increment lets both requests see `count = 0` against a limit of 1
    // and both proceed — the asset goes out twice from a link that said once. The check and the increment are
    // one statement, so exactly one of these can win.
    let created = shares::create(
        pool,
        &ShareSpec {
            max_downloads: Some(1),
            ..spec()
        },
    )
    .await
    .expect("create");

    let mut handles = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        let id = created.id;
        handles.push(tokio::spawn(async move {
            shares::consume_download(&pool, id, now()).await
        }));
    }
    let mut granted = 0;
    for handle in handles {
        if handle.await.expect("join").is_ok() {
            granted += 1;
        }
    }
    assert_eq!(
        granted, 1,
        "exactly one of eight concurrent downloads may take a limit of one"
    );

    let count: i32 = sqlx::query_scalar("SELECT download_count FROM share_links WHERE id = $1")
        .bind(created.id)
        .fetch_one(pool)
        .await
        .expect("count");
    assert_eq!(count, 1, "and the counter must not overshoot");
}

async fn consuming_a_revoked_share_is_refused(pool: &PgPool) {
    // The revocation check is in the same statement as the limit, so a share revoked between resolve and
    // download cannot slip one through.
    let created = shares::create(pool, &spec()).await.expect("create");
    shares::revoke(pool, created.id, now())
        .await
        .expect("revoke");
    assert_eq!(
        shares::consume_download(pool, created.id, now())
            .await
            .unwrap_err(),
        ShareRefusal::Exhausted
    );
}

async fn is_live_answers_what_delivery_needs(pool: &PgPool) {
    // What the delivery path calls per request for a share-issued URL. It has to be cheap and it has to be
    // exact, because it is the thing standing between a revoked share and an outstanding download URL.
    let created = shares::create(pool, &spec()).await.expect("create");
    assert!(
        shares::is_live(pool, created.id, now())
            .await
            .expect("live")
    );

    shares::revoke(pool, created.id, now())
        .await
        .expect("revoke");
    assert!(
        !shares::is_live(pool, created.id, now())
            .await
            .expect("live"),
        "a revoked share must report dead immediately"
    );

    // An unknown id is not live, rather than an error: it races a deletion, and a delivery must refuse rather
    // than fail.
    assert!(
        !shares::is_live(pool, Uuid::new_v4(), now())
            .await
            .expect("live")
    );
}

// ─── the passcode ───────────────────────────────────────────────────────────

async fn a_passcode_is_required_when_one_was_set(pool: &PgPool) {
    let created = shares::create(
        pool,
        &ShareSpec {
            passcode: Some("spring2026"),
            ..spec()
        },
    )
    .await
    .expect("create");

    let resolved = shares::resolve(pool, created.token(), now())
        .await
        .expect("resolve");
    assert!(resolved.has_passcode);

    // Missing and wrong are different answers: one says look in the email, the other says re-read it.
    assert_eq!(
        shares::check_passcode(pool, created.id, None)
            .await
            .unwrap_err(),
        ShareRefusal::PasscodeRequired
    );
    assert_eq!(
        shares::check_passcode(pool, created.id, Some("summer2026"))
            .await
            .unwrap_err(),
        ShareRefusal::PasscodeWrong
    );
    shares::check_passcode(pool, created.id, Some("spring2026"))
        .await
        .expect("the right passcode");
}

async fn a_share_without_a_passcode_accepts_none(pool: &PgPool) {
    let created = shares::create(pool, &spec()).await.expect("create");
    shares::check_passcode(pool, created.id, None)
        .await
        .expect("no passcode set, so none required");
}

async fn the_passcode_is_stored_as_an_argon2_hash_not_a_fast_digest(pool: &PgPool) {
    // The asymmetry with the token, and the reason it matters. A human's passcode is in a dictionary, so an
    // offline attack on a leaked BLAKE3 digest of `spring2026` succeeds instantly. argon2id makes each guess
    // expensive. The token gets the opposite treatment for the opposite reason: 256 random bits have no
    // dictionary, and argon2 would add ~100 ms to every share view for nothing.
    let created = shares::create(
        pool,
        &ShareSpec {
            passcode: Some("spring2026"),
            ..spec()
        },
    )
    .await
    .expect("create");

    let stored: Option<String> =
        sqlx::query_scalar("SELECT passcode_hash FROM share_links WHERE id = $1")
            .bind(created.id)
            .fetch_one(pool)
            .await
            .expect("read");
    let stored = stored.expect("a hash");
    assert!(
        stored.starts_with("$argon2"),
        "a human-chosen passcode needs a slow hash, got {stored}"
    );
    assert!(!stored.contains("spring2026"));

    // Salted, so two shares with the same passcode do not share a digest — otherwise a leak reveals which
    // links share a passcode, and cracking one cracks them all.
    let other = shares::create(
        pool,
        &ShareSpec {
            passcode: Some("spring2026"),
            ..spec()
        },
    )
    .await
    .expect("create");
    let other_hash: Option<String> =
        sqlx::query_scalar("SELECT passcode_hash FROM share_links WHERE id = $1")
            .bind(other.id)
            .fetch_one(pool)
            .await
            .expect("read");
    assert_ne!(
        other_hash.expect("a hash"),
        stored,
        "the same passcode must not produce the same digest twice"
    );
}

#[tokio::test]
async fn the_share_link_invariants_hold() {
    let (_pg, pool) = db().await;

    a_share_resolves_by_its_token(&pool).await;
    the_plaintext_token_is_never_stored(&pool).await;
    an_unknown_token_is_not_found(&pool).await;
    the_debug_impl_does_not_print_the_token(&pool).await;

    an_expired_share_is_refused_and_says_so(&pool).await;
    a_revoked_share_is_refused_before_expiry_is_even_considered(&pool).await;
    an_expired_share_cannot_have_a_download_consumed(&pool).await;
    revoking_twice_reports_only_the_first(&pool).await;
    a_download_limit_is_enforced_and_reported(&pool).await;
    concurrent_downloads_cannot_both_take_the_last_slot(&pool).await;
    consuming_a_revoked_share_is_refused(&pool).await;
    is_live_answers_what_delivery_needs(&pool).await;

    a_passcode_is_required_when_one_was_set(&pool).await;
    a_share_without_a_passcode_accepts_none(&pool).await;
    the_passcode_is_stored_as_an_argon2_hash_not_a_fast_digest(&pool).await;
}
