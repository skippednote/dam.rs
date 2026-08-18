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
//! ## An internal preview goes through the same chokepoint and is not a distribution
//!
//! `Purpose::InternalPreview` (A.7) skips the *rights verdict* and nothing else. It is still a signed token,
//! still verified here, still access-checked, and still the only path to the bytes — so D12's "one code
//! path" holds. What changes is that the chokepoint distinguishes handing an asset to the outside world from
//! showing a member of the tenant a 256-pixel thumbnail of it in their own library.
//!
//! Why that is not a hole is in [`signed_url::Purpose`]'s docs, and the three restrictions are enforced
//! **twice**: once where the token is minted and once here. Checking only at issue would mean a token minted
//! before a restriction tightened kept working, which is the same mistake as letting the signature authorise.
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
use dam_core::signed_url::{self, DeliveryClaim, Keyring, Purpose};
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
    /// A pool whose `search_path` resolves the **delivery tenant's** schema.
    ///
    /// Not the shared global pool, and the distinction is not cosmetic: almost everything this handler reads —
    /// `assets`, `derivatives`, the rights tables, `share_links` — is tenant-schema, written unqualified, and
    /// resolved through `search_path`. Handed the global pool it fails with `relation "derivatives" does not
    /// exist`, which is what happened the first time a real derivative existed to serve. The delivery route
    /// serves exactly one tenant by construction (`damd` refuses to start otherwise), so a pool pinned to that
    /// tenant is the right shape rather than a compromise — see `dam_db::tenant_conn::single_tenant_pool`.
    ///
    /// The `dam_global.` reads in here are schema-qualified and work through either pool.
    global: PgPool,
    store: Arc<dyn BlobStore>,
    keyring: Keyring,
    /// The tenant whose prefix originals live under.
    ///
    /// A `Uuid` rather than a rendered prefix string, because `Key::original` builds the path — a caller
    /// passing a prefix would be one concatenation away from naming another tenant's.
    tenant_id: Uuid,
    /// The origin to build absolute delivery URLs from, when configured.
    ///
    /// `None` means root-relative, which is right for a same-origin client and wrong for any other — see
    /// `ServerConfig::public_url`.
    public_url: Option<String>,
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
            public_url: None,
            clock: Arc::new(dam_core::SystemClock),
        }
    }

    /// Sets the origin absolute delivery URLs are built from.
    #[must_use]
    pub fn with_public_url(mut self, public_url: Option<String>) -> Self {
        self.public_url = public_url.map(|url| url.trim_end_matches('/').to_owned());
        self
    }

    /// The URL a client fetches for `token`.
    ///
    /// Absolute when the deployment says what its public origin is, root-relative otherwise. Built here rather
    /// than at each call site so the route and the URL cannot disagree — a hand-written `/d/` somewhere else is
    /// one rename away from a 404.
    pub fn url_for(&self, token: &str) -> String {
        match &self.public_url {
            Some(base) => format!("{base}/d/{token}"),
            None => format!("/d/{token}"),
        }
    }

    /// Signs an internal-preview token, without touching the database.
    ///
    /// Synchronous, and that is the point: a page of sixty assets mints sixty of these, and each is an HMAC
    /// over a few dozen bytes. Going through `issue_preview` would mean sixty async calls that each do no I/O,
    /// and — worse — would tempt a future change to add a query to a per-row path.
    ///
    /// The rights verdict is not consulted here for the reason `signed_url::Purpose` documents. The three
    /// restrictions are: the transform must be proxy-class, an identity is required, and a share link is
    /// refused. They are checked here and again at delivery.
    pub fn sign_preview(
        &self,
        asset_id: Uuid,
        transform: &str,
        identity_id: Uuid,
        ttl: ChronoDuration,
        now: DateTime<Utc>,
    ) -> Result<String, Refusal> {
        if !preview_is_permitted(transform, Some(identity_id), None) {
            tracing::error!(%asset_id, transform, "refused to sign a preview that breaks its restrictions");
            return Err(Refusal::NotDeliverable);
        }
        let ttl = ttl.min(MAX_TOKEN_TTL).max(ChronoDuration::seconds(1));
        let claim = DeliveryClaim {
            purpose: Purpose::InternalPreview,
            asset_id,
            transform: transform.to_owned(),
            // Never evaluated for this purpose; `internal` is the honest label for what it names.
            channel: "internal".to_owned(),
            territory: "WORLD".to_owned(),
            identity_id: Some(identity_id),
            share_link_id: None,
            expires_at: now + ttl,
            key_id: String::new(),
        };
        // The URL, not the token: a bare token is not something a client can fetch, and returning one is how the
        // first version put an unfetchable string in `thumbnail_url`.
        signed_url::sign(&self.keyring, &claim)
            .map(|token| self.url_for(&token))
            .ok_or(Refusal::Internal)
    }

    /// Replaces the clock, for tests that need to move time.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The current instant, from this state's clock.
    ///
    /// Used by the asset endpoints too, so a preview token is minted against the *same* clock this handler
    /// verifies it with. A caller reading `Utc::now()` instead is the bug that made an expiry case assert 404
    /// and pass on a 302 — see the note on `clock`.
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }
}

/// The delivery routes.
pub fn router(state: DeliveryState) -> axum::Router {
    router_from(Arc::new(state))
}

