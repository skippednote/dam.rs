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
    /// What connector-signed URLs need (M3d·2), or `None` if they are not honoured here.
    ///
    /// `None` is the fail-closed default: a deployment that cannot open a connector's secret must refuse its
    /// tokens rather than fall back to the server keyring, where they would verify as nothing and be served as
    /// everything.
    connectors: Option<ConnectorAuth>,
}

/// The two things a connector-signed token needs, together.
///
/// Both or neither, structurally. The secret is sealed against `{tenant}:connector:{id}`, so a keyring paired
/// with the wrong slug opens nothing — and the failure would look like every connector URL being forged rather
/// than like a misconfiguration.
#[derive(Clone)]
struct ConnectorAuth {
    sealing: dam_core::sealed::SealingKeyring,
    tenant_slug: dam_core::TenantSlug,
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
            connectors: None,
        }
    }

    /// Enables connector-signed URLs, with the keyring that opens their secrets.
    ///
    /// Takes the slug as well as the keyring because the secret is sealed against it — see [`ConnectorAuth`].
    /// Without this, a token naming a connector key is refused: falling back to the server keyring for an
    /// unopenable connector secret would turn a configuration problem into a bypass.
    #[must_use]
    pub fn with_connector_auth(
        mut self,
        sealing: dam_core::sealed::SealingKeyring,
        tenant_slug: dam_core::TenantSlug,
    ) -> Self {
        self.connectors = Some(ConnectorAuth {
            sealing,
            tenant_slug,
        });
        self
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

    /// The deployment's public origin, when it has one.
    ///
    /// For a caller building a URL that is *not* a delivery URL — a portal's own address, say. Kept here because
    /// this is where the origin already lives, and a second copy in another state would be the pair that
    /// disagrees after a config change.
    pub fn public_origin(&self) -> Option<&str> {
        self.public_url.as_deref()
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
            tenant_id: self.tenant_id,
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

    /// The pool this state reads through — pinned to the delivery tenant's schema.
    ///
    /// Exposed for the share portal, which serves the same single tenant for the same reason delivery does:
    /// a share token arrives with no tenant attached. Goes away with 3.x, alongside `tenant_id`.
    pub fn pool(&self) -> &PgPool {
        &self.global
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
    /// The bytes are in an archive class and cannot be fetched yet (§6.5).
    ///
    /// A `202`, not a `404` and not a `503`. The asset exists, the caller is entitled to it, and the request
    /// was accepted — what is missing is only time. A `404` would be a lie about existence and a `503` would
    /// invite a client to retry in a second, which for Deep Archive on Bulk is forty-eight hours of retrying.
    Restoring(RestoringBody),
    Internal,
}

/// What a caller is told about an archived object.
///
/// Carries the state of any restore already in flight, so two people clicking the same archived asset are
/// told the same ETA rather than each being invited to start their own retrieval — the coalescing in
/// `restores::request` makes that safe, but a response that did not mention it would still have them each
/// pressing the button.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RestoringBody {
    /// `archive` when nothing has been asked for yet; otherwise the request's own state.
    pub state: String,
    /// Where to ask, so a client does not have to know the URL shape.
    pub restore_url: String,
    /// When the copy is expected, if a restore is already under way.
    pub eta_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The storage class the bytes are in, so a client can say "Deep Archive, so this takes hours" rather
    /// than "unavailable".
    pub storage_class: String,
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        match self {
            // 404, not 403. A token that does not verify tells us nothing about whether the asset exists,
            // and answering 403 would confirm that it does.
            Self::NotDeliverable => StatusCode::NOT_FOUND.into_response(),
            Self::Restoring(body) => (StatusCode::ACCEPTED, axum::Json(body)).into_response(),
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
        // Rights first, then the bytes, and the order matters: a caller with no licence must not learn from the
        // refusal whether a rendition exists. A missing rendition is `NotDeliverable`, which is what the share
        // portal and a Q.14 portal already say out loud — "no preview has been rendered yet" — and could not
        // reach until this check existed.
        if !rendition_exists(state, asset_id, transform).await? {
            return Err(Refusal::NotDeliverable);
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
        tenant_id: state.tenant_id,
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
/// Whether this claim's bytes are archived, and what to tell the caller if they are.
///
/// `None` is the overwhelmingly common answer, and for anything but an original it is the *only* answer
/// without a query at all.
///
/// ## Only a claim on the original can be archived
///
/// A derivative is its own object in its own namespace, and `Key::is_tier_exempt` means the lifecycle engine
/// will never move one. So a thumbnail, a preview or a rendered conversion is deliverable whatever the state
/// of the original it came from — which is what keeps a grid of archived assets looking like a grid, and is
/// the whole reason §2 makes the proxies the search substrate.
///
/// The first version of this function ignored the claim's transform and read the original's placement for
/// every delivery. Every thumbnail of an archived asset answered `202`, so archiving anything turned its
/// grid cell into a placeholder — the exact failure the paragraph above says cannot happen. Found by
/// archiving one real asset and looking at the screen: the badge said Archived and the picture next to it
/// had gone.
async fn archived_wait(
    state: &DeliveryState,
    claim: &signed_url::DeliveryClaim,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<RestoringBody>, Refusal> {
    if claim.transform != dam_media::profiles::ORIGINAL {
        return Ok(None);
    }

    // The *coldest* copy of the original, matching how `object_key` picks. A warmest-first read here would
    // report a Standard replica and mint a URL for bytes in Deep Archive.
    let row: Option<(String, String, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT p.storage_class, p.restore_state, p.restore_expires_at \
         FROM object_placements p \
         WHERE p.asset_id = $1 AND p.derivative_id IS NULL AND p.state = 'present' \
         ORDER BY CASE p.storage_class \
                      WHEN 'DEEP_ARCHIVE' THEN 0 WHEN 'GLACIER' THEN 1 ELSE 2 \
                  END, p.object_key \
         LIMIT 1",
    )
    .bind(claim.asset_id)
    .fetch_optional(&state.global)
    .await
    .map_err(|error| {
        tracing::error!(%error, "reading a placement for delivery");
        Refusal::Internal
    })?;

    let Some((class, restore, expires)) = row else {
        // No placement is a freshly finalised upload, which is `Standard` by definition — the same reading
        // `assets::tier_of` takes, and for the same reason.
        return Ok(None);
    };
    let Ok(class) = class.parse::<dam_core::StorageClass>() else {
        tracing::error!(%class, "a placement holds an unknown storage class");
        return Err(Refusal::Internal);
    };
    if !class.requires_restore() {
        return Ok(None);
    }
    // A restored copy is deliverable until it lapses, and the expiry is the authority rather than the state:
    // a row that still says `available` past its window is exactly the trap the schema comments warn about
    // twice, and trusting it would mint a URL that 403s at S3.
    if restore == "available" && expires.is_some_and(|at| at > now) {
        return Ok(None);
    }

    Ok(Some(RestoringBody {
        state: if restore == "none" || restore == "expired" {
            "archive".to_owned()
        } else {
            restore
        },
        restore_url: format!("/assets/{}/restore", claim.asset_id),
        eta_at: in_flight_eta(state, claim.asset_id).await,
        storage_class: class.to_string(),
    }))
}

/// The ETA of a restore already under way, if there is one.
///
/// Best effort: a failure to read it costs the caller a null ETA, not their request. The 202 is still the
/// right answer with or without a date on it.
async fn in_flight_eta(
    state: &DeliveryState,
    asset_id: uuid::Uuid,
) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar(
        "SELECT eta_at FROM restore_requests \
         WHERE asset_id = $1 AND state IN ('queued', 'awaiting_approval', 'requested', 'ongoing') \
         ORDER BY requested_at DESC LIMIT 1",
    )
    .bind(asset_id)
    .fetch_optional(&state.global)
    .await
    .ok()
    .flatten()
}

async fn deliver(
    State(state): State<Arc<DeliveryState>>,
    Path(token): Path<String>,
) -> Result<Response, Refusal> {
    let now = state.now();

    // Step 0. Which keyring verifies this (M3d·2).
    //
    // A connector signs its own render URLs so a page render never blocks on an API call (§11.3), which means
    // some tokens are signed with a secret damrs holds but did not use. Selecting the keyring from an
    // *unverified* key id is unavoidable — verification needs a key before it can decide anything — and safe,
    // because naming the wrong key produces a signature that does not match. What it must never do is let the
    // choice of key confer anything, which is what `bound_by_connector` below is for.
    let connector = match signed_url::key_id_of(&token) {
        Some(key_id) if key_id.starts_with(CONNECTOR_KEY_PREFIX) => {
            Some(connector_for(&state, &key_id, now).await?)
        }
        _ => None,
    };

    // Step 1. Establishes that we issued this request unaltered. Every failure mode collapses to one
    // answer — see `Refusal::NotDeliverable`.
    let claim = signed_url::verify(
        connector.as_ref().map_or(&state.keyring, |c| &c.keyring),
        &token,
        now,
    )
    .map_err(|reason| {
        tracing::debug!(?reason, "delivery token rejected");
        Refusal::NotDeliverable
    })?;

    // Step 1a. What this connector is allowed to have asked for.
    //
    // Everything above proves the token was signed by somebody holding the secret. For a connector that is the
    // *site*, which signs whatever it likes — so without this the signing secret is a bypass of every rule §11
    // claims the connector enforces. Kept adjacent to verification rather than pushed down into the steps
    // below, because each of those already has its own reasons to refuse and a bound buried among them is a
    // bound somebody removes while fixing something else.
    let claim = match &connector {
        Some(connected) => bound_by_connector(&state, connected, claim, now).await?,
        None => claim,
    };

    // Step 1b. The token names a tenant, and this process serves one. They have to be the same one.
    //
    // Every read below resolves through a pool pinned to `state.tenant_id`'s schema, so a claim naming a
    // different tenant would be answered out of *this* tenant's library — the asset id would either miss
    // (a 404 for the wrong reason) or, if the two libraries happen to share an id, hit the wrong asset. The
    // signature makes that unforgeable rather than merely unlikely: since G22 the tenant is inside the
    // payload, so a token cannot be edited from one tenant to another without breaking the signature, and
    // the only way to hold a valid token for another tenant is to have been issued one.
    //
    // Which is possible across deployments that share a signing key — a restored backup, a staging
    // environment cloned from production — and is exactly the case this refuses. Cheap, and it is the check
    // that lets the resolution half of G22 land later without a window where the claim is carried but not
    // honoured.
    if claim.tenant_id != state.tenant_id {
        tracing::warn!(
            claimed = %claim.tenant_id,
            served = %state.tenant_id,
            "a delivery token named a tenant this process does not serve",
        );
        return Err(Refusal::NotDeliverable);
    }

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

    // Step 3. Are the bytes reachable at all?
    //
    // §6.5: a download that resolves to an archived object "cannot be served ... and this is a `202` with an
    // ETA and a cost estimate rather than an error". Before this, the redirect was minted regardless and S3
    // answered the browser with an `InvalidObjectState` XML document — a failure that happened *after* the
    // hand-off, in a response damrs never saw, and which no amount of reading our own logs would explain.
    //
    // The placement is the source of truth rather than a `HEAD` to the store. It is one local read against a
    // row we maintain, on the hot path of every thumbnail in every grid; a vendor round trip per delivery to
    // learn something we already know would be a per-image latency cost paid to answer a question that is
    // almost always "yes, it is Standard".
    if let Some(wait) = archived_wait(&state, &claim, now).await? {
        return Err(Refusal::Restoring(wait));
    }

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
/// The `op_hash` a transform name resolves to.
///
/// Resolved name -> profile -> op_hash -> row, never name -> row.
///
/// This shipped as `WHERE profile = $2` and that was a bug. `op_hash` covers the size, format, quality, fit,
/// background, colour profile and rendering intent (§18.1), so a profile that has been *redefined* has a
/// different hash — and a name lookup would keep serving the bytes rendered under the old definition forever,
/// with no error anywhere and a customer seeing yesterday's quality setting indefinitely.
///
/// Built-in profile first, then the tenant's own conversions (Q.11). Two sources, one derivation, so the mint
/// and the fetch cannot disagree about what a name means.
///
/// The tenant half was missing when the download endpoint shipped, and *only* following a real URL found it: the
/// endpoint returned a perfectly good signed URL for `web-1600`, and the fetch 404'd it because the name was not
/// a built-in. An API test that asserted a URL was returned could not see that, which is what "verify by running
/// the real thing" means in practice.
async fn op_hash_for(state: &DeliveryState, transform: &str) -> Result<String, Refusal> {
    match dam_media::profiles::by_name(transform) {
        Some(profile) => Ok(profile.op_hash()),
        None => dam_db::conversions::by_key(
            &mut *state.global.acquire().await.map_err(dam_db::Error::from)?,
            transform,
        )
        .await?
        // Withdrawn conversions resolve: the token carries the key, and a link issued while a format was
        // offered must keep working. `by_key` includes them for exactly this caller.
        .and_then(|conversion| conversion.op_hash())
        // An unknown transform is not deliverable rather than approximated: rendering something plausible
        // would silently hand back a different size than the caller integrated against.
        .ok_or(Refusal::NotDeliverable),
    }
}

/// Whether the bytes a transform names exist for this asset.
///
/// Checked *before* a distribution token is minted, which is the fix for something only a real browser found: a
/// portal listed eight assets, minted a preview URL for the one its licence allowed, and the URL 404'd — the
/// derivative row had been rendered under an older definition of the profile, so the fetch resolved a different
/// `op_hash` than the render had. Nothing was wrong with the token, and every test that asserted "a URL came
/// back" passed.
///
/// `original` needs no check: it is derived from the asset's own content hash, and an asset with no bytes is not
/// a state this system has.
async fn rendition_exists(
    state: &DeliveryState,
    asset_id: Uuid,
    transform: &str,
) -> Result<bool, Refusal> {
    if transform == "original" {
        return Ok(true);
    }
    let op_hash = op_hash_for(state, transform).await?;
    Ok(
        dam_db::derivatives::by_op_hash(&state.global, asset_id, &op_hash)
            .await?
            .is_some(),
    )
}

/// The key-id prefix a connector-signed token carries.
///
/// `connector:<id>`. Stable across a secret rotation, deliberately: the *site* decides when to switch secrets,
/// so an id that changed with the secret would mean every URL signed before the site's own deploy naming a key
/// that no longer exists. `Keyring::find` returning every secret under one id is what makes that work.
pub const CONNECTOR_KEY_PREFIX: &str = "connector:";

/// A connected site, with the keyring that verifies what it signed.
struct Connected {
    row: dam_db::connectors::Connector,
    keyring: Keyring,
}

/// Resolves the connector a token names, and the keyring that can verify it.
///
/// Every failure is the same flat refusal an unsigned token gets: a malformed id, an unknown connector, a
/// paused or revoked one, a deployment with no sealing keyring, a secret that will not open. Distinguishing
/// them would tell whoever holds the URL which connectors exist and what state they are in.
async fn connector_for(
    state: &DeliveryState,
    key_id: &str,
    now: DateTime<Utc>,
) -> Result<Connected, Refusal> {
    let id: Uuid = key_id
        .trim_start_matches(CONNECTOR_KEY_PREFIX)
        .parse()
        .map_err(|_| Refusal::NotDeliverable)?;

    // No fallback to the server keyring. A deployment that cannot open connector secrets must refuse their
    // tokens: falling back would verify them against a key the site never had, which fails — until the day
    // somebody "fixes" it by trying both.
    let auth = state.connectors.as_ref().ok_or_else(|| {
        tracing::error!("a connector-signed token arrived with no connector auth configured");
        Refusal::NotDeliverable
    })?;

    let mut conn = state
        .global
        .acquire()
        .await
        .map_err(dam_db::Error::from)
        .map_err(|error| {
            tracing::error!(%error, "acquiring a connection to resolve a connector");
            Refusal::Internal
        })?;
    let row = dam_db::connectors::by_id(&mut conn, id)
        .await?
        .ok_or(Refusal::NotDeliverable)?;
    drop(conn);

    // A paused or revoked connector's URLs stop working immediately, which is the whole point of having the
    // states — and it works on URLs already issued, exactly as a revoked share does.
    if !row.status.may_render() {
        return Err(Refusal::NotDeliverable);
    }

    let aad = dam_db::connectors::associated_data(auth.tenant_slug.as_str(), row.id);
    let current = auth
        .sealing
        .open(&row.sealed_secret, &aad)
        .map_err(|error| {
            tracing::error!(%error, connector = %row.id, "opening a connector signing secret");
            Refusal::NotDeliverable
        })?;
    let mut keyring = Keyring::single(key_id, current);
    // The superseded secret, only while it is inside its window. `previous_is_live` reads the clock rather
    // than a cleared column — see `dam_db::connectors`.
    if let Some(previous) = row.live_previous(now)
        && let Ok(opened) = auth.sealing.open(previous, &aad)
    {
        keyring = keyring.with_retired(key_id, opened);
    }

    Ok(Connected { row, keyring })
}

/// What a connector is allowed to have asked for.
///
/// Four bounds, and each closes a way the signing secret would otherwise be a bypass:
///
/// **Distribution only.** `Purpose::InternalPreview` skips the rights check (A.7), because an unlicensed asset
/// is the normal state of a freshly uploaded one and gating the grid's thumbnails on the distribution verdict
/// makes a correct DAM unusable. A connector is external — it is a customer's public website — so a preview
/// purpose from one would be a licence check skipped on a live page. This is the bound that matters most.
///
/// **No share link.** A share's authority belongs to the share. A connector claiming one would be borrowing
/// it, and while `shares::is_live` means it could not exceed what the share allows, "could not exceed" is a
/// worse property than "cannot claim".
///
/// **Originals only if allowed.** `allow_original` is off by default: a CMS wants renditions, and a site that
/// can fetch masters is a site that can leak the deliverable a customer paid for.
///
/// **Only assets the connector can see.** Through the connector's own grants and the ordinary predicate —
/// §11.1's claim that "a misconfigured Drupal view cannot surface an unapproved asset" is only true if this
/// check exists, because a site that knows an asset id can sign a URL for it whether or not it was ever shown
/// one.
///
/// And one substitution rather than a refusal: when the original is cold and `allow_restore` is off, the claim
/// is rewritten to the master proxy. §11.1 is explicit — "a page render must never trigger a Glacier restore"
/// — and a proxy is what the `<img>` tag wanted anyway. Refusing instead would blank an image on a live page
/// for a storage-class reason the site cannot do anything about.
async fn bound_by_connector(
    state: &DeliveryState,
    connected: &Connected,
    claim: DeliveryClaim,
    now: DateTime<Utc>,
) -> Result<DeliveryClaim, Refusal> {
    if !claim.purpose.is_distribution() {
        tracing::warn!(
            connector = %connected.row.id,
            "a connector-signed token claimed an internal preview; refusing",
        );
        return Err(Refusal::NotDeliverable);
    }
    if claim.share_link_id.is_some() {
        tracing::warn!(
            connector = %connected.row.id,
            "a connector-signed token claimed a share link; refusing",
        );
        return Err(Refusal::NotDeliverable);
    }
    if claim.transform == dam_media::profiles::ORIGINAL && !connected.row.allow_original {
        return Err(Refusal::NotDeliverable);
    }

    // The connector's own predicate, resolved the ordinary way: its key's identity holds a role carrying its
    // asset groups, so this is `grants_for` and `policy::compile` exactly as for any caller. Reading the
    // groups off the connector row and compiling them here would be a second place where a connector's scope
    // is decided, and the two would drift.
    let api_key_id = connected.row.api_key_id.ok_or_else(|| {
        tracing::error!(connector = %connected.row.id, "a connector with no api key signed a URL");
        Refusal::NotDeliverable
    })?;
    let identity: Option<Uuid> = sqlx::query_scalar(
        "SELECT identity_id FROM dam_global.api_keys          WHERE id = $1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(api_key_id)
    .fetch_optional(&state.global)
    .await
    .map_err(dam_db::Error::from)?
    .flatten();
    // A revoked or expired key stops the URLs too. Otherwise revoking a connector's credential would leave
    // its render URLs working for as long as the site kept signing them, which is indefinitely.
    let identity = identity.ok_or(Refusal::NotDeliverable)?;

    let mut conn = state
        .global
        .acquire()
        .await
        .map_err(dam_db::Error::from)
        .map_err(|_| Refusal::Internal)?;
    let grants =
        dam_db::auth::grants_for(&state.global, &mut *conn, state.tenant_id, identity, &[]).await?;
    let predicate = dam_core::policy::compile(&grants, dam_core::policy::Action::Read, now);
    let visible = dam_db::assets::visible_among(&mut *conn, &predicate, &[claim.asset_id]).await?;
    drop(conn);
    if visible.is_empty() {
        return Err(Refusal::NotDeliverable);
    }

    // The cold-original substitution. Checked here rather than left to `archived_wait` below, because that
    // function's answer is a 202 with an ETA — the right answer for a person who asked for a master, and the
    // wrong one for an `<img>` tag on a page nobody is watching.
    if claim.transform == dam_media::profiles::ORIGINAL
        && !connected.row.allow_restore
        && archived_wait(state, &claim, now).await?.is_some()
    {
        return Ok(DeliveryClaim {
            transform: dam_media::profiles::WEB_2048.name.to_owned(),
            ..claim
        });
    }

    Ok(claim)
}

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

    let op_hash = op_hash_for(state, &claim.transform).await?;

    let derivative = dam_db::derivatives::by_op_hash(&state.global, claim.asset_id, &op_hash)
        .await?
        // A miss is not a failure — it means this recipe has not been rendered yet. For a tenant conversion the
        // download endpoint queues the render and answers 202, so a caller reaching here with a miss is one
        // whose URL was minted before the bytes existed and used after they were reaped.
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
