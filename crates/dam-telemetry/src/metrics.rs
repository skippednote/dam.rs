//! A small Prometheus registry, deliberately hand-written.
//!
//! ## Why not the `metrics` crate
//!
//! Two dependencies and a transitive tree, for four series. Everything this needs is a counter, a
//! fixed-bucket histogram and a gauge, rendered as text — about two hundred lines, all of it visible, with no
//! global recorder to install and no ordering question about when it is installed relative to the subscriber.
//! If the metric set grows past what fits on one screen, the ecosystem crate is the right answer and this
//! should be deleted rather than extended.
//!
//! ## Cardinality is the whole design problem
//!
//! A Prometheus label set multiplies. `route` **must** be the matched path template — `/assets/{asset_id}` —
//! and never the request URI, or a library with a million assets produces a million series and takes the
//! monitoring system down with it. That is enforced at the call site by taking axum's `MatchedPath`, and it is
//! the one rule in here worth stating twice.
//!
//! Status is recorded as a class (`2xx`, `4xx`, `5xx`) rather than a code for the same reason at smaller
//! scale: the questions asked of it — is anything failing, how much — are answered by the class, and the
//! exact code is in the logs and the trace.

use std::collections::BTreeMap;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// Latency buckets, in seconds.
///
/// Chosen for what this service actually does rather than from a template: a thumbnail redirect should be in
/// the first two buckets, a search in the middle, and anything past two seconds is a question. The last bucket
/// before `+Inf` is deliberately generous because a large multipart upload legitimately lives there.
const BUCKETS: [f64; 9] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 1.0, 2.5, 10.0];

/// One label set: what is being measured about which route.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Labels {
    method: &'static str,
    route: String,
    /// `2xx`, `4xx`, `5xx`. Empty for the histogram, which is not split by outcome.
    class: &'static str,
}

#[derive(Debug, Default)]
struct Histogram {
    /// Counts per bucket, cumulative rendering happens at scrape time.
    buckets: [AtomicU64; BUCKETS.len()],
    /// Observations above the last bucket.
    overflow: AtomicU64,
    count: AtomicU64,
    /// Sum in microseconds, because an f64 has no atomic and a lock per observation would be worse than the
    /// rounding. Rendered back to seconds at scrape time.
    sum_micros: AtomicU64,
}

impl Histogram {
    fn observe(&self, seconds: f64) {
        let index = BUCKETS.iter().position(|edge| seconds <= *edge);
        match index {
            Some(i) => self.buckets[i].fetch_add(1, Ordering::Relaxed),
            None => self.overflow.fetch_add(1, Ordering::Relaxed),
        };
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_micros
            .fetch_add((seconds * 1_000_000.0) as u64, Ordering::Relaxed);
    }
}

/// The process's metrics.
///
/// Cloneable and shared: one per process, held by the router and by whatever sets gauges.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    requests: RwLock<BTreeMap<Labels, AtomicU64>>,
    durations: RwLock<BTreeMap<Labels, Histogram>>,
    /// Named gauges with their own label text, set wholesale rather than incremented.
    ///
    /// Job queue depth is the motivating case: it is a fact about the database, not something this process
    /// counts, so it is *read* and published rather than accumulated. Replacing the whole map on each refresh
    /// is what stops a kind that has gone to zero from reporting its last non-zero value forever.
    gauges: RwLock<BTreeMap<String, Vec<(String, i64)>>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one finished request.
    ///
    /// `route` must be a path *template*. See the module docs: passing a URI here is how a monitoring system
    /// falls over.
    pub fn request(&self, method: &'static str, route: &str, status: u16, seconds: f64) {
        let class = match status {
            100..=299 => "2xx",
            300..=399 => "3xx",
            400..=499 => "4xx",
            _ => "5xx",
        };
        let counted = Labels {
            method,
            route: route.to_owned(),
            class,
        };
        // Read-locked fast path: after the first request for a label set, every subsequent one only needs the
        // atomic. The write lock is taken once per new label set, and the set is bounded by routes × methods ×
        // classes precisely because `route` is a template.
        {
            let map = self
                .inner
                .requests
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(counter) = map.get(&counted) {
                counter.fetch_add(1, Ordering::Relaxed);
            } else {
                drop(map);
                self.inner
                    .requests
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry(counted)
                    .or_default()
                    .fetch_add(1, Ordering::Relaxed);
            }
        }

        let timed = Labels {
            method,
            route: route.to_owned(),
            class: "",
        };
        {
            let map = self
                .inner
                .durations
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(histogram) = map.get(&timed) {
                histogram.observe(seconds);
                return;
            }
        }
        self.inner
            .durations
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(timed)
            .or_default()
            .observe(seconds);
    }

    /// Publishes a gauge family, replacing whatever was there.
    ///
    /// Wholesale rather than per-series: a queue that has drained must stop reporting its last depth, and an
    /// incremental API makes that the caller's problem to remember.
    pub fn set_gauge(&self, name: &str, series: Vec<(String, i64)>) {
        self.inner
            .gauges
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_owned(), series);
    }

    /// The Prometheus text exposition of everything recorded.
    pub fn render(&self) -> String {
        let mut out = String::new();

        out.push_str("# HELP damrs_http_requests_total Requests by method, route template and status class.\n");
        out.push_str("# TYPE damrs_http_requests_total counter\n");
        for (labels, count) in self
            .inner
            .requests
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            out.push_str(&format!(
                "damrs_http_requests_total{{method=\"{}\",route=\"{}\",status=\"{}\"}} {}\n",
                labels.method,
                escape(&labels.route),
                labels.class,
                count.load(Ordering::Relaxed)
            ));
        }

        out.push_str(
            "# HELP damrs_http_request_duration_seconds Request latency by route template.\n",
        );
        out.push_str("# TYPE damrs_http_request_duration_seconds histogram\n");
        for (labels, histogram) in self
            .inner
            .durations
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let route = escape(&labels.route);
            // Cumulative, which is what a Prometheus histogram means: each bucket counts everything at or
            // below its edge. Storing them non-cumulatively and summing here keeps the hot path to one atomic
            // add rather than one per bucket.
            let mut running = 0u64;
            for (edge, bucket) in BUCKETS.iter().zip(&histogram.buckets) {
                running += bucket.load(Ordering::Relaxed);
                out.push_str(&format!(
                    "damrs_http_request_duration_seconds_bucket{{method=\"{}\",route=\"{route}\",le=\"{edge}\"}} {running}\n",
                    labels.method
                ));
            }
            running += histogram.overflow.load(Ordering::Relaxed);
            out.push_str(&format!(
                "damrs_http_request_duration_seconds_bucket{{method=\"{}\",route=\"{route}\",le=\"+Inf\"}} {running}\n",
                labels.method
            ));
            out.push_str(&format!(
                "damrs_http_request_duration_seconds_sum{{method=\"{}\",route=\"{route}\"}} {}\n",
                labels.method,
                histogram.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0
            ));
            out.push_str(&format!(
                "damrs_http_request_duration_seconds_count{{method=\"{}\",route=\"{route}\"}} {}\n",
                labels.method,
                histogram.count.load(Ordering::Relaxed)
            ));
        }

        for (name, series) in self
            .inner
            .gauges
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            out.push_str(&format!("# TYPE {name} gauge\n"));
            for (labels, value) in series {
                out.push_str(&format!("{name}{{{labels}}} {value}\n"));
            }
        }

        out
    }
}

