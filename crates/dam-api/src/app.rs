//! The whole HTTP surface, assembled once.
//!
//! Each feature module owns its routes and its state; this composes them and applies the layers that must
//! wrap *everything*. Composition in one place is what makes "is this endpoint authenticated" answerable by
//! reading a single file — and it is where a route added without a timeout or a body limit becomes visible.
//!
//! ## Timeouts and body limits are outermost, and apply to every route
//!
//! A handler that forgets a timeout is a handler that holds a connection until the client goes away. Putting
//! the budget here means a new route inherits it rather than remembering it.
//!
//! ## `/health` is unauthenticated and says nothing
//!
//! It answers 200 with a fixed body. A health endpoint that reported version, tenant counts or database
//! state would be an unauthenticated disclosure endpoint, and it is the first thing anybody scans.

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, StatusCode, header};
use axum::routing::get;
use dam_core::Config;
use dam_search::IndexPool;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{Any, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

/// Everything the surface needs to be built.
pub struct AppDeps {
    /// The shared pool. Tenant scoping is per request through `TenantConn` (§5.2).
    pub global: PgPool,
    /// A pool pinned to the delivery tenant's schema.
    ///
    /// The delivery route reads tenant-schema tables written unqualified, and it serves exactly one tenant by
    /// construction — see `DeliveryState::global`. Separate from `global` rather than derived here, because
    /// building it needs the database URL and this function has a `Config` that redacts it.
    pub delivery_pool: PgPool,
    pub store: Arc<dyn dam_store::ResumableStore>,
    /// The blob store the delivery path presigns from.
    pub delivery_store: Arc<dyn dam_store::BlobStore>,
    pub indexes: Arc<IndexPool>,
    pub keyring: dam_core::signed_url::Keyring,
    /// Only the delivery routes need this, and only until 3.x makes delivery tenant-resolved from the token
    /// rather than from configuration.
    pub delivery_tenant: uuid::Uuid,
    /// Builds a protocol router over the states this module assembles — today, the MCP server.
    ///
    /// A closure rather than a `Router` or a flag, and the reason is the dependency graph: `dam-mcp` calls the
    /// REST handlers on purpose (§8.5's "the same ABAC layer"), so it depends on `dam-api` and `dam-api` cannot
    /// depend on it. Inverting it here lets the binary wire the crate while this module still owns the states —
    /// which is what keeps the MCP tools and the HTTP routes reading the same pools and the same index pool
    /// rather than two of each.
    ///
    /// `None` mounts nothing: an MCP endpoint is a second front door, and a deployment that does not want agents
    /// talking to its library should not have to trust that nobody has a key.
    #[allow(clippy::type_complexity)]
    pub protocols: Option<
        Box<
            dyn FnOnce(
                Arc<crate::search::SearchState>,
                Arc<crate::downloads::DownloadState>,
            ) -> Router,
        >,
    >,
    /// How a hosted-model call leaves the process.
    ///
    /// Injected rather than constructed here so the credential-verify route can be driven against a recorded
    /// transport — the same seam `dam_ai::model::Transport` exists for. `dam_ai::http::HttpTransport` is what a
    /// binary passes.
    pub model_transport: Arc<dyn dam_ai::model::Transport>,
}

impl std::fmt::Debug for AppDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppDeps").finish_non_exhaustive()
    }
}

/// The largest JSON body any endpoint accepts.
///
/// Metadata is the biggest of them and is measured in kilobytes. The upload routes set their own, larger
/// limit inside `tus::router`, which is why this one can be small: a global limit sized for the largest
/// endpoint is no limit at all for the rest.
const MAX_JSON_BODY: usize = 256 * 1024;

