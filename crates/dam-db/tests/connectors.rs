//! Connected sites and their signing secrets (M3d·1, §11).
//!
//! Four properties, and three of them are about the secret:
//!
//! **A scheduled rotation keeps a grace window; a leak does not.** The DAM-side rotation and the site-side
//! configuration change are separate deploys, so a rotation with no window is an outage — but a week of grace
//! is a week of forgery when the reason for rotating is that the secret got out. So the choice is an argument.
//!
//! **The window is a comparison, never a cleared column.** A cleanup job that fails leaves a superseded secret
//! valid forever and nothing says so.
//!
//! **Revoking clears both secrets and is terminal.** A revoked connector's secret is already out there;
//! reactivating one would bring every URL the remote ever signed back to life.
//!
//! And the ordinary one: **one site per kind**, so a second registration of the same Drupal install is a named
//! refusal rather than a second row that quietly signs with a different secret.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{Duration, Utc};
use dam_db::connectors::{self, ConnectorRefusal, Kind, NewConnector, SECRET_GRACE, Status};
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

fn new<'a>(id: Uuid, site: &'a str, sealed: &'a str) -> NewConnector<'a> {
    NewConnector {
        id,
        kind: Kind::Drupal,
        label: "  Marketing site  ",
        site_url: site,
        remote_version: Some("drupal 11.1 / damrs_dam 1.0.0"),
        api_key_id: Some(Uuid::new_v4()),
        sealed_secret: sealed,
        asset_group_ids: &[],
        allow_all_groups: true,
        allow_original: false,
        allow_restore: false,
        config: serde_json::json!({}),
    }
}

#[tokio::test]
async fn a_registration_trims_its_label_and_its_trailing_slash() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = Uuid::new_v4();

    connectors::register(&mut conn, &new(id, "https://example.test/", "v1.k1.n.c"))
        .await
        .expect("register");

    let found = connectors::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.label, "Marketing site");
    // The URL is the CORS allowlist entry and the audience claim on issued tokens, so a trailing slash is the
    // difference between matching and not.
    assert_eq!(found.site_url, "https://example.test");
    assert_eq!(found.status, Status::Active);
    // The defaults that matter, and both are off: a CMS wants renditions, and a page render must never wake
    // Glacier.
    assert!(!found.allow_original);
    assert!(!found.allow_restore);
    assert!(found.previous_sealed_secret.is_none());
    assert!(found.secret_rotated_at.is_none());
}

#[tokio::test]
async fn one_site_per_kind_is_a_named_refusal() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");

    connectors::register(
        &mut conn,
        &new(Uuid::new_v4(), "https://example.test", "v1.k1.n.a"),
    )
    .await
    .expect("first");

    // A second registration of the same install would leave two rows signing with different secrets, and
    // whichever the site happened to hold would work while the other quietly did not.
    let refused = connectors::register(
        &mut conn,
        &new(Uuid::new_v4(), "https://example.test", "v1.k1.n.b"),
    )
    .await;
    match refused {
        Err(ConnectorRefusal::AlreadyConnected { kind, site_url }) => {
            assert_eq!(kind, "drupal");
            assert_eq!(site_url, "https://example.test");
        }
        other => panic!("expected AlreadyConnected, got {other:?}"),
    }

    // Another *kind* at the same URL is fine: one host can run a Drupal site and a WordPress one.
    let mut wordpress = new(Uuid::new_v4(), "https://example.test", "v1.k1.n.c");
    wordpress.kind = Kind::WordPress;
    connectors::register(&mut conn, &wordpress)
        .await
        .expect("a different kind at the same host");
}

#[tokio::test]
async fn a_scheduled_rotation_keeps_the_old_secret_for_the_window() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = Uuid::new_v4();
    connectors::register(&mut conn, &new(id, "https://example.test", "v1.k1.n.old"))
        .await
        .expect("register");

    let rotated_at = Utc::now();
    connectors::rotate(&mut conn, id, "v1.k1.n.new", true, rotated_at)
        .await
        .expect("rotate");

    let found = connectors::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.sealed_secret, "v1.k1.n.new");
    assert_eq!(found.previous_sealed_secret.as_deref(), Some("v1.k1.n.old"));

    // Live inside the window, so the site keeps rendering until its own deploy lands.
    assert!(found.previous_is_live(rotated_at + Duration::hours(1)));
    assert_eq!(
        found.live_previous(rotated_at + Duration::hours(1)),
        Some("v1.k1.n.old")
    );

    // And dead after it — from the comparison, with the column still populated. That is the point: a cleanup
    // job that fails leaves a superseded secret valid forever, and a comparison cannot fail that way.
    let after = rotated_at + SECRET_GRACE + Duration::seconds(1);
    assert!(!found.previous_is_live(after));
    assert_eq!(found.live_previous(after), None);
    assert!(
        found.previous_sealed_secret.is_some(),
        "the column is still set; expiry is decided by the clock, not by clearing it"
    );
}

#[tokio::test]
async fn a_leak_rotation_kills_the_old_secret_immediately() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = Uuid::new_v4();
    connectors::register(
        &mut conn,
        &new(id, "https://example.test", "v1.k1.n.leaked"),
    )
    .await
    .expect("register");

    let now = Utc::now();
    connectors::rotate(&mut conn, id, "v1.k1.n.fresh", false, now)
        .await
        .expect("rotate");

    let found = connectors::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.sealed_secret, "v1.k1.n.fresh");
    // Gone, not expiring. The whole point of rotating after a leak is that the old one stops working now.
    assert!(found.previous_sealed_secret.is_none());
    assert!(!found.previous_is_live(now));
    assert_eq!(found.live_previous(now), None);
}

