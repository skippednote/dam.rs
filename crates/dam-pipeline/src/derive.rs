//! Rendering an asset's derivatives: the thumbnail, the preview, and the master proxy.
//!
//! ## Two renderers, and which one runs is decided by the file
//!
//! The pure-Rust path (`dam_media::derive::render`) handles the formats the `image` crate decodes, which is
//! most of a DAM's volume. libvips handles the rest — camera RAW, PSD, PDF, HEIF — and §16 puts it in a
//! subprocess because **libvips marks 14 of its own loaders "untrusted"**, which is to say precisely the
//! formats a DAM most needs are the ones its maintainers flag as risky on hostile input.
//!
//! So: try the pure-Rust path, fall back to vips, and if vips is not installed say so rather than recording a
//! silent failure. A worker that quietly skips RAW files produces a library where some assets have thumbnails
//! and nobody knows why.
//!
//! ## The original is read once
//!
//! §18.3 budgets the original for two reads — one to hash at ingest, one to derive. All three profiles render
//! from the single download below rather than fetching per profile, which would be three.
//!
//! ## The full-fidelity measurement happens here, not at ingest
//!
//! Finalisation measures from the first 256 KB with the pure-Rust probe, which is fast, needs no subprocess,
//! and answers for most of a library. It cannot answer for the rest: the `image` crate has no HEIF decoder, so
//! every iPhone photograph landed with no dimensions at all while its thumbnails rendered perfectly through
//! libvips — the two halves of the system disagreeing about what it can read. Video was worse: nothing probed
//! it, so a clip had no dimensions and no duration, though `avprobe` had existed and been tested all along.
//! And a JPEG whose `SOF` marker sits past 256 KB measured as unknown for a third reason again.
//!
//! One remedy for all three, and it costs no extra read: this job has already downloaded the original, so it
//! measures again with the tool that can — `vipsheader` for stills, `ffprobe` for anything timed — and fills
//! in what finalisation left null. Only nulls: a value the cheap probe *did* produce is not overwritten, so a
//! re-run cannot flip a dimension, and the fast path stays authoritative where it worked.
//!
//! Found by uploading a real iPhone library rather than by reading the code. Every one of the three was
//! invisible: `probe::image(bytes).ok()` discards the error, so "could not measure" and "has no dimensions"
//! were the same row.
//!
//! ## Idempotent on `(asset_id, op_hash)`
//!
//! `derivatives::record` is `ON CONFLICT DO NOTHING`, and the object key is derived from the op hash — so a
//! re-run overwrites an identical object and inserts nothing. That is what makes this safe under an
//! at-least-once queue, and it is also why a *redefined* profile is a new hash rather than an overwrite: the
//! old bytes stay addressable until something reaps them, and no reader is served a rendition it did not ask
//! for.

use crate::{Error, Result};
use dam_core::StorageClass;
use dam_db::TenantConn;
use dam_media::derive::Rendition;
use dam_media::profiles::{self, Profile};
use dam_store::{BlobStore, Key};
use std::borrow::Cow;
use uuid::Uuid;

/// What derivation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    pub asset_id: Uuid,
    /// Profile names that now have an object and a row.
    pub rendered: Vec<String>,
    /// Profiles skipped because they were already recorded.
    pub already: Vec<String>,
    /// Profiles that could not be rendered, with the reason.
    ///
    /// Reported rather than turned into a job failure. **A format no renderer can read is not a broken asset**
    /// — a DAM stores text files, spreadsheets and archives, and none of those has a thumbnail. The grid draws
    /// a placeholder, which is the honest thing to draw.
    ///
    /// A *missing tool* is different and does not appear here: it fails the job, because it is an environment
    /// problem that a correctly configured worker will not have. That distinction was wrong in the first
    /// version — a `.txt` came back as transient, so the queue retried it five times and dead-lettered it.
    pub refused: Vec<(String, String)>,
}

impl Derived {
    /// Whether anything at all can be shown for this asset.
    pub fn has_thumbnail(&self) -> bool {
        self.rendered
            .iter()
            .chain(self.already.iter())
            .any(|name| profiles::by_name(name).is_some_and(|profile| profile.role == "thumbnail"))
    }
}

