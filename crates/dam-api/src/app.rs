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

    let api = Router::new()
        .merge(crate::assets::router(crate::assets::AssetState {
            global: deps.global.clone(),
            delivery: Some(Arc::clone(&delivery)),
        }))
        .merge(crate::search::router(crate::search::SearchState {
            global: deps.global.clone(),
            indexes: Arc::clone(&deps.indexes),
        }))
        .merge(crate::bulk::router(crate::bulk::BulkState {
            global: deps.global.clone(),
        }))
        .merge(crate::upload_profiles::router(
            crate::upload_profiles::UploadProfileState {
                global: deps.global.clone(),
            },
        ))
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
        .merge(crate::shares::router(crate::shares::ShareState {
            global: deps.global.clone(),
            delivery: Arc::clone(&delivery),
        }))
        .layer(DefaultBodyLimit::max(MAX_JSON_BODY));

    Router::new()
        .route("/health", get(health))
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
        .merge(crate::delivery::router_from(delivery))
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
        .layer(TraceLayer::new_for_http())
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