#[tokio::test]
async fn a_second_rotation_does_not_resurrect_the_first_secret() {
    // The subtle one: rotate twice inside the window and the *original* must be gone, not still verifying.
    // `previous` holds one secret, so the second rotation has to displace the first — and if it did not, a
    // secret two rotations old would keep working.
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = Uuid::new_v4();
    connectors::register(&mut conn, &new(id, "https://example.test", "v1.k1.n.one"))
        .await
        .expect("register");

    let now = Utc::now();
    connectors::rotate(&mut conn, id, "v1.k1.n.two", true, now)
        .await
        .expect("first rotation");
    connectors::rotate(
        &mut conn,
        id,
        "v1.k1.n.three",
        true,
        now + Duration::minutes(1),
    )
    .await
    .expect("second rotation");

    let found = connectors::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.sealed_secret, "v1.k1.n.three");
    assert_eq!(found.previous_sealed_secret.as_deref(), Some("v1.k1.n.two"));
    assert_ne!(
        found.previous_sealed_secret.as_deref(),
        Some("v1.k1.n.one"),
        "a secret two rotations old must not still verify"
    );
}

#[tokio::test]
async fn revoking_clears_both_secrets_and_is_terminal() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = Uuid::new_v4();
    connectors::register(&mut conn, &new(id, "https://example.test", "v1.k1.n.old"))
        .await
        .expect("register");
    let now = Utc::now();
    connectors::rotate(&mut conn, id, "v1.k1.n.new", true, now)
        .await
        .expect("rotate");

    assert!(
        connectors::set_status(&mut conn, id, Status::Revoked)
            .await
            .expect("revoke")
    );

    let found = connectors::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(found.status, Status::Revoked);
    assert!(!found.status.may_render());
    // Both gone. Leaving them would mean a row that could be edited back to `active` and immediately honour
    // every URL the remote had ever signed.
    assert_eq!(found.sealed_secret, "");
    assert!(found.previous_sealed_secret.is_none());

    // Terminal: nothing brings it back, and rotating it is refused by name rather than silently working.
    assert!(
        !connectors::set_status(&mut conn, id, Status::Active)
            .await
            .expect("reactivate"),
        "a revoked connector is never reactivated"
    );
    match connectors::rotate(&mut conn, id, "v1.k1.n.another", true, now).await {
        Err(ConnectorRefusal::Revoked(which)) => assert_eq!(which, id),
        other => panic!("expected Revoked, got {other:?}"),
    }
}

#[tokio::test]
async fn pausing_is_reversible_and_an_error_still_renders() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = Uuid::new_v4();
    connectors::register(&mut conn, &new(id, "https://example.test", "v1.k1.n.s"))
        .await
        .expect("register");

    connectors::set_status(&mut conn, id, Status::Paused)
        .await
        .expect("pause");
    let paused = connectors::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert!(!paused.status.may_render());
    // Unlike revoking: the secret is kept, because a pause is meant to be undone.
    assert_eq!(paused.sealed_secret, "v1.k1.n.s");

    connectors::set_status(&mut conn, id, Status::Error)
        .await
        .expect("error");
    let failing = connectors::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    // An error must not stop a site rendering. Whatever went wrong — a failed webhook, a bad response — is not
    // a reason to blank the images on somebody's home page.
    assert!(failing.status.may_render());
}

#[tokio::test]
async fn a_heartbeat_records_the_version_and_clears_the_last_error() {
    let (_pg, pool) = db().await;
    let mut conn = pool.acquire().await.expect("conn");
    let id = Uuid::new_v4();
    connectors::register(&mut conn, &new(id, "https://example.test", "v1.k1.n.s"))
        .await
        .expect("register");

    connectors::record_error(&mut conn, id, "webhook 500")
        .await
        .expect("record");
    let failing = connectors::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(failing.last_error.as_deref(), Some("webhook 500"));
    // Recording an error does not change the status: something going wrong and whether that should stop the
    // connector are different decisions.
    assert_eq!(failing.status, Status::Active);

    let now = Utc::now();
    connectors::seen(&mut conn, id, Some("drupal 11.2 / damrs_dam 1.1.0"), now)
        .await
        .expect("seen");
    let healthy = connectors::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(
        healthy.remote_version.as_deref(),
        Some("drupal 11.2 / damrs_dam 1.1.0")
    );
    assert!(healthy.last_error.is_none(), "a successful call clears it");
    assert!(healthy.last_seen_at.is_some());

    // A heartbeat with no version keeps the one on record rather than blanking it — the version is what an
    // operator needs when a site starts failing, and losing it to a call that did not send one is a downgrade.
    connectors::seen(&mut conn, id, None, now + Duration::minutes(5))
        .await
        .expect("seen");
    let still = connectors::by_id(&mut conn, id)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(
        still.remote_version.as_deref(),
        Some("drupal 11.2 / damrs_dam 1.1.0")
    );
}

#[tokio::test]
async fn the_sealing_context_is_bound_to_the_tenant_and_the_row() {
    // Not a database test — a statement of the rule, next to the table it applies to. A secret sealed for one
    // connector must not open for another, and neither must one moved between tenants.
    let one = Uuid::new_v4();
    let two = Uuid::new_v4();
    assert_eq!(
        connectors::associated_data("t_acme", one),
        format!("t_acme:connector:{one}")
    );
    assert_ne!(
        connectors::associated_data("t_acme", one),
        connectors::associated_data("t_acme", two)
    );
    assert_ne!(
        connectors::associated_data("t_acme", one),
        connectors::associated_data("t_other", one)
    );
}