/// Renders every built-in profile for `asset_id`.
pub async fn asset(
    global: &sqlx::PgPool,
    store: &dyn BlobStore,
    slug: &dam_core::TenantSlug,
    tenant_id: Uuid,
    asset_id: Uuid,
    // The deployment's C2PA signing identity, when one is configured. `None` renders without credentials,
    // which is the default and is why `provenance_gaps` exists. Passed in rather than read from configuration
    // here, for the same reason the store is: the pipeline has no business knowing how the deployment is
    // configured.
    identity: Option<&dam_media::provenance::SigningIdentity>,
) -> Result<Derived> {
    let mut conn = TenantConn::begin(global, slug).await?;
    // `filename` rides along for the C2PA ingredient's title. A manifest whose parent is labelled with a
    // content hash is technically complete and useless to a human reading the chain.
    let row = sqlx::query_as::<
        _,
        (
            String,
            String,
            i64,
            Option<i32>,
            Option<i32>,
            Option<i64>,
            String,
        ),
    >(
        "SELECT content_hash, mime, bytes, width, height, duration_ms, filename \
         FROM assets WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(asset_id)
    .fetch_optional(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;
    conn.commit().await?;

    let Some((content_hash, mime, _bytes, width, height, duration_ms, filename)) = row else {
        // Deleted between the job being queued and being claimed. Permanent, and not an error worth alarming
        // about: the queue is asynchronous and a user deleting an asset immediately after uploading it is
        // ordinary.
        return Err(Error::Permanent(format!(
            "asset {asset_id} does not exist or was deleted"
        )));
    };

    // Which profiles are already recorded, in one query rather than one per profile.
    let mut conn = TenantConn::begin(global, slug).await?;
    let recorded = dam_db::derivatives::op_hashes_for(conn.executor(), asset_id).await?;
    conn.commit().await?;

    let outstanding: Vec<&Profile> = profiles::ALL
        .iter()
        .filter(|profile| !recorded.contains(&profile.op_hash()))
        .collect();
    let already: Vec<String> = profiles::ALL
        .iter()
        .filter(|profile| recorded.contains(&profile.op_hash()))
        .map(|profile| profile.name.to_owned())
        .collect();

    // Whether the row is still missing something a full-fidelity probe could supply. Timed media needs a
    // duration as well as dimensions; a still does not have one to miss.
    let timed = mime.starts_with("video/") || mime.starts_with("audio/");
    let unmeasured = width.is_none() || height.is_none() || (timed && duration_ms.is_none());

    // Nothing to render *and* nothing to measure is the only case that returns without reading the original.
    // Getting this wrong is how the first version of the measurement missed every asset that already had its
    // derivatives — which is to say every asset in an existing library, the one population a backfill is for.
    if outstanding.is_empty() && !unmeasured {
        return Ok(Derived {
            asset_id,
            rendered: Vec::new(),
            already,
            refused: Vec::new(),
        });
    }

    // One read of the original, for every profile — see the module docs.
    let original = Key::original(tenant_id, &content_hash)?;
    let source = store.get(&original, None).await?.into_bytes(&original)?;

    // Measured from these bytes before anything is rendered, because rendering may refuse the file and the
    // measurement is worth having either way — a video has dimensions and a duration whether or not this
    // build can make a poster frame from it.
    // The duration matters twice: the row wants it, and the poster frame below seeks by it. Measured first so
    // a video uploaded a moment ago has a real duration to seek into rather than a fallback.
    let mut duration_ms = duration_ms;
    if unmeasured {
        match measure_and_fill(global, slug, asset_id, &mime, &source).await {
            Ok(measured) => duration_ms = duration_ms.or(measured.duration_ms),
            // Not fatal. A missing dimension is a worse asset, not a failed upload, and the reason is logged
            // rather than swallowed — which is exactly what the old `.ok()` did wrong.
            Err(error) => {
                tracing::warn!(%error, %asset_id, %mime, "could not measure the original")
            }
        }
    }

    // A video is rendered from a *frame* of itself (3.5's visible half). The image profiles cannot decode a
    // container, so before this a clip had no derivative at all and the grid showed "processing" forever — a
    // library of videos as a wall of grey rectangles. Extracting one frame turns the whole existing rendition
    // path, and the delivery path behind it, into video thumbnails for free.
    //
    // The full proxy — H.264, loudness, HLS — is still M3.5. This is the poster, which is what a person
    // actually notices.
    let renderable: Cow<'_, [u8]> = if timed {
        match poster_of(&mime, duration_ms, &source).await {
            Ok(frame) => Cow::Owned(frame),
            Err(error) => {
                // Not fatal, and named: a clip whose frame cannot be extracted is a clip with no thumbnail,
                // which is what it already had.
                tracing::warn!(%error, %asset_id, %mime, "could not extract a poster frame");
                Cow::Borrowed(&source)
            }
        }
    } else {
        Cow::Borrowed(&source)
    };

    let mut rendered = Vec::new();
    let mut refused = Vec::new();

    for profile in outstanding {
        let op_hash = profile.op_hash();
        let recipe = Recipe {
            rendition: &profile.rendition,
            color_profile: profile.color_profile,
            op_hash: &op_hash,
            source_mime: &mime,
            source_title: &filename,
        };
        match render_one(
            store,
            tenant_id,
            &content_hash,
            &renderable,
            &recipe,
            identity,
        )
        .await
        {
            Ok(output) => {
                // `record_on`, not `record`: this has to run on the tenant-scoped connection, or the
                // unqualified `derivatives` resolves against whatever schema the pooled connection last had.
                let mut conn = TenantConn::begin(global, slug).await?;
                let recorded = dam_db::derivatives::record_on(
                    conn.executor(),
                    &dam_db::derivatives::NewDerivative {
                        asset_id,
                        role: profile.role,
                        profile: profile.name,
                        op_hash: &profile.op_hash(),
                        object_key: output.key.as_str(),
                        mime: output.mime,
                        bytes: i64::try_from(output.bytes.len()).unwrap_or(i64::MAX),
                        width: dimension(profile.rendition.width),
                        height: dimension(profile.rendition.height),
                        regen_cost_ms: output.cost_ms,
                    },
                )
                .await;
                conn.commit().await?;

                match recorded {
                    Ok(derivative) => {
                        // The manifest relationally as well as embedded. `provenance_manifests` is what makes
                        // "what did damrs do to this file" a query rather than an exercise in parsing every
                        // derivative, and what allows a manifest to be regenerated after a certificate
                        // rotation. Chained to the asset's inbound manifest where there is one, so the row
                        // records the same lineage the signed bytes do.
                        if let Some(manifest) = &output.manifest
                            && let Err(error) = record_derivative_manifest(
                                global,
                                slug,
                                asset_id,
                                derivative.id,
                                output.key.as_str(),
                                manifest,
                                profile.name,
                            )
                            .await
                        {
                            // The credential is in the bytes either way; only the index is missing.
                            tracing::warn!(%error, %asset_id, "could not record a derivative manifest");
                        }
                        rendered.push(profile.name.to_owned());
                    }
                    // An asset already has exactly one master proxy (D5), so a redefined `web-2048` refuses
                    // here and names `replace_proxy`. Reported rather than failing the job: the thumbnail and
                    // the preview still rendered, and a redefinition is an operator action rather than an
                    // ingest failure.
                    Err(dam_db::Error::Unsupported(reason)) => {
                        refused.push((profile.name.to_owned(), reason));
                    }
                    Err(other) => return Err(other.into()),
                }
            }
            // A format nothing can read is reported, not failed — see `Derived::refused`.
            Err(Refusal::Unreadable(reason)) => refused.push((profile.name.to_owned(), reason)),
            // A missing tool or a broken render is the job's problem. Transient, because both are things a
            // differently configured or less unlucky worker succeeds at, and the attempt counter is what
            // stops an infinite loop.
            Err(other @ (Refusal::ToolMissing(_) | Refusal::Failed(_))) => {
                return Err(Error::Transient(format!(
                    "rendering {} for asset {asset_id} ({mime}): {}",
                    profile.name,
                    other.reason()
                )));
            }
        }
    }

    Ok(Derived {
        asset_id,
        rendered,
        already,
        refused,
    })
}

/// What rendering one tenant conversion produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionRendered {
    pub asset_id: Uuid,
    /// The conversion's key, which is also `derivatives.profile`.
    pub key: String,
    /// False when the recipe was already recorded, which is the ordinary outcome of two people asking for the
    /// same format at once. Not an error: the queue is at-least-once and the cache key is content-addressed.
    pub rendered: bool,
}

