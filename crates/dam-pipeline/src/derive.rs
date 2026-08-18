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
use dam_media::profiles::{self, Profile};
use dam_store::{BlobStore, Key};
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
) -> Result<Derived> {
    let mut conn = TenantConn::begin(global, slug).await?;
    let row = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT content_hash, mime, bytes FROM assets WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(asset_id)
    .fetch_optional(conn.executor())
    .await
    .map_err(dam_db::Error::from)?;
    conn.commit().await?;

    let Some((content_hash, mime, _bytes)) = row else {
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

    if outstanding.is_empty() {
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

    let mut rendered = Vec::new();
    let mut refused = Vec::new();

    for profile in outstanding {
        match render_one(store, tenant_id, &content_hash, &source, profile).await {
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
                    Ok(_) => rendered.push(profile.name.to_owned()),
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
    cost_ms: Option<i32>,
}

/// Renders one profile and stores it.
///
/// The error is a `String` rather than a `crate::Error` because every failure here is *per profile* and the
/// caller collects them: a PDF that has no `web-2048` must not stop the thumbnail from being recorded.
async fn render_one(
    store: &dyn BlobStore,
    tenant_id: Uuid,
    content_hash: &str,
    source: &[u8],
    profile: &Profile,
) -> std::result::Result<Output, Refusal> {
    let started = std::time::Instant::now();

    let bytes = match dam_media::derive::render(source, &profile.rendition) {
        Ok(bytes) => bytes,
        Err(pure_rust) => {
            // The libvips fallback. Reached for camera RAW, PSD, PDF and HEIF — the formats §18.2 puts behind
            // it — and it runs inside `dam_media::sandbox`, because those are the loaders libvips itself marks
            // untrusted.
            match render_via_vips(source, profile).await {
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

    let ext = profile.rendition.format.extension();
    let key = Key::derivative(tenant_id, content_hash, &profile.op_hash(), ext)
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
        mime: profile.rendition.format.mime(),
        bytes,
        cost_ms: i32::try_from(started.elapsed().as_millis()).ok(),
    })
}

/// The libvips path: source to a temp file, render, read back.
///
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
    profile: &Profile,
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
        .join(format!("out.{}", profile.rendition.format.extension()));
    tokio::fs::write(&input, source)
        .await
        .map_err(|e| VipsFailure::Failed(format!("writing the source: {e}")))?;

    dam_media::vips::render(
        &tools,
        &input,
        &output,
        &dam_media::vips::RenderSpec {
            width: profile.rendition.width,
            height: profile.rendition.height,
            format: profile.rendition.format,
            quality: profile.rendition.quality,
            fit: profile.rendition.fit,
            // Converted at delivery, which is D11's rule: the master keeps its own profile, and a derivative
            // for the web is sRGB.
            output_profile: Some(profile.color_profile.to_owned()),
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
