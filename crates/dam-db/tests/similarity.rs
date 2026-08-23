//! Perceptual hashes, colours, and the near-duplicate queue (M4, §8.1).
//!
//! Three tables from migration 0003 that nothing had ever written to. The properties worth asserting are the
//! ones that decide whether a review queue is usable or ignored:
//!
//! **A dismissed pair stays dismissed.** A re-process that re-opened everything somebody had already judged is
//! how a queue becomes noise. `ON CONFLICT DO NOTHING` rather than an update, and this is the test that holds
//! it there.
//!
//! **A pair is one row.** 0003 has `CHECK (asset_id < other_id)`, so the caller cannot choose a direction —
//! and the unique index only deduplicates if every writer orders the pair the same way.
//!
//! **The Hamming distance is computed in Postgres**, over hashes stored as signed `bigint` because `u64` does
//! not fit. Half the stored values are negative, which is harmless — XOR and population count are bit
//! operations — but it is exactly the kind of thing that looks like a bug later, so the round trip through
//! negative values is asserted rather than assumed.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::similarity::{self, Colour, Hashes};
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

async fn held(pool: &PgPool) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
    pool.acquire().await.expect("acquire")
}

/// An asset with a controllable id, so `asset_id < other_id` can be reasoned about.
async fn asset_with(pool: &PgPool, id: Uuid, name: &str) -> Uuid {
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

async fn asset(pool: &PgPool, name: &str) -> Uuid {
    asset_with(pool, Uuid::new_v4(), name).await
}

// ─── hashes ─────────────────────────────────────────────────────────────────

async fn a_hash_survives_the_round_trip_through_a_signed_column(pool: &PgPool) {
    // `u64::MAX` and anything above `i64::MAX` come back negative from `bigint`. Nothing compares two hashes
    // for magnitude, so that is harmless — but it is the sort of thing that looks like a bug six months later,
    // so the round trip is asserted.
    let id = asset(pool, "signed").await;
    let hashes = Hashes {
        phash: u64::MAX,
        dhash: 0x8000_0000_0000_0000,
    };
    similarity::record_hashes(&mut *held(pool).await, id, hashes)
        .await
        .expect("record");

    let (phash, dhash): (i64, i64) =
        sqlx::query_as("SELECT phash, dhash FROM asset_phashes WHERE asset_id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("read back");
    assert_eq!(phash, -1, "u64::MAX is -1 as a bit pattern");
    assert_eq!(dhash, i64::MIN);

    // And the distance still works on those bits: an identical hash is zero away.
    let found = similarity::near(&mut *held(pool).await, Uuid::new_v4(), hashes, 0)
        .await
        .expect("near");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0], (id, 0));
}