/// Renders one tenant-defined conversion for one asset (Q.11c).
///
/// Separate from [`asset`] because the two answer different questions. That one renders *everything the system
/// needs* for a new upload, in a fixed set, from one read of the original. This renders *one format somebody
/// asked for*, on demand, and its failure modes are the caller's problem rather than a placeholder in a grid:
/// somebody is waiting for a download.
///
/// Shares the renderer, the vips fallback and the cache key derivation with [`asset`] — see [`Recipe`] on why
/// that mattered enough to change a signature for.
pub async fn conversion(
    global: &sqlx::PgPool,
    store: &dyn BlobStore,
    slug: &dam_core::TenantSlug,
    tenant_id: Uuid,
    asset_id: Uuid,
    key: &str,
    // As in [`asset`]: a tenant-defined conversion signs the same way a built-in profile does, or a download
    // in a named format would be the one thing that leaves the system without credentials.
    identity: Option<&dam_media::provenance::SigningIdentity>,
) -> Result<ConversionRendered> {
    let mut conn = TenantConn::begin(global, slug).await?;
    let found = dam_db::conversions::by_key(conn.executor(), key).await?;
    let asset = sqlx::query_as::<_, (String, String, String)>(
        "SELECT content_hash, mime, filename FROM assets WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(asset_id)
    .fetch_optional(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;
    conn.commit().await?;

    // Withdrawn conversions render. The key is what a delivery token carries, so a link issued while a format
    // was offered must still resolve — and refusing here would make withdrawing a format retroactively break
    // links that were valid when they were sent.
    let Some(conversion) = found else {
        return Err(Error::Permanent(format!(
            "no conversion named {key} in this tenant"
        )));
    };
    let Some((content_hash, mime, filename)) = asset else {
        // Deleted between the request and the job being claimed. Ordinary, and permanent.
        return Err(Error::Permanent(format!(
            "asset {asset_id} does not exist or was deleted"
        )));
    };

    // The class is checked here as well as where the format was offered. A job payload is not a promise: it
    // could have been enqueued before somebody redefined the conversion, and rendering an image recipe over a
    // PDF would either fail obscurely or produce something nobody asked for.
    let class = dam_db::conversions::class_of(&mime);
    if class != conversion.media_class {
        return Err(Error::Permanent(format!(
            "conversion {key} applies to {} and asset {asset_id} is {class}",
            conversion.media_class
        )));
    }

    let Some(rendition) = conversion.rendition() else {
        // A recipe this build cannot render — a newer migration widened the vocabulary and this binary is the
        // older half of a rolling deploy. Permanent for *this* worker; a newer one will take the retry.
        return Err(Error::Permanent(format!(
            "conversion {key} names a format or fit this build cannot render ({}, {})",
            conversion.format, conversion.fit
        )));
    };
    let op_hash = dam_media::profiles::tenant_op_hash(&rendition);

    // Already there is the ordinary outcome of two people choosing the same format at once, and of a redelivery
    // of a link. Checked before the original is fetched, because that fetch is the expensive part.
    let mut conn = TenantConn::begin(global, slug).await?;
    let existing = dam_db::derivatives::by_op_hash(conn.executor(), asset_id, &op_hash).await?;
    conn.commit().await?;
    if existing.is_some() {
        return Ok(ConversionRendered {
            asset_id,
            key: conversion.key,
            rendered: false,
        });
    }

    let original = Key::original(tenant_id, &content_hash)?;
    let source = store.get(&original, None).await?.into_bytes(&original)?;

    let recipe = Recipe {
        rendition: &rendition,
        // Fixed, not from the row: `derive::render` does not apply a colour profile, so letting a tenant set
        // one would change the cache key without changing the output. See `profiles::TENANT_COLOR_PROFILE`.
        color_profile: dam_media::profiles::TENANT_COLOR_PROFILE,
        op_hash: &op_hash,
        source_mime: &mime,
        source_title: &filename,
    };
    let output = match render_one(store, tenant_id, &content_hash, &source, &recipe, identity).await
    {
        Ok(output) => output,
        // Unlike [`asset`], an unreadable source *is* this job's failure. There is no placeholder to fall back
        // to: somebody chose a format for a file that cannot be rendered into it, and the honest outcome is a
        // failed job with the reason rather than a download that never appears.
        Err(refusal) => {
            return Err(match refusal {
                Refusal::Unreadable(reason) => Error::Permanent(format!(
                    "asset {asset_id} ({mime}) cannot be rendered as {key}: {reason}"
                )),
                other => Error::Transient(format!(
                    "rendering {key} for asset {asset_id} ({mime}): {}",
                    other.reason()
                )),
            });
        }
    };

    let mut conn = TenantConn::begin(global, slug).await?;
    dam_db::derivatives::record_on(
        conn.executor(),
        &dam_db::derivatives::NewDerivative {
            asset_id,
            // `rendition`, which is the role 0001 reserved for exactly this and which nothing had ever
            // written. It tiers, unlike a thumbnail or a proxy: a print-sized export is large and rarely
            // fetched twice, which is the case §6.4's minimum billable size argument does *not* cover.
            role: "rendition",
            profile: &conversion.key,
            op_hash: &op_hash,
            object_key: output.key.as_str(),
            mime: output.mime,
            bytes: i64::try_from(output.bytes.len()).unwrap_or(i64::MAX),
            width: dimension(rendition.width),
            height: dimension(rendition.height),
            regen_cost_ms: output.cost_ms,
        },
    )
    .await?;
    conn.commit().await?;

    Ok(ConversionRendered {
        asset_id,
        key: conversion.key,
        rendered: true,
    })
}

/// Why one profile did not render.
enum Refusal {
    /// No renderer can read this source. Not a failure: see [`Derived::refused`].
    Unreadable(String),
    /// libvips is not installed on this worker, so the formats behind it cannot be rendered here.
    ///
    /// Its own variant because it fails the job rather than being reported: a worker missing a tool is a
    /// deployment mistake, and a queue that swallowed it would leave a library where some assets have
    /// thumbnails and nobody can say why.
    ToolMissing(String),
    /// Something else went wrong — writing the object, a temp file, a render that started and failed.
    Failed(String),
}

impl Refusal {
    fn reason(&self) -> &str {
        match self {
            Self::Unreadable(reason) | Self::ToolMissing(reason) | Self::Failed(reason) => reason,
        }
    }
}

/// One rendered object.
struct Output {
    key: Key,
    mime: &'static str,
    bytes: Vec<u8>,
    /// The manifest embedded in `bytes`, when one was signed.
    ///
    /// Carried out so the caller can record it relationally as well: `provenance_manifests` is what makes
    /// "what did damrs do to this file" a query, and what allows a manifest to be regenerated if a signing
    /// certificate is rotated.
    manifest: Option<Vec<u8>>,
    cost_ms: Option<i32>,
}

/// Records a derivative's signed manifest, chained to the asset's inbound one.
///
/// The parent link is looked up rather than passed down because the *asset's* inbound manifest is the thing a
/// derivative chains to, and the render loop has no reason to hold it. Absent when the original arrived
/// without credentials, which is the common case — a derivative of an uncredentialed original still gets its
/// own manifest saying what damrs did, it just does not continue anybody else's chain.
async fn record_derivative_manifest(
    global: &sqlx::PgPool,
    slug: &dam_core::TenantSlug,
    asset_id: Uuid,
    derivative_id: Uuid,
    object_key: &str,
    manifest: &[u8],
    profile: &str,
) -> Result<()> {
    let mut conn = TenantConn::begin(global, slug).await?;
    let parent = dam_db::provenance::for_asset(conn.executor(), asset_id)
        .await?
        .into_iter()
        .find(|stored| stored.role == dam_db::provenance::Role::Inbound)
        .map(|stored| stored.id);

    dam_db::provenance::record_signed(
        conn.executor(),
        derivative_id,
        parent,
        &dam_db::provenance::NewManifest {
            object_key,
            bytes: i64::try_from(manifest.len()).unwrap_or(i64::MAX),
            // Ours, freshly signed with our own certificate — so the state is whatever a verifier would say
            // about that certificate. `untrusted` until the certificate chains to a root a verifier knows,
            // which is a property of the deployment's PKI rather than of this manifest.
            validation_state: "untrusted",
            validation_detail: serde_json::json!({ "profile": profile }),
            signer_cn: None,
            claim_generator: Some(&dam_media::provenance::claim_generator()),
            spec_version: None,
            captured_at: None,
            actions: vec!["c2pa.resized".to_owned(), "c2pa.converted".to_owned()],
        },
    )
    .await?;
    conn.commit().await?;
    Ok(())
}

/// Signs one rendered derivative, chaining it to the original.
///
/// The action list is built from the recipe rather than being generic: `c2pa.resized` without a size records
/// that something happened without recording what, which is not provenance. A format change adds
/// `c2pa.converted` — two actions where two things happened.
///
/// `Provenance::DerivedFrom` rather than `Created`, which is the whole point. `Created` would claim this file
/// came into existence here, discarding whatever the original said about a camera or a generative model — and
/// for an AI-generated original that would silently drop the Article 50 marking (D15) the derivative is
/// obliged to carry.
fn sign_derivative(
    identity: Option<&dam_media::provenance::SigningIdentity>,
    source: &[u8],
    rendered: &[u8],
    recipe: &Recipe<'_>,
) -> std::result::Result<(Vec<u8>, Option<Vec<u8>>), String> {
    let Some(identity) = identity else {
        return Err("no signing identity is configured".to_owned());
    };

    let mut actions = vec![dam_media::provenance::Action::resized(
        recipe.rendition.width,
        recipe.rendition.height,
    )];
    actions.push(dam_media::provenance::Action::converted(
        recipe.rendition.format.extension(),
    ));

    let signed = dam_media::provenance::sign(
        identity,
        rendered,
        recipe.rendition.format.mime(),
        dam_media::provenance::Claim {
            claim_generator: dam_media::provenance::claim_generator(),
            provenance: dam_media::provenance::Provenance::DerivedFrom(
                dam_media::provenance::Parent {
                    // The original's bytes, which the caller already holds for rendering — so the ingredient
                    // costs nothing extra here. Reading them again to sign would double the transfer on every
                    // derivative of every asset.
                    bytes: source.to_vec(),
                    format: recipe.source_mime.to_owned(),
                    title: recipe.source_title.to_owned(),
                },
            ),
            actions,
        },
    )
    .map_err(|error| error.to_string())?;

    Ok((signed.bytes, signed.manifest))
}

/// One thing to render: the recipe, the colour treatment it is hashed under, and the cache key.
///
/// Takes the place of a `&Profile` so a tenant-defined conversion (Q.11) renders through *this* function rather
/// than a parallel one — including the libvips fallback, which is the half a second implementation would forget.
/// A `Profile` has a `&'static str` name and a per-profile revision that a database row cannot have, so the
/// shared shape is the three things the renderer actually reads.
struct Recipe<'a> {
    rendition: &'a Rendition,
    color_profile: &'a str,
    op_hash: &'a str,
    /// The original's media type and filename, for the C2PA ingredient.
    ///
    /// On the recipe rather than passed alongside it, so a tenant-defined conversion (Q.11) rendering through
    /// this same function cannot forget them and produce a manifest whose ingredient claims the wrong format.
    source_mime: &'a str,
    source_title: &'a str,
}

/// Renders one recipe and stores it.
///
/// The error is a `String` rather than a `crate::Error` because every failure here is *per profile* and the
/// caller collects them: a PDF that has no `web-2048` must not stop the thumbnail from being recorded.
async fn render_one(
    store: &dyn BlobStore,
    tenant_id: Uuid,
    content_hash: &str,
    source: &[u8],
    recipe: &Recipe<'_>,
    identity: Option<&dam_media::provenance::SigningIdentity>,
) -> std::result::Result<Output, Refusal> {
    let started = std::time::Instant::now();

    let bytes = match dam_media::derive::render(source, recipe.rendition) {
        Ok(bytes) => bytes,
        Err(pure_rust) => {
            // The libvips fallback. Reached for camera RAW, PSD, PDF and HEIF — the formats §18.2 puts behind
            // it — and it runs inside `dam_media::sandbox`, because those are the loaders libvips itself marks
            // untrusted.
            match render_via_vips(source, recipe).await {
                Ok(bytes) => bytes,
                Err(VipsFailure::NotInstalled(reason)) => {
                    return Err(Refusal::ToolMissing(format!(
                        "pure-Rust: {pure_rust}; {reason}"
                    )));
                }
                Err(VipsFailure::Unreadable(reason)) => {
                    return Err(Refusal::Unreadable(format!(
                        "no renderer can read this: pure-Rust: {pure_rust}; libvips: {reason}"
                    )));
                }
                Err(VipsFailure::Failed(reason)) => {
                    return Err(Refusal::Failed(format!(
                        "pure-Rust: {pure_rust}; libvips: {reason}"
                    )));
                }
            }
        }
    };

    // Content credentials, embedded before the bytes are stored (D13, G1).
    //
    // This is the half of G1 that makes the pipeline stop *destroying* provenance. libvips, ffmpeg and pdfium
    // all strip embedded metadata by default, so a rendered derivative arrives here with no manifest whatever
    // the original had — reliably, not occasionally. Signing puts one back, chained to the original as a
    // `parentOf` ingredient so the result is a continuation of the chain rather than a new claim about a file
    // that appeared from nowhere.
    //
    // Embedded rather than detached, unlike the inbound manifest: a derivative is the thing that leaves —
    // downloaded, embedded in a page, handed to a connector — and a credential that stayed behind in our
    // database would be provenance nobody downstream can check. The inbound manifest is stored separately for
    // the opposite reason: originals tier to Deep Archive, and the credential has to outlive that.
    //
    // Failure to sign is **not** failure to render. A missing signing identity is the ordinary case, and a
    // signing error is an operator problem with a certificate — neither is a reason to have no thumbnail.
    // `provenance_gaps` is the view that reports the difference afterwards.
    let (bytes, manifest) = match sign_derivative(identity, source, &bytes, recipe) {
        Ok(signed) => signed,
        Err(reason) => {
            tracing::warn!(%reason, profile = recipe.op_hash, "derivative signed with no credentials");
            (bytes, None)
        }
    };

    let ext = recipe.rendition.format.extension();
    let key = Key::derivative(tenant_id, content_hash, recipe.op_hash, ext)
        .map_err(|e| Refusal::Failed(format!("deriving the object key: {e}")))?;

    store
        .put(
            &key,
            bytes::Bytes::from(bytes.clone()),
            // Never a colder class. §6.4 does not tier `proxy`, `thumbnail` or `preview`, because the 128 KiB
            // minimum billable size makes tiering a 20 KB thumbnail cost *more* than leaving it in Standard.
            StorageClass::Standard,
        )
        .await
        .map_err(|e| Refusal::Failed(format!("storing the derivative: {e}")))?;

    Ok(Output {
        key,
        mime: recipe.rendition.format.mime(),
        bytes,
        manifest,
        cost_ms: i32::try_from(started.elapsed().as_millis()).ok(),
    })
}

/// The libvips path: source to a temp file, render, read back.
///
/// One frame of a video, as bytes the image renditions can render.
///
/// Separate from the measurement so a clip whose duration is unknown still gets a poster: the seek falls back
/// to one second in, which is inside the shot for anything longer than that and harmless for anything shorter
/// — ffmpeg lands on the last frame rather than failing.
async fn poster_of(mime: &str, duration_ms: Option<i64>, source: &[u8]) -> Result<Vec<u8>> {
    if !mime.starts_with("video/") {
        // Audio has no frame to take. A waveform would be a picture of the file rather than of the recording,
        // and inventing one is worse than leaving the tile blank.
        return Err(Error::Permanent(format!(
            "{mime} has no frame to use as a poster"
        )));
    }
    let tools = dam_media::avprobe::AvToolchain::discover()
        .map_err(|e| Error::Permanent(format!("ffmpeg is not on PATH: {e}")))?;
    let dir = tempfile::tempdir().map_err(|e| Error::Permanent(format!("temp dir: {e}")))?;
    let path = dir.path().join("clip");
    tokio::fs::write(&path, source)
        .await
        .map_err(|e| Error::Permanent(format!("writing the clip: {e}")))?;
    dam_media::video::poster_frame(&tools, &path, duration_ms)
        .await
        .map_err(|e| Error::Permanent(format!("poster frame: {e}")))
}

/// Fills in the dimensions, duration and page count finalisation could not measure.
///
/// Writes the bytes to a temporary file because both tools work on paths — which is the same reason the vips
/// render path does, and the same containment §16 asks for.
///
/// Timed media goes to `ffprobe` and stills to `vipsheader`, decided by the *sniffed* mime rather than by the
/// filename: a `.MP4` that is really QuickTime and a `.PNG` that is really a JPEG both arrived in the first
/// real upload, and trusting the extension would have sent each to the wrong tool.
async fn measure_and_fill(
    global: &sqlx::PgPool,
    slug: &dam_core::TenantSlug,
    asset_id: Uuid,
    mime: &str,
    source: &[u8],
) -> Result<Measured> {
    let dir = tempfile::tempdir().map_err(|e| Error::Permanent(format!("temp dir: {e}")))?;
    let path = dir.path().join("original");
    tokio::fs::write(&path, source)
        .await
        .map_err(|e| Error::Permanent(format!("writing the original: {e}")))?;

    let timed = mime.starts_with("video/") || mime.starts_with("audio/");
    let measured = if timed {
        let tools = dam_media::avprobe::AvToolchain::discover()
            .map_err(|e| Error::Permanent(format!("ffprobe is not on PATH: {e}")))?;
        let probe = dam_media::avprobe::probe(&tools, &path)
            .await
            .map_err(|e| Error::Permanent(format!("ffprobe: {e}")))?;
        Measured {
            // Timed media carries no EXIF orientation to apply; ffprobe reports the stream's own dimensions,
            // which is what a player shows.
            width: probe.width,
            height: probe.height,
            duration_ms: probe.duration_ms,
            page_count: None,
        }
    } else {
        let tools = dam_media::vips::Toolchain::discover()
            .map_err(|e| Error::Permanent(format!("libvips is not on PATH: {e}")))?;
        let probe = dam_media::vips::probe(&tools, &path)
            .await
            .map_err(|e| Error::Permanent(format!("vipsheader: {e}")))?;
        // vips reports *stored* dimensions and rotates at render time, so the axes swap here for exactly the
        // orientations that swap them — the same rule `probe::image` applies to a JPEG.
        let swaps = matches!(probe.orientation, Some(5..=8));
        Measured {
            width: Some(if swaps { probe.height } else { probe.width }),
            height: Some(if swaps { probe.width } else { probe.height }),
            duration_ms: None,
            page_count: probe.page_count,
        }
    };

    let mut conn = TenantConn::begin(global, slug).await?;
    // `coalesce`, so this only ever fills a null. The cheap probe is authoritative where it worked, and a
    // re-run of this job must not change an answer somebody has already seen.
    sqlx::query(
        "UPDATE assets SET \
             width = coalesce(width, $2), \
             height = coalesce(height, $3), \
             duration_ms = coalesce(duration_ms, $4), \
             page_count = coalesce(page_count, $5), \
             updated_at = now() \
         WHERE id = $1",
    )
    .bind(asset_id)
    .bind(measured.width.and_then(|v| i32::try_from(v).ok()))
    .bind(measured.height.and_then(|v| i32::try_from(v).ok()))
    .bind(measured.duration_ms)
    .bind(
        measured
            .page_count
            .and_then(|pages| i32::try_from(pages).ok()),
    )
    .execute(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;
    conn.commit().await?;
    Ok(measured)
}

/// What a full-fidelity probe found. Every field optional: a tool that cannot answer must leave the column
/// alone rather than write a zero.
struct Measured {
    width: Option<u32>,
    height: Option<u32>,
    duration_ms: Option<i64>,
    page_count: Option<usize>,
}

/// vips works on paths rather than buffers, and that is not merely an API detail — it is what lets the render
/// happen in another process, which is the containment §16 asks for.
enum VipsFailure {
    NotInstalled(String),
    /// vips ran and said it does not know the format. That is an answer about the *file*, not about the
    /// installation, and the two must not be conflated — one is permanent and one is a deployment fix.
    Unreadable(String),
    Failed(String),
}

async fn render_via_vips(
    source: &[u8],
    recipe: &Recipe<'_>,
) -> std::result::Result<Vec<u8>, VipsFailure> {
    let tools = dam_media::vips::Toolchain::discover().map_err(|e| {
        // Named explicitly. A worker without vips renders a subset of formats, and "no thumbnail" with no
        // explanation is the version of this that wastes somebody's afternoon.
        VipsFailure::NotInstalled(format!(
            "libvips is not on PATH, so this format cannot be rendered on this worker: {e}"
        ))
    })?;

    let dir = tempfile::tempdir().map_err(|e| VipsFailure::Failed(format!("temp dir: {e}")))?;
    let input = dir.path().join("source");
    let output = dir
        .path()
        .join(format!("out.{}", recipe.rendition.format.extension()));
    tokio::fs::write(&input, source)
        .await
        .map_err(|e| VipsFailure::Failed(format!("writing the source: {e}")))?;

    dam_media::vips::render(
        &tools,
        &input,
        &output,
        &dam_media::vips::RenderSpec {
            width: recipe.rendition.width,
            height: recipe.rendition.height,
            format: recipe.rendition.format,
            quality: recipe.rendition.quality,
            fit: recipe.rendition.fit,
            // Converted at delivery, which is D11's rule: the master keeps its own profile, and a derivative
            // for the web is sRGB.
            output_profile: Some(recipe.color_profile.to_owned()),
            intent: dam_media::vips::Intent::Perceptual,
        },
    )
    .await
    .map_err(|e| {
        let message = e.to_string();
        // vips's own words. "is not a known file format" and "unable to thumbnail" are how it says the input
        // is not something it can decode, which is a fact about the file rather than about this worker.
        if message.contains("not a known file format") || message.contains("unable to thumbnail") {
            VipsFailure::Unreadable(message)
        } else {
            VipsFailure::Failed(message)
        }
    })?;

    tokio::fs::read(&output)
        .await
        .map_err(|e| VipsFailure::Failed(format!("reading the rendition: {e}")))
}

fn dimension(value: u32) -> Option<i32> {
    i32::try_from(value).ok()
}