/// Assembles the router.
pub fn router(cfg: &Config, deps: AppDeps) -> Router {
    // Built first and shared, because a thumbnail URL is a delivery token: the asset endpoints mint them and
    // the delivery route verifies them, and two keyrings would mean tokens that fail verification — which
    // presents as an intermittently broken grid rather than as a configuration error.
    let delivery = Arc::new(
        crate::delivery::DeliveryState::new(
            deps.delivery_pool.clone(),
            Arc::clone(&deps.delivery_store),
            deps.keyring.clone(),
            deps.delivery_tenant,
        )
        .with_public_url(cfg.server.public_url.clone()),
    );

    // Built before the API router so the two share one `SearchState` and one `DownloadState` — the MCP tools
    // *are* the REST handlers (§8.5), and two states would be two pools, two index pools and eventually two
    // answers to the same question.
    let search_state = Arc::new(crate::search::SearchState {
        global: deps.global.clone(),
        indexes: Arc::clone(&deps.indexes),
        delivery: Some(Arc::clone(&delivery)),
    });
    let download_state = Arc::new(crate::downloads::DownloadState {
        global: deps.global.clone(),
        delivery: Some(Arc::clone(&delivery)),
    });
    let protocols = deps
        .protocols
        .map(|build| build(Arc::clone(&search_state), Arc::clone(&download_state)));

    let api = Router::new()
        .merge(crate::assets::router(crate::assets::AssetState {
            global: deps.global.clone(),
            delivery: Some(Arc::clone(&delivery)),
        }))
        .merge(crate::search::router_from(Arc::clone(&search_state)))
        .merge(crate::bulk::router(crate::bulk::BulkState {
            global: deps.global.clone(),
        }))
        .merge(crate::upload_profiles::router(
            crate::upload_profiles::UploadProfileState {
                global: deps.global.clone(),
            },
        ))
        .merge(crate::attachments::router(
            crate::attachments::AttachmentState {
                global: deps.global.clone(),
            },
        ))
        .merge(crate::orders::router(crate::orders::OrderState {
            global: deps.global.clone(),
            // The same origin the delivery URLs use: a pickup link and a delivery link are both things somebody
            // pastes into a browser, and two sources for "where are we reachable" would disagree eventually.
            public_url: cfg.server.public_url.clone(),
        }))
        .merge(crate::downloads::router_from(Arc::clone(&download_state)))
        .merge(crate::conversions::router(
            crate::conversions::ConversionState {
                global: deps.global.clone(),
            },
        ))
        .merge(crate::archival::router(crate::archival::ArchivalState {
            global: deps.global.clone(),
        }))
        .merge(crate::proofing::router(crate::proofing::ProofingState {
            global: deps.global.clone(),
            delivery: Some(Arc::clone(&delivery)),
        }))
        .merge(crate::duplicates::router(
            crate::duplicates::DuplicateState {
                global: deps.global.clone(),
                delivery: Some(Arc::clone(&delivery)),
            },
        ))
        .merge(crate::branding::router(crate::branding::BrandingState {
            global: deps.global.clone(),
            delivery: Some(Arc::clone(&delivery)),
        }))
        .merge(crate::webhooks::router(crate::webhooks::WebhookState {
            global: deps.global.clone(),
            // A developer's receiver is on localhost over http, and the first version of this refused it —
            // which made developing a webhook integration impossible without writing SQL. Permits those two
            // things and nothing else: private and link-local stay refused everywhere.
            development: matches!(cfg.environment, dam_core::config::Environment::Development),
        }))
        .merge(crate::vocabularies::router(
            crate::vocabularies::VocabularyState {
                global: deps.global.clone(),
            },
        ))
        .merge(crate::worklists::router(crate::worklists::WorklistState {
            global: deps.global.clone(),
            delivery: Some(Arc::clone(&delivery)),
        }))
        .merge(crate::collections::router(
            crate::collections::CollectionState {
                global: deps.global.clone(),
                delivery: Some(Arc::clone(&delivery)),
            },
        ))
        .merge(crate::history::router(crate::history::HistoryState {
            global: deps.global.clone(),
        }))
        .merge(crate::versions::router(crate::versions::VersionState {
            global: deps.global.clone(),
        }))
        .merge(crate::dashboard::router(crate::dashboard::DashboardState {
            global: deps.global.clone(),
        }))
        .merge(crate::comments::router(crate::comments::CommentState {
            global: deps.global.clone(),
        }))
        .merge(crate::engagement::router(
            crate::engagement::EngagementState {
                global: deps.global.clone(),
            },
        ))
        .merge(crate::auto_import::router(
            crate::auto_import::AutoImportState {
                global: deps.global.clone(),
            },
        ))
        .merge(crate::categories::router(
            crate::categories::CategoryState {
                global: deps.global.clone(),
            },
        ))
        .merge(crate::schema::router(crate::schema::SchemaState {
            global: deps.global.clone(),
        }))
        .merge(crate::ai::router(crate::ai::AiState {
            global: deps.global.clone(),
            // Built once here rather than per request: it derives a key per entry, and doing that on the path of
            // every credential read would be a cost paid for nothing.
            keyring: cfg.ai.keyring(),
            prices: dam_ai::pricing::Prices::with_overrides(&cfg.ai.prices),
            transport: Arc::clone(&deps.model_transport),
        }))
        .merge(crate::shares::router(crate::shares::ShareState {
            global: deps.global.clone(),
            delivery: Arc::clone(&delivery),
        }))
        // The *same* delivery state as the share portal: a portal's previews are minted here and verified
        // there, and two keyrings would mean tokens that fail verification — the bug 3.x already paid for once.
        .merge(crate::portals::router(crate::portals::PortalState {
            global: deps.global.clone(),
            delivery: Arc::clone(&delivery),
        }))
        .layer(DefaultBodyLimit::max(MAX_JSON_BODY));

    // The registry is created here and handed to both the middleware and the endpoint, so there is exactly one
    // per process. A second registry would render a second, partial view and nothing would say which.
    let metrics = dam_telemetry::metrics::Metrics::new();

    Router::new()
        .route("/health", get(health))
        .merge(crate::observability::router(
            crate::observability::ObservabilityState {
                global: deps.global.clone(),
                store: Arc::clone(&deps.delivery_store),
                metrics: metrics.clone(),
                metrics_token: cfg.server.metrics_token.clone(),
            },
        ))
        .merge(api)
        // Its own body limit, set inside its own router: a resumable upload chunk is megabytes.
        .merge(crate::tus::router(crate::tus::AppState::new(
            deps.global.clone(),
            Arc::clone(&deps.store),
        )))
        // The *same* state the asset endpoints mint preview tokens with. A second `DeliveryState` here was a
        // real bug: it was built from the global pool, so every delivery failed with `relation "derivatives"
        // does not exist` while the mint side worked perfectly — and two keyrings would have been the next
        // failure once one rotated.
        // The public routes, behind the limiter when one is configured. Layered on this merge rather than on
        // the whole app, because the authenticated API must not be address-keyed — see `throttle`'s docs.
        .merge(throttled(crate::delivery::router_from(delivery), cfg))
        // Outside the JSON body limit above, because a protocol router frames its own requests and enforces its
        // own maximum. Mounted here rather than inside `api` for the same reason `tus` is.
        .merge(protocols.unwrap_or_default())
        // Applied to every route above, including the ones a later change adds.
        // `with_status_code` rather than the deprecated `new`, and 504 rather than 408: the request did not
        // time out on the client's side, the server gave up on it, and a client retrying a 408 forever is the
        // failure that distinction prevents.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            Duration::from_secs(cfg.server.request_timeout_secs),
        ))
        // `nosniff` because a delivery response carries a content type the *uploader* influenced, and a
        // browser that sniffs its way to `text/html` on a file somebody uploaded is stored XSS.
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(cors(cfg))
        // Outermost of the application layers, so the timing it records is what a client experienced —
        // including the time spent in the timeout layer and in CORS — rather than only the handler. A
        // middleware placed under the timeout would report a fast request for one the client saw as a 504.
        .layer(axum::middleware::from_fn_with_state(
            metrics,
            crate::observability::record,
        ))
        .layer(TraceLayer::new_for_http())
}

