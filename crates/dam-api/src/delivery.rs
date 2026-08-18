//! Signed delivery: the one chokepoint every download passes through (3.1).
//!
//! D12 in one module. `0005_rights.sql` says rights are enforced at the point of distribution rather than
//! recorded and hoped for, and this is that point: there is a single handler, and nothing reaches an
//! object's bytes without going through it.
//!
//! ## The order of the two checks is the design
//!
//! 1. **Verify the signature.** Establishes that we issued this exact request and nobody edited the
//!    transform, channel or territory. Proves nothing about entitlement.
//! 2. **Evaluate rights, now.** Loads the licences and releases and decides afresh.
//!
//! Step 2 is not a repeat of a check made at issue time — it is the check. A URL issued on Monday under a
//! valid licence must stop working on Tuesday when the licence lapses, and the only way that happens is by
//! asking again at delivery. If the signature authorised, every URL ever issued would be an outstanding
//! grant that nothing could withdraw.
//!
//! ## The redirect is deliberately short-lived
//!
//! Bytes are served by the object store, not by this process — a DAM that proxies every download is a DAM
//! sized for its customers' bandwidth. But a presigned S3 URL is itself a bearer credential that outlives
//! the rights check, so [`PRESIGN_TTL`] is seconds rather than minutes: long enough for a browser to follow
//! a redirect, short enough that a captured URL is useless.
//!
//! ## A refusal says why, but only to someone who already has the asset
//!
//! A rights denial carries its reason codes, because a customer who cannot download their own asset needs
//! to know a model release lapsed rather than to file a ticket. A *signature* failure carries nothing: the
//! caller is not known to be entitled to anything, and naming which part of a forgery failed is a hint
//! about how to succeed.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use dam_core::Clock;
use dam_core::rights::RightsState;
use dam_core::rights_eval::Usage;
use dam_core::signed_url::{self, DeliveryClaim, Keyring};
use dam_db::rights;
use dam_store::{BlobStore, Key};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// How long the redirect target stays valid.
///
/// Seconds, not minutes. The redirect hands the caller a credential the rights check can no longer
/// supervise, so the window is sized for a browser following a `302` and nothing else.
pub const PRESIGN_TTL: Duration = Duration::from_secs(30);

/// The longest a delivery token may be valid for.
///
/// A token is re-checked against rights on every use, so a long-lived one is not an outstanding grant —
/// but it *is* a stable URL that can be shared, indexed and cached, and a customer pasting one into a
/// public page should not be handing out a week of access to an asset that may be re-scoped tomorrow.
/// Share links (3.3) are the supported way to publish a URL, and they carry their own revocation.
pub const MAX_TOKEN_TTL: ChronoDuration = ChronoDuration::hours(24);

/// Everything the delivery path reads.
#[derive(Clone)]
pub struct DeliveryState {
    global: PgPool,
    store: Arc<dyn BlobStore>,
    keyring: Keyring,
    /// The tenant whose prefix originals live under.
    ///
    /// A `Uuid` rather than a rendered prefix string, because `Key::original` builds the path — a caller
    /// passing a prefix would be one concatenation away from naming another tenant's.
    tenant_id: Uuid,
    /// Where "now" comes from.
    ///
    /// Injected rather than `Utc::now()` at the point of use, because every interesting property of this
    /// handler is about time: a token expiring, a licence lapsing between issue and delivery. Reading the
    /// wall clock inside the handler makes all of that untestable, and the tests that look like they cover
    /// it silently do not — a fixed fake `now` in the test and a real one in the handler had an "expired"
    /// token still in the future.
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for DeliveryState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Neither the pool nor the keyring: one carries a database password and the other the signing key.
        f.debug_struct("DeliveryState")
            .field("tenant_id", &self.tenant_id)
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

impl DeliveryState {
    pub fn new(
        global: PgPool,
        store: Arc<dyn BlobStore>,
        keyring: Keyring,
        tenant_id: Uuid,
    ) -> Self {
        Self {
            global,
            store,
            keyring,
            tenant_id,
            clock: Arc::new(dam_core::SystemClock),
        }
    }

    /// Replaces the clock, for tests that need to move time.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The current instant, from this state's clock.
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }
}

/// The delivery routes.
pub fn router(state: DeliveryState) -> axum::Router {
    axum::Router::new()
        .route("/d/{token}", get(deliver))
        .with_state(Arc::new(state))
}