/// Prometheus label-value escaping: backslash, quote, newline.
///
/// Route templates contain none of these today, which is exactly why it is easy to forget and worth doing
/// once here rather than trusting that they never will.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_counted_by_class_not_by_code() {
        let metrics = Metrics::new();
        metrics.request("GET", "/assets/{id}", 200, 0.004);
        metrics.request("GET", "/assets/{id}", 204, 0.004);
        metrics.request("GET", "/assets/{id}", 404, 0.001);

        let text = metrics.render();
        // 200 and 204 share a series; a per-code label would have made three.
        assert!(
            text.contains(
                r#"damrs_http_requests_total{method="GET",route="/assets/{id}",status="2xx"} 2"#
            ),
            "{text}"
        );
        assert!(
            text.contains(
                r#"damrs_http_requests_total{method="GET",route="/assets/{id}",status="4xx"} 1"#
            ),
            "{text}"
        );
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_include_infinity() {
        let metrics = Metrics::new();
        metrics.request("GET", "/x", 200, 0.004); // first bucket
        metrics.request("GET", "/x", 200, 0.2); // 0.25
        metrics.request("GET", "/x", 200, 60.0); // overflow

        let text = metrics.render();
        assert!(text.contains(r#"le="0.005"} 1"#), "{text}");
        // Cumulative: the 0.25 bucket must include the 0.004 observation too.
        assert!(text.contains(r#"le="0.25"} 2"#), "{text}");
        assert!(
            text.contains(r#"le="+Inf"} 3"#),
            "an observation past the last edge must still be counted: {text}"
        );
        assert!(
            text.contains("_count{method=\"GET\",route=\"/x\"} 3"),
            "{text}"
        );
    }

    /// The failure this whole design is shaped around.
    #[test]
    fn one_route_template_is_one_series_however_many_ids_pass_through() {
        let metrics = Metrics::new();
        for _ in 0..1_000 {
            metrics.request("GET", "/assets/{asset_id}", 200, 0.01);
        }
        let series = metrics
            .render()
            .lines()
            .filter(|line| line.starts_with("damrs_http_requests_total{"))
            .count();
        assert_eq!(
            series, 1,
            "a thousand requests to one template must be one series — the alternative is a million series \
             for a million assets and a monitoring system that falls over"
        );
    }

    #[test]
    fn a_gauge_family_is_replaced_rather_than_accumulated() {
        let metrics = Metrics::new();
        metrics.set_gauge(
            "damrs_jobs",
            vec![(r#"kind="derive",state="queued""#.to_owned(), 5)],
        );
        assert!(
            metrics
                .render()
                .contains(r#"damrs_jobs{kind="derive",state="queued"} 5"#)
        );

        // The queue drained. The old value must not linger.
        metrics.set_gauge("damrs_jobs", vec![]);
        assert!(
            !metrics.render().contains("damrs_jobs{"),
            "a drained queue that keeps reporting its last depth is an alert nobody can clear"
        );
    }

    #[test]
    fn label_values_are_escaped() {
        let metrics = Metrics::new();
        metrics.request("GET", "/a\"b\\c", 200, 0.01);
        assert!(metrics.render().contains(r#"route="/a\"b\\c""#));
    }
}
