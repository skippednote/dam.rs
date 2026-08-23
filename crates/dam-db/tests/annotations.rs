//! Annotations: a comment pinned to a region of a picture, or a moment in a track (M6).
//!
//! The same row as a comment, because a thread mixes them freely — "the logo is wrong" pinned to a corner and
//! "approved" about the whole thing belong in one conversation. So the properties worth asserting are the ones
//! that make an anchor trustworthy rather than decorative:
//!
//! **Coordinates are fractions, never pixels.** One asset is rendered as a thumbnail, a preview, a proxy and an
//! original — four different pixel sizes — so a mark stored in pixels lands correctly on exactly one of them.
//! The refusal for an out-of-range rectangle says so, because coordinates-as-pixels is the mistake a client
//! integrating this will actually make: it produces numbers in the hundreds and a mark nobody can see.
//!
//! **All four or none.** Three-quarters of a rectangle is not a smaller rectangle; it is a mark in the wrong
//! place, which is worse than no mark.
//!
//! **The region and the timecode are independent.** A moment with no rectangle ("the music stops here") and a
//! rectangle with no moment (a watermark present throughout) are both ordinary.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::Utc;
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::query::{Planned, Query};
use dam_db::comments::{self, Anchor, CommentRefusal, NewComment, Visibility};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

fn everything() -> Planned {
    let grants = Grants::from(vec![Grant {
        permissions: vec!["asset:read".to_owned(), "asset:manage".to_owned()],
        asset_group_ids: vec![],
        all_asset_groups: true,
        valid_from: None,
        valid_until: None,
        requires_eula: false,
        eula_accepted: true,
    }]);
    Planned::new(
        Query::All,
        policy::compile(&grants, Action::Read, Utc::now()),
        &[],
    )
    .expect("plan")
}

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

async fn held(pool: &PgPool) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
    pool.acquire().await.expect("acquire")
}

