// SPDX-License-Identifier: AGPL-3.0-only

use std::{convert::Infallible, time::Instant};

use axum::{
    extract::{MatchedPath, Request},
    http::Method,
    middleware::{self, Next},
    response::Response,
    routing::Route,
};
use tower::{Layer, Service};
use tracing_subscriber::EnvFilter;

use crate::config::LogFormat;

/// Filter comes from `NM_LOG` (same syntax as `RUST_LOG`), default `info`.
/// A set but malformed `NM_LOG` panics at startup rather than being ignored.
pub fn init_logging(format: LogFormat) {
    let spec = match std::env::var("NM_LOG") {
        Ok(s) => Some(s),
        Err(std::env::VarError::NotPresent) => None,
        Err(e) => panic!("NM_LOG is not readable: {e}"),
    };
    let builder = tracing_subscriber::fmt().with_env_filter(env_filter(spec.as_deref()));
    match format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Pretty => builder.pretty().init(),
    }
}

fn env_filter(spec: Option<&str>) -> EnvFilter {
    match spec {
        None => EnvFilter::new("info"),
        Some(s) => EnvFilter::try_new(s).unwrap_or_else(|e| panic!("invalid NM_LOG {s:?}: {e}")),
    }
}

/// Records `http_requests_total` and `http_request_duration_seconds`, labelled by
/// method, matched route template (never the raw path, to bound cardinality) and
/// status. Unrouted requests (404s) are labelled `route="unmatched"`.
pub fn metrics_layer() -> impl Layer<
    Route,
    Service = impl Service<Request, Response = Response, Error = Infallible, Future: Send>
              + Clone
              + Send
              + Sync
              + 'static,
> + Clone
+ Send
+ Sync
+ 'static {
    middleware::from_fn(track_metrics)
}

/// Fixed label set: hyper accepts any token as a method, so a raw value would let
/// an unauthenticated client mint unbounded metric series.
fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    }
}

async fn track_metrics(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = method_label(req.method());
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str)
        .to_owned();
    let response = next.run(req).await;
    let status = response.status().as_u16().to_string();
    metrics::counter!(
        "http_requests_total",
        "method" => method, "route" => route.clone(), "status" => status.clone()
    )
    .increment(1);
    metrics::histogram!(
        "http_request_duration_seconds",
        "method" => method, "route" => route, "status" => status
    )
    .record(start.elapsed().as_secs_f64());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_spec_defaults_to_info() {
        assert_eq!(env_filter(None).to_string(), "info");
    }

    #[test]
    fn valid_spec_is_used() {
        assert_eq!(env_filter(Some("nm=debug")).to_string(), "nm=debug");
    }

    #[test]
    #[should_panic(expected = "invalid NM_LOG")]
    fn malformed_spec_panics() {
        env_filter(Some("nm=notalevel"));
    }
}
