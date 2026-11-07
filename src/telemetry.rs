// SPDX-License-Identifier: AGPL-3.0-only

use std::{convert::Infallible, time::Instant};

use axum::{
    extract::{MatchedPath, Request},
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

async fn track_metrics(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().to_string();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str)
        .to_owned();
    let response = next.run(req).await;
    let labels = [
        ("method", method),
        ("route", route),
        ("status", response.status().as_u16().to_string()),
    ];
    metrics::counter!("http_requests_total", &labels).increment(1);
    metrics::histogram!("http_request_duration_seconds", &labels)
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