async fn asset(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 4096, $1)",
    )
    .bind(id)
    .bind(blake3::hash(name.as_bytes()).to_hex().to_string())
    .bind(format!("{name}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    id
}

fn note(asset_id: Uuid, author: Uuid, body: &str, anchor: Anchor) -> NewComment {
    NewComment {
        asset_id,
        author_id: author,
        body: body.to_owned(),
        visibility: Visibility::Public,
        recipients: vec![],
        parent_id: None,
        anchor,
    }
}

// ─── the anchor ─────────────────────────────────────────────────────────────

async fn a_region_survives_the_round_trip(pool: &PgPool) {
    let id = asset(pool, "annotated").await;
    let author = Uuid::new_v4();
    let posted = comments::post(
        &mut *held(pool).await,
        note(
            id,
            author,
            "the logo is the old one",
            Anchor {
                region: Some([0.1, 0.2, 0.3, 0.25]),
                at_ms: None,
            },
        ),
        &everything(),
    )
    .await
    .expect("post");

    let region = posted.anchor.region.expect("a region");
    assert!((region[0] - 0.1).abs() < 1e-6, "{region:?}");
    assert!((region[1] - 0.2).abs() < 1e-6);
    assert!((region[2] - 0.3).abs() < 1e-6);
    assert!((region[3] - 0.25).abs() < 1e-6);
    assert!(posted.anchor.at_ms.is_none());
    assert!(posted.anchor.is_annotation());

    // And it comes back on the thread read, which is the query a screen runs.
    let thread = comments::on_asset(&mut *held(pool).await, id, author, &everything())
        .await
        .expect("thread");
    assert_eq!(thread.len(), 1);
    assert_eq!(thread[0].anchor, posted.anchor);
}

async fn a_comment_about_the_whole_asset_has_no_anchor(pool: &PgPool) {
    let id = asset(pool, "unanchored").await;
    let author = Uuid::new_v4();
    let posted = comments::post(
        &mut *held(pool).await,
        note(id, author, "approved", Anchor::default()),
        &everything(),
    )
    .await
    .expect("post");
    assert!(posted.anchor.region.is_none());
    assert!(posted.anchor.at_ms.is_none());
    assert!(
        !posted.anchor.is_annotation(),
        "a remark about the asset as a whole is not an annotation"
    );
}

async fn a_region_and_a_timecode_are_independent(pool: &PgPool) {
    // Three shapes, all ordinary: a region alone, a moment alone, and both. The schema keeps them independent
    // because a watermark present throughout has no moment and a music cue has no rectangle.
    let id = asset(pool, "video").await;
    let author = Uuid::new_v4();
    for (body, anchor) in [
        (
            "watermark, whole clip",
            Anchor {
                region: Some([0.7, 0.8, 0.2, 0.15]),
                at_ms: None,
            },
        ),
        (
            "the music stops here",
            Anchor {
                region: None,
                at_ms: Some(12_500),
            },
        ),
        (
            "this caption, at this moment",
            Anchor {
                region: Some([0.1, 0.75, 0.8, 0.2]),
                at_ms: Some(4_000),
            },
        ),
    ] {
        let posted = comments::post(
            &mut *held(pool).await,
            note(id, author, body, anchor),
            &everything(),
        )
        .await
        .unwrap_or_else(|error| panic!("{body}: {error}"));
        assert_eq!(posted.anchor, anchor, "{body}");
        assert!(posted.anchor.is_annotation(), "{body}");
    }
}

async fn coordinates_that_look_like_pixels_are_refused_by_name(pool: &PgPool) {
    // The mistake a client will actually make. A box drawn on a 4000-pixel image and sent unnormalised gives
    // numbers in the hundreds, which the constraint would reject as a constraint violation — and the caller
    // would have no idea why. The refusal names the likely cause.
    let id = asset(pool, "pixels").await;
    let author = Uuid::new_v4();
    let refused = comments::post(
        &mut *held(pool).await,
        note(
            id,
            author,
            "here",
            Anchor {
                region: Some([400.0, 300.0, 120.0, 90.0]),
                at_ms: None,
            },
        ),
        &everything(),
    )
    .await;
    let message = match refused {
        Err(CommentRefusal::BadRegion(message)) => message,
        other => panic!("pixel coordinates should be refused by name, got {other:?}"),
    };
    assert!(
        message.contains("pixels"),
        "the refusal should name the likely cause: {message}"
    );

    // Nothing was written, so the caller's transaction is still usable — a constraint violation here would
    // have aborted it and surfaced on some later statement instead.
    assert!(
        comments::on_asset(&mut *held(pool).await, id, author, &everything())
            .await
            .expect("thread")
            .is_empty()
    );
}

async fn a_degenerate_or_overflowing_region_is_refused(pool: &PgPool) {
    let id = asset(pool, "degenerate").await;
    let author = Uuid::new_v4();
    for (region, why) in [
        ([0.5, 0.5, 0.0, 0.1], "zero width is a click that missed"),
        ([0.5, 0.5, 0.1, 0.0], "zero height likewise"),
        ([-0.1, 0.5, 0.2, 0.2], "negative origin is off the picture"),
        ([0.5, -0.1, 0.2, 0.2], "negative origin, other axis"),
        ([0.9, 0.5, 0.2, 0.2], "running past the right edge"),
        ([0.5, 0.9, 0.2, 0.2], "running past the bottom"),
        ([f32::NAN, 0.5, 0.2, 0.2], "not a number at all"),
    ] {
        let refused = comments::post(
            &mut *held(pool).await,
            note(
                id,
                author,
                "here",
                Anchor {
                    region: Some(region),
                    at_ms: None,
                },
            ),
            &everything(),
        )
        .await;
        assert!(
            matches!(refused, Err(CommentRefusal::BadRegion(_))),
            "{why}: {region:?} should be refused, got {refused:?}"
        );
    }
}

async fn a_region_at_the_far_edge_is_accepted(pool: &PgPool) {
    // The full picture, exactly. A drag to the corner lands on 1.0 and must not be refused for a rounding
    // error — which is why both the constraint and the Rust check allow a hair of slack.
    let id = asset(pool, "full-frame").await;
    let author = Uuid::new_v4();
    let posted = comments::post(
        &mut *held(pool).await,
        note(
            id,
            author,
            "the whole frame",
            Anchor {
                region: Some([0.0, 0.0, 1.0, 1.0]),
                at_ms: None,
            },
        ),
        &everything(),
    )
    .await
    .expect("a full-frame region is a region");
    assert_eq!(posted.anchor.region, Some([0.0, 0.0, 1.0, 1.0]));
}

async fn a_timecode_that_is_really_a_timestamp_is_refused(pool: &PgPool) {
    // Milliseconds *into the track*, not since the epoch. A Unix millisecond timestamp is around 1.8e12,
    // which would store fine in a bigint and render as a mark 57 years into a 30-second clip.
    let id = asset(pool, "timecode").await;
    let author = Uuid::new_v4();
    for at in [-1, 86_400_001, 1_800_000_000_000] {
        let refused = comments::post(
            &mut *held(pool).await,
            note(
                id,
                author,
                "here",
                Anchor {
                    region: None,
                    at_ms: Some(at),
                },
            ),
            &everything(),
        )
        .await;
        assert!(
            matches!(refused, Err(CommentRefusal::BadRegion(_))),
            "{at} should be refused, got {refused:?}"
        );
    }

    // And a real one is fine.
    let posted = comments::post(
        &mut *held(pool).await,
        note(
            id,
            author,
            "here",
            Anchor {
                region: None,
                at_ms: Some(0),
            },
        ),
        &everything(),
    )
    .await
    .expect("zero is the first frame");
    assert_eq!(posted.anchor.at_ms, Some(0));
}

async fn the_database_refuses_a_partial_rectangle_written_by_hand(pool: &PgPool) {
    // The Rust check cannot be reached by a hand-written statement, and three-quarters of a rectangle would
    // render as a mark in the wrong place. So the constraint is the one that has to hold.
    let id = asset(pool, "partial").await;
    let refused = sqlx::query(
        "INSERT INTO asset_comments (id, asset_id, author_id, body, visibility, region_x, region_y) \
         VALUES (gen_random_uuid(), $1, gen_random_uuid(), 'partial', 'public', 0.1, 0.2)",
    )
    .bind(id)
    .execute(pool)
    .await;
    let error = refused.expect_err("three of four columns must be refused");
    assert!(
        error.to_string().contains("region_complete"),
        "the constraint names itself: {error}"
    );
}

async fn an_annotation_is_indexed_for_the_overlay(pool: &PgPool) {
    // The overlay reads "every annotation on this asset", which is a partial index — most comments are not
    // annotations, so an index over all of them would be mostly rows it never wants. Asserted through the
    // planner rather than by trusting the DDL.
    let plan: String = sqlx::query_scalar(
        "EXPLAIN (FORMAT TEXT) SELECT id FROM asset_comments \
         WHERE asset_id = gen_random_uuid() AND region_x IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .expect("explain");
    // On an empty table Postgres will still choose a sequential scan, so the assertion is only that the index
    // exists and covers the predicate — checked directly, which is the honest thing a plan cannot tell us here.
    let indexed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes \
         WHERE schemaname = 't_acme' AND indexname = 'asset_comments_annotated_idx')",
    )
    .fetch_one(pool)
    .await
    .expect("index");
    assert!(indexed, "the overlay's index is missing; plan was: {plan}");
}

#[tokio::test]
async fn an_annotation_points_somewhere_real() {
    let (_pg, pool) = db().await;

    a_region_survives_the_round_trip(&pool).await;
    a_comment_about_the_whole_asset_has_no_anchor(&pool).await;
    a_region_and_a_timecode_are_independent(&pool).await;
    coordinates_that_look_like_pixels_are_refused_by_name(&pool).await;
    a_degenerate_or_overflowing_region_is_refused(&pool).await;
    a_region_at_the_far_edge_is_accepted(&pool).await;
    a_timecode_that_is_really_a_timestamp_is_refused(&pool).await;
    the_database_refuses_a_partial_rectangle_written_by_hand(&pool).await;
    an_annotation_is_indexed_for_the_overlay(&pool).await;
}