/// Why a delivery was refused.
#[derive(Debug)]
pub enum Refusal {
    /// The token is absent, malformed, unsigned, expired, or names an unknown key.
    ///
    /// **One variant for all of them, on purpose.** Distinguishing "bad signature" from "expired" tells a
    /// forger their attempt was otherwise accepted and they need only a fresher timestamp.
    NotDeliverable,
    /// The signature was good and the rights say no. Carries the codes, because the caller has established
    /// they hold a URL we issued.
    RightsDenied {
        state: RightsState,
        codes: Vec<String>,
    },
    Internal,
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        match self {
            // 404, not 403. A token that does not verify tells us nothing about whether the asset exists,
            // and answering 403 would confirm that it does.
            Self::NotDeliverable => StatusCode::NOT_FOUND.into_response(),
            Self::RightsDenied { state, codes } => (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({
                    "error": "rights_denied",
                    "rights_state": state.as_str(),
                    "reasons": codes,
                })),
            )
                .into_response(),
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

impl From<dam_db::Error> for Refusal {
    fn from(error: dam_db::Error) -> Self {
        tracing::error!(%error, "delivery database error");
        Self::Internal
    }
}

/// Mints a delivery URL for `asset_id`.
///
/// Rights are checked here too, even though delivery checks again. Not redundancy: issuing a URL that is
/// already refused wastes the caller's round trip and, worse, produces a link that looks valid in an email
/// and fails when somebody clicks it. Failing at issue time puts the error in front of the person who can
/// act on it.
///
/// When 3.2 hydrates search results into delivery URLs it must pass the *same* access predicate that
/// filtered the search, rather than re-deriving one — §12's argument applied to delivery, and the reason
/// this takes the usage explicitly instead of inferring it.
pub async fn issue(
    state: &DeliveryState,
    asset_id: Uuid,
    transform: &str,
    usage: &Usage,
    identity_id: Option<Uuid>,
    ttl: ChronoDuration,
    now: DateTime<Utc>,
) -> Result<String, Refusal> {
    issue_for_share(
        state,
        asset_id,
        transform,
        usage,
        identity_id,
        None,
        ttl,
        now,
    )
    .await
}

/// Mints a delivery URL on behalf of a share link.
///
/// The share's id goes **into the signature**, and delivery re-checks the share on every use. That is what
/// makes revoking a share take effect on the URLs it has already issued — without it, revocation would leave
/// every outstanding delivery token working for its own TTL, and "revoke" would mean "revoke, eventually".
#[expect(
    clippy::too_many_arguments,
    reason = "every one of these is signed into the token; a struct would hide that and invite a caller to \
              build a claim with a field left at its default"
)]
pub async fn issue_for_share(
    state: &DeliveryState,
    asset_id: Uuid,
    transform: &str,
    usage: &Usage,
    identity_id: Option<Uuid>,
    share_link_id: Option<Uuid>,
    ttl: ChronoDuration,
    now: DateTime<Utc>,
) -> Result<String, Refusal> {
    let verdict = rights::effective(&state.global, asset_id, usage, now).await?;
    if !permits(verdict) {
        let codes = reason_codes(&state.global, asset_id, usage, now).await;
        return Err(Refusal::RightsDenied {
            state: verdict,
            codes,
        });
    }

    // Clamped rather than refused. A caller asking for a year is asking for a share link, and answering
    // with a 24-hour URL is more useful than an error about a constant they cannot see.
    let ttl = ttl.min(MAX_TOKEN_TTL).max(ChronoDuration::seconds(1));
    let claim = DeliveryClaim {
        asset_id,
        transform: transform.to_owned(),
        channel: usage.channel.clone(),
        territory: usage.territory.clone(),
        identity_id,
        share_link_id,
        expires_at: now + ttl,
        // Replaced by the keyring; see `signed_url::sign`.
        key_id: String::new(),
    };
    signed_url::sign(&state.keyring, &claim).ok_or(Refusal::Internal)
}

/// Serves a signed delivery URL.
async fn deliver(
    State(state): State<Arc<DeliveryState>>,
    Path(token): Path<String>,
) -> Result<Response, Refusal> {
    let now = state.now();

    // Step 1. Establishes that we issued this request unaltered. Every failure mode collapses to one
    // answer — see `Refusal::NotDeliverable`.
    let claim = signed_url::verify(&state.keyring, &token, now).map_err(|reason| {
        tracing::debug!(?reason, "delivery token rejected");
        Refusal::NotDeliverable
    })?;

    // Re-checked before anything else about the asset. A revoked share must stop working immediately, and it
    // must stop working *for the same reason* whether the URL was minted a second or a day ago.
    if let Some(share_id) = claim.share_link_id
        && !dam_db::shares::is_live(&state.global, share_id, now).await?
    {
        // The same flat 404 an unsigned token gets. A revoked share is no longer a thing this URL names, and
        // saying "revoked" here would confirm the asset exists to whoever now holds the link.
        return Err(Refusal::NotDeliverable);
    }

    let usage = Usage {
        channel: claim.channel.clone(),
        territory: claim.territory.clone(),
    };

    // Step 2. Asked afresh, which is what makes a lapsed licence stop an already-issued URL.
    let verdict = rights::effective(&state.global, claim.asset_id, &usage, now).await?;
    if !permits(verdict) {
        let codes = reason_codes(&state.global, claim.asset_id, &usage, now).await;
        return Err(Refusal::RightsDenied {
            state: verdict,
            codes,
        });
    }

    let key = object_key(&state, &claim, now).await?;
    let url = state
        .store
        .presign_get(&key, PRESIGN_TTL)
        .await
        .map_err(|error| {
            tracing::error!(%error, "presigning a delivery URL");
            Refusal::Internal
        })?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        url.parse().map_err(|_| Refusal::Internal)?,
    );
    // Never cached by a shared cache. The URL embeds a credential and the verdict behind it can change at
    // any moment, so a proxy holding this redirect would serve access after it was withdrawn.
    headers.insert(
        header::CACHE_CONTROL,
        "private, no-store".parse().map_err(|_| Refusal::Internal)?,
    );
    Ok((StatusCode::FOUND, headers).into_response())
}

