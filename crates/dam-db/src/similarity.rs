//! Perceptual hashes, dominant colours, and the near-duplicate queue (M4, §8.1).
//!
//! Three tables that have been in migration 0003 since the start with nothing writing to them:
//! `asset_phashes`, `asset_colors` and `duplicate_candidates`.
//!
//! ## The Hamming distance runs in Postgres
//!
//! `bit_count(a # b)` on `bigint`, which PG14 and later provide. The alternative was reading every hash in the
//! tenant into Rust and comparing there — fine at ten thousand assets and a full table transfer per upload at
//! a hundred thousand. It is still a sequential scan, because Hamming distance has no btree ordering to
//! exploit; what it avoids is moving the rows.
//!
//! A BK-tree or an LSH index is the answer past a few hundred thousand assets, and this is deliberately not
//! that: the scan is one pass over a two-column table, and the point at which it stops being adequate is far
//! past the point at which somebody will have measured it.
//!
//! ## The hash is stored as a signed bigint, and that is fine
//!
//! `u64` does not fit `bigint`, so it goes in bit-for-bit as `i64` — half the values come back negative.
//! Nothing compares them for magnitude: XOR and population count are bit operations, and the ordering of two
//! hashes has no meaning. The cast is `as`, deliberately, and this is the note explaining why that is not a
//! bug waiting to happen.
//!
//! ## A pair is stored once
//!
//! 0003 has `CHECK (asset_id < other_id)`, so the caller cannot decide which way round to write a pair.
//! [`record_candidates`] orders every pair before inserting, which is also what makes the unique index on
//! `(asset_id, other_id)` a real deduplication rather than one that admits both directions.

use crate::Error;
use uuid::Uuid;

/// One dominant colour, as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct Colour {
    pub hex: String,
    pub lab: [f32; 3],
    pub coverage: f32,
    pub palette_bucket: String,
}

/// The columns [`open_candidates`] reads back, in order.
///
/// A named alias rather than an inline tuple: the shape appears in the query, the binding and the mapping, and
/// three copies of a seven-element tuple is where a column gets silently transposed.
type CandidateRow = (
    Uuid,
    Uuid,
    Uuid,
    Option<i16>,
    Option<f32>,
    Option<String>,
    String,
);

/// A near-duplicate pair awaiting a human.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub other_id: Uuid,
    pub hamming: Option<i16>,
    pub cosine: Option<f32>,
    pub relation: Option<String>,
    pub state: String,
}

/// A hash pair to compare against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hashes {
    pub phash: u64,
    pub dhash: u64,
}

/// Stores one asset's hashes, replacing any it already had.
///
/// Idempotent, because a re-process must not fail and must not accumulate rows. `asset_phashes` is keyed on
/// the asset alone, so the upsert is the natural shape.
pub async fn record_hashes(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    hashes: Hashes,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO asset_phashes (asset_id, phash, dhash) VALUES ($1, $2, $3) \
         ON CONFLICT (asset_id) DO UPDATE SET phash = excluded.phash, dhash = excluded.dhash",
    )
    .bind(asset_id)
    .bind(signed(hashes.phash))
    .bind(signed(hashes.dhash))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Replaces one asset's colours.
