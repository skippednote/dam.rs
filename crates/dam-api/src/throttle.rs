//! Rate limiting the routes that face the internet.
//!
//! `governor` has been a declared dependency of this crate — commented "per-tenant rate limiting" — and never
//! called. This is the call.
//!
//! ## Only the public routes, and keyed by address
//!
//! The internet-facing surface is `/d/{token}`, `/share/{token}` and `/portal/{key}`. Those take no API key,
//! so the only thing to key a limit on is where the request came from, and they are the ones worth protecting:
//! a token guess costs an HMAC verify, and a portal page costs a database read.
//!
//! The authenticated API is deliberately **not** limited by address, and that is not an oversight. A company
//! using a DAM sits behind one or two egress addresses, so an address-keyed limit on the API is a limit on the
//! customer as a whole — the entire art department sharing one bucket, and a bulk upload starving everybody
//! else's thumbnails. Authenticated traffic is already bounded by a credential that can be revoked and by the
//! per-tenant quotas in `dam_db::quotas`, which are the right instruments for it.
//!
//! ## Trusting `X-Forwarded-For` only when told to
//!
//! Taking the leftmost `X-Forwarded-For` entry is the classic mistake: the header is client-supplied, so
//! anybody can put a different address in it and get a fresh bucket per request — turning the limiter into
//! decoration — or put *somebody else's* address in it and exhaust their bucket instead.
//!
//! So the peer address is the default, and a deployment behind proxies sets `trusted_proxy_hops` to how many
//! of them there are. The address taken is then that many entries from the **right** of the header, because
//! the rightmost entries are the ones each successive proxy appended and therefore the only ones a client
//! could not forge.

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;

/// A limiter keyed by client address.
#[derive(Clone)]
pub struct Throttle {
    limiter: Arc<RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>>,
    /// How many reverse proxies sit in front. Zero means trust nothing but the socket.
    trusted_proxy_hops: usize,
}

impl std::fmt::Debug for Throttle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Throttle")
            .field("trusted_proxy_hops", &self.trusted_proxy_hops)
            .finish_non_exhaustive()
    }
}

impl Throttle {
    /// `per_second` sustained, `burst` in hand.
    ///
    /// A burst larger than the rate is what makes this usable on a page that fetches sixty thumbnails at
    /// once: the sustained rate governs abuse, and the burst governs whether an ordinary grid load looks like
    /// abuse. A limiter tuned only on the sustained rate throttles the first screen every user ever sees.
    pub fn new(per_second: NonZeroU32, burst: NonZeroU32, trusted_proxy_hops: usize) -> Self {
        Self {
            limiter: Arc::new(RateLimiter::keyed(
                Quota::per_second(per_second).allow_burst(burst),
            )),
            trusted_proxy_hops,
        }
    }

    /// The address to key on.
    ///
    /// See the module docs for why the count is from the right.
    fn client(&self, headers: &HeaderMap, peer: Option<SocketAddr>) -> Option<IpAddr> {
        let peer_ip = peer.map(|socket| socket.ip());
        if self.trusted_proxy_hops == 0 {
            return peer_ip;
        }
        let forwarded = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())?;
        let hops: Vec<&str> = forwarded.split(',').map(str::trim).collect();
        // `hops` holds what the proxies appended, oldest first. With one trusted proxy the client is the last
        // entry; with two it is the second-to-last, because the outermost proxy appended the address of the
        // inner one. Falling back to the peer when the header is shorter than claimed is the safe direction:
        // it under-trusts rather than accepting a forged left-hand entry.
        hops.len()
            .checked_sub(self.trusted_proxy_hops)
            .and_then(|index| hops.get(index))
            .and_then(|candidate| candidate.parse().ok())
            .or(peer_ip)
    }
}