/// Whether a verdict permits delivery.
///
/// `Expiring` does. It is a warning with a deadline, not a refusal — see 2.8.
fn permits(verdict: RightsState) -> bool {
    matches!(verdict, RightsState::Allowed | RightsState::Expiring)
}

/// The codes explaining a denial, best-effort.
///
/// Best-effort because a failure to *explain* a refusal must not turn it into a different status. The
/// refusal already stands; an empty reason list is worse than a 500.
async fn reason_codes(
    global: &PgPool,
    asset_id: Uuid,
    usage: &Usage,
    now: DateTime<Utc>,
) -> Vec<String> {
    match rights::cached(global, asset_id, usage, now).await {
        Ok(Some(hit)) => hit
            .reasons
            .as_array()
            .map(|reasons| {
                reasons
                    .iter()
                    .filter_map(|r| r["code"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Resolves the object a claim points at.
///
/// The transform is looked up against `derivatives` rather than trusted as a path. A transform that reached
/// the key builder directly would be a path-traversal parameter signed by us — the signature would make it
/// *harder* to notice, not safer.
async fn object_key(
    state: &DeliveryState,
    claim: &DeliveryClaim,
    now: DateTime<Utc>,
) -> Result<Key, Refusal> {
    if claim.transform == "original" {
        // Derived from the content hash rather than read from a column. Assets are content-addressed (D1),
        // so there is no stored path — and deriving it means a delivery URL cannot name an object the
        // asset's own hash does not account for.
        let content_hash: Option<String> = sqlx::query_scalar(
            "SELECT content_hash FROM assets WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(claim.asset_id)
        .fetch_optional(&state.global)
        .await
        .map_err(dam_db::Error::from)?;
        // A deleted asset is not deliverable, and it answers the same way an unsigned token does: the
        // caller learns nothing about whether it ever existed.
        let content_hash = content_hash.ok_or(Refusal::NotDeliverable)?;
        return Key::original(state.tenant_id, &content_hash).map_err(|error| {
            tracing::error!(%error, "an asset's content_hash does not form a valid key");
            Refusal::Internal
        });
    }

    // Resolved name -> profile -> op_hash -> row, never name -> row.
    //
    // This shipped as `WHERE profile = $2` and that was a bug. `op_hash` covers the size, format, quality,
    // fit, background, colour profile and rendering intent (§18.1), so a profile that has been *redefined*
    // has a different hash — and a name lookup would keep serving the bytes rendered under the old
    // definition forever, with no error anywhere and a customer seeing yesterday's quality setting
    // indefinitely.
    let profile = dam_media::profiles::by_name(&claim.transform)
        // An unknown profile is not deliverable rather than approximated: rendering something plausible
        // would silently hand back a different size than the caller integrated against.
        .ok_or(Refusal::NotDeliverable)?;

    let derivative =
        dam_db::derivatives::by_op_hash(&state.global, claim.asset_id, &profile.op_hash())
            .await?
            // A miss is not a failure — it means this recipe has not been rendered yet. Returning
            // `NotDeliverable` is the honest answer until 3.2's render-on-demand path exists; what it must
            // never do is fall back to a name match, which is the bug above.
            .ok_or(Refusal::NotDeliverable)?;

    // Coarse by design — see `derivatives::SERVED_RESOLUTION`. Failing to record a serve must not fail the
    // delivery: the bytes are authorised and the timestamp is a lifecycle hint.
    if let Err(error) = dam_db::derivatives::mark_served(&state.global, derivative.id, now).await {
        tracing::warn!(%error, "recording a derivative serve");
    }

    Key::new(derivative.object_key).map_err(|error| {
        tracing::error!(%error, "a derivative's stored object_key is not a valid key");
        Refusal::Internal
    })
}