///
/// Deleted and re-inserted rather than upserted, because the *number* of colours can shrink: an image that
/// used to yield five clusters and now yields two would otherwise keep three stale rows at ranks 2 to 4, and a
/// facet would count colours the picture no longer has.
pub async fn record_colours(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    colours: &[Colour],
) -> Result<(), Error> {
    sqlx::query("DELETE FROM asset_colors WHERE asset_id = $1")
        .bind(asset_id)
        .execute(&mut *conn)
        .await?;

    for (rank, colour) in colours.iter().enumerate() {
        sqlx::query(
            "INSERT INTO asset_colors (asset_id, rank, hex, lab, coverage, palette_bucket) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(asset_id)
        .bind(i16::try_from(rank).unwrap_or(i16::MAX))
        .bind(&colour.hex)
        .bind(colour.lab.as_slice())
        .bind(colour.coverage)
        .bind(&colour.palette_bucket)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Every asset within `threshold` bits of `hashes`, excluding `asset_id` itself.
///
/// Returns the *closer* of the two hash distances per row, matching
/// `dam_media::similarity::Hashes::distance` — the two algorithms fail on different transformations, so a pair
/// is worth surfacing when either says the pictures are alike.
///
/// **Assets sharing a content hash are excluded**, and that is not an optimisation. 0003 is explicit: "exact
/// duplicates are free — identical BLAKE3 means one object, caught at ingest. This table is for NEAR
/// duplicates." Two asset rows can legitimately share a content hash — the same file uploaded twice, into two
/// collections, by two people — and a perceptual hash tells a reviewer nothing they could not get from the
/// hash they already have. Running this over a real library made the point: 33 of 84 pairs, nearly half the
/// queue, were byte-identical, which is exactly the noise that makes a review queue go unread.
///
/// **A collapsed hash is excluded from the comparison**, on both sides. The same run turned up a 932-byte test
/// pattern paired with an MP4 at distance 0: both had `dhash = 0`, because neither picture has any
/// pixel-to-pixel variation for the gradient hash to record, and `least` then reported them identical while
/// the DCT hashes were correctly saying otherwise.
///
/// That is only half the defence. The other half is upstream: an image with too little tonal variation gets no
/// hash stored at all, so it is absent from this table and cannot be found or find anything. It has to work
/// that way, because the DCT hash *cannot* be detected as degenerate from its bits — see
/// `dam_media::similarity::MIN_LUMA_DEVIATION`.
///
/// `IS DISTINCT FROM` rather than `<>`, and the difference is not stylistic: the subquery is NULL when
/// `asset_id` names no row, and `content_hash <> NULL` is NULL — so a plain comparison excluded *everything*
/// and the function silently returned nothing. A test that probes with an id not in `assets` caught it.
pub async fn near(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    hashes: Hashes,
    threshold: u32,
) -> Result<Vec<(Uuid, i16)>, Error> {
    // A degenerate hash is excluded from the comparison on *both* sides, mirroring
    // `dam_media::similarity::Hashes::distance`. `NULL` for an unusable hash, so `least` ignores it — and a
    // row where neither hash is usable yields `NULL` and fails the threshold, which is what drops it.
    //
    // A collapsed hash — all zeroes or all ones — is excluded from the comparison on both sides, which
    // mirrors `dam_media::similarity::Hashes::distance`. `NULL` for an unusable one, so `least` ignores it; a
    // row where neither is usable yields `NULL`, fails the threshold, and drops out.
    //
    // `$5` and `$6` say whether the *probe's* hashes are usable. Passed in rather than recomputed in SQL
    // because the caller already knows, and two implementations of one rule is how they drift apart.
    let rows: Vec<(Uuid, Option<i32>)> = sqlx::query_as(
        "SELECT p.asset_id, \
                least( \
                  CASE WHEN $5 AND bit_count(p.phash::bit(64)) NOT IN (0, 64) \
                       THEN bit_count((p.phash # $2)::bit(64)) END, \
                  CASE WHEN $6 AND bit_count(p.dhash::bit(64)) NOT IN (0, 64) \
                       THEN bit_count((p.dhash # $3)::bit(64)) END \
                )::int AS distance \
         FROM asset_phashes p \
         JOIN assets a ON a.id = p.asset_id \
         WHERE p.asset_id <> $1 \
           AND a.deleted_at IS NULL \
           AND a.is_current \
           AND a.attached_to IS NULL \
           AND a.content_hash IS DISTINCT FROM (SELECT content_hash FROM assets WHERE id = $1) \
           AND least( \
                 CASE WHEN $5 AND bit_count(p.phash::bit(64)) NOT IN (0, 64) \
                      THEN bit_count((p.phash # $2)::bit(64)) END, \
                 CASE WHEN $6 AND bit_count(p.dhash::bit(64)) NOT IN (0, 64) \
                      THEN bit_count((p.dhash # $3)::bit(64)) END \
               ) <= $4 \
         ORDER BY distance, p.asset_id",
    )
    .bind(asset_id)
    .bind(signed(hashes.phash))
    .bind(signed(hashes.dhash))
    .bind(i32::try_from(threshold).unwrap_or(i32::MAX))
    .bind(dam_media::similarity::discriminative(hashes.phash))
    .bind(dam_media::similarity::discriminative(hashes.dhash))
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(id, distance)| Some((id, i16::try_from(distance?).unwrap_or(i16::MAX))))
        .collect())
}

/// Records candidate pairs, leaving any a human has already resolved alone.
///
/// `DO NOTHING` on conflict rather than an update, and that is the important half: a re-process must not
/// resurrect a pair somebody dismissed. Re-opening a dismissed duplicate on every reprocess is how a review
/// queue becomes something people ignore.
pub async fn record_candidates(
    conn: &mut sqlx::PgConnection,
    asset_id: Uuid,
    found: &[(Uuid, i16)],
) -> Result<u64, Error> {
    let mut inserted = 0;
    for (other, hamming) in found {
        // Ordered, because 0003 has `CHECK (asset_id < other_id)`: a pair is one row, not two.
        let (left, right) = if asset_id < *other {
            (asset_id, *other)
        } else {
            (*other, asset_id)
        };
        inserted += sqlx::query(
            "INSERT INTO duplicate_candidates (id, asset_id, other_id, hamming, relation) \
             VALUES (gen_random_uuid(), $1, $2, $3, $4) \
             ON CONFLICT (asset_id, other_id) DO NOTHING",
        )
        .bind(left)
        .bind(right)
        .bind(hamming)
        .bind(relation_for(*hamming))
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    Ok(inserted)
}

/// A guess at what kind of duplicate this is, from the distance alone.
///
/// Only two of the schema's five values are reachable without an embedding: `near_identical` and `variant`.
/// The rest — `crop`, `recolor`, `rescale` — need the cosine similarity of two embeddings to tell apart, which
/// is the model-dependent half of M4. Returning a value this cannot support would be a label a reviewer trusts
/// and should not.
fn relation_for(hamming: i16) -> &'static str {
    if hamming <= 2 {
        "near_identical"
    } else {
        "variant"
    }
}

/// The open review queue, closest pairs first.
///
/// Ordered by distance rather than by age: a reviewer working down the list should see the ones most likely to
/// be real duplicates first, and an eighteen-month-old pair at distance 1 matters more than yesterday's at 11.
pub async fn open_candidates(
    conn: &mut sqlx::PgConnection,
    limit: i64,
) -> Result<Vec<Candidate>, Error> {
    let rows: Vec<CandidateRow> = sqlx::query_as(
        "SELECT d.id, d.asset_id, d.other_id, d.hamming, d.cosine, d.relation, d.state \
             FROM duplicate_candidates d \
             JOIN assets a ON a.id = d.asset_id AND a.deleted_at IS NULL \
             JOIN assets b ON b.id = d.other_id AND b.deleted_at IS NULL \
             WHERE d.state = 'open' \
             ORDER BY d.hamming NULLS LAST, d.created_at LIMIT $1",
    )
    .bind(limit.clamp(1, 500))
    .fetch_all(&mut *conn)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, asset_id, other_id, hamming, cosine, relation, state)| Candidate {
                id,
                asset_id,
                other_id,
                hamming,
                cosine,
                relation,
                state,
            },
        )
        .collect())
}