async fn recording_a_hash_twice_replaces_it(pool: &PgPool) {
    // A re-process must neither fail nor accumulate.
    let id = asset(pool, "reprocessed").await;
    for phash in [0x0000_0000_0000_00ffu64, 0xffff_0000_0000_0000] {
        similarity::record_hashes(&mut *held(pool).await, id, Hashes { phash, dhash: 1 })
            .await
            .expect("record");
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_phashes WHERE asset_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("count");
    assert_eq!(rows, 1);
    let phash: i64 = sqlx::query_scalar("SELECT phash FROM asset_phashes WHERE asset_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("phash");
    assert_eq!(phash, 0xffff_0000_0000_0000u64 as i64);
}

async fn near_takes_the_closer_of_the_two_hashes(pool: &PgPool) {
    // The whole reason there are two. This asset's phash is far from the probe and its dhash is identical — the
    // situation a brightness change produces — so it must be found at distance 0, not at the phash distance.
    let (_pg, fresh) = db().await;
    let id = asset(&fresh, "one-hash-matches").await;
    similarity::record_hashes(
        &mut *held(&fresh).await,
        id,
        Hashes {
            phash: 0xffff_ffff_ffff_ffff,
            dhash: 0x0000_0000_0000_00ff,
        },
    )
    .await
    .expect("record");

    let probe = Hashes {
        phash: 0x0000_0000_0000_0000,
        dhash: 0x0000_0000_0000_00ff,
    };
    let found = similarity::near(&mut *held(&fresh).await, Uuid::new_v4(), probe, 4)
        .await
        .expect("near");
    assert_eq!(found, vec![(id, 0)], "the matching dhash wins");
    let _ = pool;
}

async fn near_excludes_the_asset_itself_and_anything_deleted(pool: &PgPool) {
    let (_pg, fresh) = db().await;
    let me = asset(&fresh, "me").await;
    let gone = asset(&fresh, "gone").await;
    let attachment = asset(&fresh, "release-form").await;
    let hashes = Hashes {
        phash: 42,
        dhash: 42,
    };
    for id in [me, gone, attachment] {
        similarity::record_hashes(&mut *held(&fresh).await, id, hashes)
            .await
            .expect("record");
    }
    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(gone)
        .execute(&fresh)
        .await
        .expect("delete");
    // Paperwork hanging off another asset is not a library row, so it is not a duplicate of anything.
    sqlx::query("UPDATE assets SET attached_to = $2, attachment_kind = 'release' WHERE id = $1")
        .bind(attachment)
        .bind(me)
        .execute(&fresh)
        .await
        .expect("attach");

    let found = similarity::near(&mut *held(&fresh).await, me, hashes, 0)
        .await
        .expect("near");
    assert!(found.is_empty(), "{found:?}");
    let _ = pool;
}

// ─── the queue ──────────────────────────────────────────────────────────────

async fn a_pair_is_one_row_whichever_way_it_is_found(pool: &PgPool) {
    // 0003's `CHECK (asset_id < other_id)` means the caller cannot pick a direction, and the unique index only
    // deduplicates if every writer orders the pair the same way. Recorded from both ends here.
    let (_pg, fresh) = db().await;
    let low = asset_with(
        &fresh,
        Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
        "low",
    )
    .await;
    let high = asset_with(
        &fresh,
        Uuid::parse_str("99999999-9999-4999-8999-999999999999").unwrap(),
        "high",
    )
    .await;

    let inserted = similarity::record_candidates(&mut *held(&fresh).await, high, &[(low, 3)])
        .await
        .expect("record from the high end");
    assert_eq!(inserted, 1);
    let again = similarity::record_candidates(&mut *held(&fresh).await, low, &[(high, 3)])
        .await
        .expect("record from the low end");
    assert_eq!(
        again, 0,
        "the same pair the other way round is the same pair"
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM duplicate_candidates")
        .fetch_one(&fresh)
        .await
        .expect("count");
    assert_eq!(rows, 1);
    let (stored_low, stored_high): (Uuid, Uuid) =
        sqlx::query_as("SELECT asset_id, other_id FROM duplicate_candidates")
            .fetch_one(&fresh)
            .await
            .expect("pair");
    assert_eq!((stored_low, stored_high), (low, high), "stored in id order");
    let _ = pool;
}

async fn a_dismissed_pair_is_not_reopened_by_a_reprocess(pool: &PgPool) {
    // The property that decides whether anybody uses the queue. A re-process that resurrected every judgement
    // would make the list permanently full of things already dealt with.
    let (_pg, fresh) = db().await;
    let a = asset(&fresh, "kept").await;
    let b = asset(&fresh, "also-kept").await;
    similarity::record_candidates(&mut *held(&fresh).await, a, &[(b, 1)])
        .await
        .expect("record");

    let open = similarity::open_candidates(&mut *held(&fresh).await, 10)
        .await
        .expect("open");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].relation.as_deref(), Some("near_identical"));

    let reviewer = Uuid::new_v4();
    assert!(
        similarity::resolve(
            &mut *held(&fresh).await,
            open[0].id,
            "dismissed",
            Some(reviewer)
        )
        .await
        .expect("resolve")
    );

    // The re-process.
    let reinserted = similarity::record_candidates(&mut *held(&fresh).await, a, &[(b, 1)])
        .await
        .expect("record again");
    assert_eq!(reinserted, 0);
    assert!(
        similarity::open_candidates(&mut *held(&fresh).await, 10)
            .await
            .expect("open")
            .is_empty(),
        "a dismissed pair must stay dismissed"
    );

    // And resolving it twice reports false rather than overwriting who judged it.
    assert!(
        !similarity::resolve(&mut *held(&fresh).await, open[0].id, "confirmed", None)
            .await
            .expect("resolve"),
        "an already-resolved pair is not re-resolvable"
    );
    let (state, by): (String, Option<Uuid>) =
        sqlx::query_as("SELECT state, resolved_by FROM duplicate_candidates")
            .fetch_one(&fresh)
            .await
            .expect("row");
    assert_eq!(state, "dismissed");
    assert_eq!(by, Some(reviewer), "the first reviewer's judgement stands");
    let _ = pool;
}

async fn an_invented_resolution_is_refused(pool: &PgPool) {
    let (_pg, fresh) = db().await;
    let a = asset(&fresh, "left").await;
    let b = asset(&fresh, "right").await;
    similarity::record_candidates(&mut *held(&fresh).await, a, &[(b, 1)])
        .await
        .expect("record");
    let open = similarity::open_candidates(&mut *held(&fresh).await, 10)
        .await
        .expect("open");

    let refused = similarity::resolve(&mut *held(&fresh).await, open[0].id, "deleted", None).await;
    assert!(matches!(refused, Err(dam_db::Error::Unsupported(_))));
    // Refused in Rust before the statement, so the caller's transaction is untouched — a CHECK violation here
    // would abort it and surface somewhere else entirely.
    assert_eq!(
        similarity::open_candidates(&mut *held(&fresh).await, 10)
            .await
            .expect("open")
            .len(),
        1
    );
    let _ = pool;
}

async fn the_relation_only_claims_what_a_hash_can_tell(pool: &PgPool) {
    // Two of the schema's five values are reachable without an embedding. `crop`, `recolor` and `rescale` need
    // a cosine similarity to distinguish, which is the model-dependent half of M4 — and a label a reviewer
    // trusts must not be a guess.
    let (_pg, fresh) = db().await;
    let a = asset(&fresh, "identical").await;
    let b = asset(&fresh, "variant").await;
    let c = asset(&fresh, "further").await;
    similarity::record_candidates(&mut *held(&fresh).await, a, &[(b, 1), (c, 9)])
        .await
        .expect("record");

    let open = similarity::open_candidates(&mut *held(&fresh).await, 10)
        .await
        .expect("open");
    let relations: Vec<Option<&str>> = open.iter().map(|c| c.relation.as_deref()).collect();
    assert!(relations.contains(&Some("near_identical")));
    assert!(relations.contains(&Some("variant")));
    // Closest first, so a reviewer sees the likely ones before the marginal ones.
    assert_eq!(open[0].hamming, Some(1));
    assert_eq!(open[1].hamming, Some(9));
    // No cosine yet, and absent rather than zero: nothing has computed one.
    assert!(open.iter().all(|c| c.cosine.is_none()));
    let _ = pool;
}

// ─── colours ────────────────────────────────────────────────────────────────

async fn colours_are_replaced_wholesale_rather_than_upserted(pool: &PgPool) {
    // The number of colours can shrink. Upserting by rank would leave stale rows at the higher ranks, and a
    // facet would count colours the picture no longer has.
    let (_pg, fresh) = db().await;
    let id = asset(&fresh, "recoloured").await;

    let five: Vec<Colour> = (0..5)
        .map(|index| Colour {
            hex: format!("#00000{index}"),
            lab: [index as f32 * 10.0, 5.0, 5.0],
            coverage: 0.2,
            palette_bucket: "grey".to_owned(),
        })
        .collect();
    similarity::record_colours(&mut *held(&fresh).await, id, &five)
        .await
        .expect("five");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM asset_colors WHERE asset_id = $1")
        .bind(id)
        .fetch_one(&fresh)
        .await
        .expect("count");
    assert_eq!(count, 5);

    similarity::record_colours(
        &mut *held(&fresh).await,
        id,
        &[Colour {
            hex: "#ff0000".to_owned(),
            lab: [50.0, 70.0, 50.0],
            coverage: 1.0,
            palette_bucket: "red".to_owned(),
        }],
    )
    .await
    .expect("one");

    let rows: Vec<(i16, String)> =
        sqlx::query_as("SELECT rank, hex FROM asset_colors WHERE asset_id = $1 ORDER BY rank")
            .bind(id)
            .fetch_all(&fresh)
            .await
            .expect("rows");
    assert_eq!(
        rows,
        vec![(0, "#ff0000".to_owned())],
        "no stale ranks left behind"
    );
    let _ = pool;
}

async fn the_facet_counts_the_primary_colour_only(pool: &PgPool) {
    // Counting every rank would make the numbers sum to five times the library and put every asset in four
    // buckets it is only incidentally in — not what somebody clicking "blue" is asking for.
    let (_pg, fresh) = db().await;
    let blue = asset(&fresh, "mostly-blue").await;
    let red = asset(&fresh, "mostly-red").await;

    similarity::record_colours(
        &mut *held(&fresh).await,
        blue,
        &[
            Colour {
                hex: "#0000ff".to_owned(),
                lab: [30.0, 50.0, -80.0],
                coverage: 0.7,
                palette_bucket: "blue".to_owned(),
            },
            Colour {
                hex: "#ff0000".to_owned(),
                lab: [50.0, 70.0, 50.0],
                coverage: 0.3,
                palette_bucket: "red".to_owned(),
            },
        ],
    )
    .await
    .expect("blue");
    similarity::record_colours(
        &mut *held(&fresh).await,
        red,
        &[Colour {
            hex: "#ff0000".to_owned(),
            lab: [50.0, 70.0, 50.0],
            coverage: 1.0,
            palette_bucket: "red".to_owned(),
        }],
    )
    .await
    .expect("red");

    let buckets = similarity::colour_buckets(&mut *held(&fresh).await)
        .await
        .expect("buckets");
    assert_eq!(
        buckets,
        vec![("blue".to_owned(), 1), ("red".to_owned(), 1)],
        "one count per asset, from its primary colour: {buckets:?}"
    );

    // A deleted asset drops out of the facet.
    sqlx::query("UPDATE assets SET deleted_at = now() WHERE id = $1")
        .bind(blue)
        .execute(&fresh)
        .await
        .expect("delete");
    assert_eq!(
        similarity::colour_buckets(&mut *held(&fresh).await)
            .await
            .expect("buckets"),
        vec![("red".to_owned(), 1)]
    );
    let _ = pool;
}

async fn lab_survives_the_round_trip(pool: &PgPool) {
    // Stored as `real[3]` so a nearest-colour search can compare perceptually without converting every row.
    let (_pg, fresh) = db().await;
    let id = asset(&fresh, "lab").await;
    similarity::record_colours(
        &mut *held(&fresh).await,
        id,
        &[Colour {
            hex: "#c81e28".to_owned(),
            lab: [43.25, 63.3, 40.0],
            coverage: 1.0,
            palette_bucket: "red".to_owned(),
        }],
    )
    .await
    .expect("record");

    let lab: Vec<f32> = sqlx::query_scalar("SELECT lab FROM asset_colors WHERE asset_id = $1")
        .bind(id)
        .fetch_one(&fresh)
        .await
        .expect("lab");
    assert_eq!(lab.len(), 3);
    assert!((lab[0] - 43.25).abs() < 0.01, "{lab:?}");
    assert!((lab[1] - 63.3).abs() < 0.01);
    assert!((lab[2] - 40.0).abs() < 0.01);
    let _ = pool;
}

#[tokio::test]
async fn the_similarity_tables_hold_their_invariants() {
    let (_pg, pool) = db().await;

    a_hash_survives_the_round_trip_through_a_signed_column(&pool).await;
    recording_a_hash_twice_replaces_it(&pool).await;
    near_takes_the_closer_of_the_two_hashes(&pool).await;
    near_excludes_the_asset_itself_and_anything_deleted(&pool).await;

    a_pair_is_one_row_whichever_way_it_is_found(&pool).await;
    a_dismissed_pair_is_not_reopened_by_a_reprocess(&pool).await;
    an_invented_resolution_is_refused(&pool).await;
    the_relation_only_claims_what_a_hash_can_tell(&pool).await;

    colours_are_replaced_wholesale_rather_than_upserted(&pool).await;
    the_facet_counts_the_primary_colour_only(&pool).await;
    lab_survives_the_round_trip(&pool).await;
}