/// The same routes over a state the caller already holds.
///
/// The asset endpoints need the same `DeliveryState` in order to mint preview tokens with the same keyring and
/// the same clock, so it has to be shareable rather than moved in here.
pub fn router_from(state: Arc<DeliveryState>) -> axum::Router {
    axum::Router::new()
        .route("/d/{token}", get(deliver))
        .with_state(state)
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
    issue_with_purpose(
        state,
        Purpose::Distribution,
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

/// Mints a preview URL for the DAM's own interface.
///
/// The rights verdict is not consulted — see [`Purpose`] for why that is a decision rather than an omission —
/// so this must never be reachable from anything that hands a URL outside the tenant. It is called from the
/// asset endpoints, for a caller who has already been access-checked, and it refuses anything but a
/// proxy-class transform.
pub async fn issue_preview(
    state: &DeliveryState,
    asset_id: Uuid,
    transform: &str,
    identity_id: Uuid,
    ttl: ChronoDuration,
    now: DateTime<Utc>,
) -> Result<String, Refusal> {
    issue_with_purpose(
        state,
        Purpose::InternalPreview,
        asset_id,
        transform,
        // A preview is not a channel delivery, and the usage is carried only so the claim has one shape. It
        // is never evaluated for this purpose; `internal` is the honest label for what it names.
        &Usage {
            channel: "internal".to_owned(),
            territory: "WORLD".to_owned(),
        },
        Some(identity_id),
        None,
        ttl,
        now,
    )
    .await
}

/// Whether `transform` may be delivered as an internal preview.
///
/// A **known built-in profile**, and nothing else. That covers both things this has to exclude: `original` is
/// not a profile at all, and a future tenant-defined profile will not be in `profiles::ALL` — so it is refused
/// until somebody decides deliberately whether a tenant's own render is an internal preview, which is the
/// right default because the answer is not obvious.
///
/// An earlier version also required the profile's role to be `thumbnail`, `preview` or `proxy`. That branch was
/// **unfalsifiable**: every built-in profile is proxy-class, so no test could distinguish the role check from
/// its absence, and a mutation removing it survived. An untested branch is one that will be wrong when it
/// finally matters, so it is gone — and when a non-proxy built-in profile exists, the decision about whether it
/// is previewable is a decision to make then, with a test that can see it.
fn previewable(transform: &str) -> bool {
    dam_media::profiles::by_name(transform).is_some()
}

/// All three restrictions on an internal preview, in one predicate.
///
/// One function so the mint side and the serve side cannot check different subsets — which is the way this
/// kind of restriction usually rots: a new field is checked where it is created and not where it is used.
fn preview_is_permitted(
    transform: &str,
    identity_id: Option<Uuid>,
    share_link_id: Option<Uuid>,
) -> bool {
    previewable(transform) && identity_id.is_some() && share_link_id.is_none()
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
    issue_with_purpose(
        state,
        Purpose::Distribution,
        asset_id,
        transform,
        usage,
        identity_id,
        share_link_id,
        ttl,
        now,
    )
    .await
}

/// The one place a token is minted.
#[expect(
    clippy::too_many_arguments,
    reason = "every one of these is signed into the token; a struct would hide that and invite a caller to \
              build a claim with a field left at its default"
)]
async fn issue_with_purpose(
    state: &DeliveryState,
    purpose: Purpose,
    asset_id: Uuid,
    transform: &str,
    usage: &Usage,
    identity_id: Option<Uuid>,
    share_link_id: Option<Uuid>,
    ttl: ChronoDuration,
    now: DateTime<Utc>,
) -> Result<String, Refusal> {
    if purpose.is_distribution() {
        let verdict = rights::effective(&state.global, asset_id, usage, now).await?;
        if !permits(verdict) {
            let codes = reason_codes(&state.global, asset_id, usage, now).await;
            return Err(Refusal::RightsDenied {
                state: verdict,
                codes,
            });
        }
    } else if !preview_is_permitted(transform, identity_id, share_link_id) {
        // `NotDeliverable`, not a distinct variant: the caller asking for this is our own code, and a
        // programming mistake here should look like a dead URL rather than produce a hint that a preview of
        // the original is a thing worth asking for.
        tracing::error!(
            %asset_id, transform, has_identity = identity_id.is_some(), shared = share_link_id.is_some(),
            "refused to mint an internal preview that breaks its own restrictions"
        );
        return Err(Refusal::NotDeliverable);
    }

    // Clamped rather than refused. A caller asking for a year is asking for a share link, and answering
    // with a 24-hour URL is more useful than an error about a constant they cannot see.
    let ttl = ttl.min(MAX_TOKEN_TTL).max(ChronoDuration::seconds(1));
    let claim = DeliveryClaim {
        purpose,
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

    if claim.purpose.is_distribution() {
        // Step 2. Asked afresh, which is what makes a lapsed licence stop an already-issued URL.
        let verdict = rights::effective(&state.global, claim.asset_id, &usage, now).await?;
        if !permits(verdict) {
            let codes = reason_codes(&state.global, claim.asset_id, &usage, now).await;
            return Err(Refusal::RightsDenied {
                state: verdict,
                codes,
            });
        }
    } else if !preview_is_permitted(&claim.transform, claim.identity_id, claim.share_link_id) {
        // Re-checked here and not only where the token was minted. A token minted before a restriction
        // tightened must stop working, which is the same argument as re-evaluating rights: a signature
        // records what was asked for, never that it is still allowed.
        return Err(Refusal::NotDeliverable);
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