/// Refuses a request that is over its address's budget.
///
/// `Option<ConnectInfo<_>>`, and the option is load-bearing. A required extractor here *rejects* when the
/// server was built without `into_make_service_with_connect_info` — turning a wiring mistake into a 500 on
/// every public route, which is a worse outage than the traffic the limiter exists to survive. It also makes
/// the middleware untestable through `oneshot`, which sets no connection extension.
///
/// So a request with no determinable address is allowed, and the wiring is asserted by a test that supplies
/// the extension rather than by hoping one line in `damd` stays put. The first version of this took the
/// extractor by value and documented the opposite of what it did.
pub async fn limit(State(throttle): State<Throttle>, request: Request, next: Next) -> Response {
    // Read from the extensions rather than taken as an extractor: `Option<ConnectInfo<_>>` is not a position
    // axum accepts, and this is what `ConnectInfo` does internally anyway. The difference is that a missing
    // extension is `None` here instead of a rejection.
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(socket)| *socket);
    let Some(client) = throttle.client(request.headers(), peer) else {
        return next.run(request).await;
    };

    match throttle.limiter.check_key(&client) {
        Ok(()) => next.run(request).await,
        Err(until) => {
            let wait = until.wait_time_from(governor::clock::Clock::now(&DefaultClock::default()));
            let seconds = wait.as_secs().max(1);
            tracing::debug!(%client, seconds, "rate limited");
            // `Retry-After` because a client that does not know when to come back comes back immediately.
            // 429 rather than 503: the service is fine, this caller is asking too often.
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, seconds.to_string())],
                "too many requests\n",
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    fn nz(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("non-zero")
    }

    fn socket(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().expect("ip"), 1234)
    }

    #[test]
    fn the_socket_is_the_key_when_no_proxy_is_trusted() {
        let throttle = Throttle::new(nz(1), nz(1), 0);
        let mut headers = HeaderMap::new();
        // A client claiming to be somebody else must be ignored entirely.
        headers.insert("x-forwarded-for", "203.0.113.9".parse().expect("header"));
        assert_eq!(
            throttle.client(&headers, Some(socket("192.0.2.1"))),
            Some("192.0.2.1".parse::<IpAddr>().expect("ip")),
            "a forwarded header must not be believed when nothing was said about proxies"
        );
    }

    #[test]
    fn one_trusted_proxy_takes_the_rightmost_entry() {
        let throttle = Throttle::new(nz(1), nz(1), 1);
        let mut headers = HeaderMap::new();
        // The left entry is whatever the client sent; the right is what the proxy appended.
        headers.insert(
            "x-forwarded-for",
            "1.2.3.4, 198.51.100.7".parse().expect("header"),
        );
        assert_eq!(
            throttle.client(&headers, Some(socket("10.0.0.1"))),
            Some("198.51.100.7".parse::<IpAddr>().expect("ip")),
            "the forgeable left-hand entry must be ignored"
        );
    }

    #[test]
    fn two_trusted_proxies_step_one_further_left() {
        let throttle = Throttle::new(nz(1), nz(1), 2);
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "1.2.3.4, 198.51.100.7, 198.51.100.8"
                .parse()
                .expect("header"),
        );
        assert_eq!(
            throttle.client(&headers, Some(socket("10.0.0.1"))),
            Some("198.51.100.7".parse::<IpAddr>().expect("ip"))
        );
    }

    /// A header shorter than the claimed hop count is a misconfiguration or an attack; either way it must not
    /// promote a client-supplied entry.
    #[test]
    fn a_short_header_falls_back_to_the_socket() {
        let throttle = Throttle::new(nz(1), nz(1), 3);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4".parse().expect("header"));
        assert_eq!(
            throttle.client(&headers, Some(socket("10.0.0.1"))),
            Some("10.0.0.1".parse::<IpAddr>().expect("ip"))
        );
    }

    #[test]
    fn the_burst_is_spent_before_anything_is_refused() {
        let throttle = Throttle::new(nz(1), nz(5), 0);
        let ip: IpAddr = "192.0.2.50".parse().expect("ip");
        for n in 0..5 {
            assert!(
                throttle.limiter.check_key(&ip).is_ok(),
                "request {n} is inside the burst, and a grid loading sixty thumbnails must not look like abuse"
            );
        }
        assert!(
            throttle.limiter.check_key(&ip).is_err(),
            "and the one past the burst is refused"
        );
    }

    /// The wiring, asserted rather than trusted.
    ///
    /// A `ConnectInfo` extension present means the limiter engages; absent means it allows. The second half is
    /// what keeps a missing `into_make_service_with_connect_info` in `damd` from becoming a 500 on every
    /// public route, and the first half is what stops that leniency from meaning the limiter never runs.
    #[tokio::test]
    async fn the_limiter_engages_with_a_peer_and_allows_without_one() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use axum::routing::get;
        use tower::ServiceExt;

        let app = axum::Router::new()
            .route("/public", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                Throttle::new(nz(1), nz(2), 0),
                limit,
            ));

        let with_peer = || {
            let mut request = HttpRequest::builder()
                .uri("/public")
                .body(Body::empty())
                .expect("request");
            request
                .extensions_mut()
                .insert(ConnectInfo(socket("192.0.2.99")));
            request
        };

        for n in 0..2 {
            let response = app.clone().oneshot(with_peer()).await.expect("response");
            assert_eq!(response.status(), StatusCode::OK, "burst request {n}");
        }
        let refused = app.clone().oneshot(with_peer()).await.expect("response");
        assert_eq!(
            refused.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "past the burst the limiter must actually refuse"
        );
        assert!(
            refused.headers().contains_key(header::RETRY_AFTER),
            "a client that is not told when to come back comes back immediately"
        );

        // No extension: allowed, however many times.
        for n in 0..5 {
            let bare = HttpRequest::builder()
                .uri("/public")
                .body(Body::empty())
                .expect("request");
            let response = app.clone().oneshot(bare).await.expect("response");
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "request {n} with no connection info must not be refused — a wiring mistake is not traffic"
            );
        }
    }

    #[test]
    fn one_addresss_budget_is_not_anothers() {
        let throttle = Throttle::new(nz(1), nz(1), 0);
        let a: IpAddr = "192.0.2.1".parse().expect("ip");
        let b: IpAddr = "192.0.2.2".parse().expect("ip");
        assert!(throttle.limiter.check_key(&a).is_ok());
        assert!(throttle.limiter.check_key(&a).is_err(), "a is spent");
        assert!(
            throttle.limiter.check_key(&b).is_ok(),
            "b must be unaffected, or one noisy client denies service to everyone"
        );
    }
}