/// Wraps a router in the address-keyed limiter, when one is configured.
///
/// Returns the router untouched when `rate_limit_per_second` is unset, which is the default. A limiter with a
/// guessed number either does nothing or throttles a legitimate first page load, and neither is discovered
/// until it is in front of users — so it is opt-in with the number an operator chose.
fn throttled(router: Router, cfg: &Config) -> Router {
    let Some(per_second) = cfg
        .server
        .rate_limit_per_second
        .and_then(std::num::NonZeroU32::new)
    else {
        return router;
    };
    // A burst below the sustained rate would be a lower ceiling than the rate it is meant to permit, so the
    // rate is the floor. Stated here rather than validated in config, because the fix is obvious and refusing
    // to start over it would be worse than quietly doing the sensible thing.
    let burst = std::num::NonZeroU32::new(cfg.server.rate_limit_burst.max(per_second.get()))
        .unwrap_or(per_second);
    router.layer(axum::middleware::from_fn_with_state(
        crate::throttle::Throttle::new(per_second, burst, cfg.server.trusted_proxy_hops),
        crate::throttle::limit,
    ))
}

/// The CORS policy.
///
/// Credentials here are a bearer token in a header rather than a cookie, so there is no ambient authority for
/// a hostile origin to ride: a cross-origin request without the header is anonymous, and one *with* the header
/// had to be given the key. That is what makes a permissive origin list defensible for the API, and it is
/// worth being explicit about rather than leaving as an unexamined `Any`.
///
/// Outside development the allowed origins are configured, because a browser is not the only client and a
/// wildcard is one bad cookie decision away from being the whole story.
fn cors(cfg: &Config) -> CorsLayer {
    let layer = CorsLayer::new()
        .allow_methods(Any)
        // TUS needs its own request headers through, and `expose_headers` matters as much: a client that
        // cannot read `Upload-Offset` cannot resume, and the failure looks like a protocol bug.
        .allow_headers(Any)
        .expose_headers(Any);

    if cfg.environment.is_production() {
        let origins: Vec<HeaderValue> = cfg
            .server
            .allowed_origins
            .iter()
            .filter_map(|origin| origin.parse().ok())
            .collect();
        layer.allow_origin(origins)
    } else {
        layer.allow_origin(Any)
    }
}

/// Liveness. Says nothing — see the module docs.
async fn health() -> (StatusCode, &'static str) {
    (StatusCode::OK, "ok")
}