/// Resolves a candidate.
///
/// `merged` is accepted and recorded but merges nothing: 0003 is explicit that "auto-merging a crop that is
/// actually a different licensed deliverable is a rights problem, so a human decides". What a merge *means* —
/// which asset survives, what happens to the other's rights and references — is a separate decision this
/// table only records the outcome of.
pub async fn resolve(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    state: &str,
    by: Option<Uuid>,
) -> Result<bool, Error> {
    if !matches!(state, "confirmed" | "dismissed" | "merged") {
        return Err(Error::Unsupported(format!(
            "{state:?} is not a resolution; use confirmed, dismissed or merged"
        )));
    }
    let updated = sqlx::query(
        "UPDATE duplicate_candidates \
         SET state = $2, resolved_by = $3, resolved_at = now() \
         WHERE id = $1 AND state = 'open'",
    )
    .bind(id)
    .bind(state)
    .bind(by)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// The colour buckets present in the library, with counts — for a facet.
///
/// Counts the *primary* colour only (`rank = 0`). Counting every rank would make the numbers sum to five times
/// the library and put every asset in four buckets it is only incidentally in, which is not what somebody
/// clicking "blue" is asking for.
pub async fn colour_buckets(conn: &mut sqlx::PgConnection) -> Result<Vec<(String, i64)>, Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT c.palette_bucket, count(*) \
         FROM asset_colors c \
         JOIN assets a ON a.id = c.asset_id \
         WHERE c.rank = 0 AND a.deleted_at IS NULL AND a.is_current AND a.attached_to IS NULL \
         GROUP BY c.palette_bucket ORDER BY count(*) DESC, c.palette_bucket",
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows)
}

/// A `u64` hash as the `bigint` the column holds.
///
/// Bit-for-bit, so half of them are negative. Nothing compares two hashes for magnitude — XOR and population
/// count are bit operations — so the sign is meaningless rather than wrong.
#[expect(
    clippy::cast_possible_wrap,
    reason = "a bit pattern, never compared for magnitude"
)]
const fn signed(hash: u64) -> i64 {
    hash as i64
}
