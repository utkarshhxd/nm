// SPDX-License-Identifier: AGPL-3.0-only

use std::time::Duration;

use axum::{Json, Router, extract::State, routing::get};
use serde_json::{Value, json};

use crate::{
    app::AppState,
    error::{ApiError, ApiResult},
};

const READY_TIMEOUT: Duration = Duration::from_secs(2);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
}

/// Liveness: the process is up. Deliberately touches no dependency.
async fn healthz() -> &'static str {
    "ok"
}

/// Readiness: Postgres answers within 2 seconds. The timeout covers acquiring a
/// connection as well as the query, so an exhausted pool also reads as not ready.
///
/// The 200 body stays `{"database":"ok"}` — `web/e2e/support/server.ts` reads it to confirm the
/// server it reached is ours. The failures are RFC 9457, like every other error body.
async fn readyz(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let check = sqlx::query_scalar!(r#"SELECT 1 AS "ok!""#).fetch_one(&state.pool);
    match tokio::time::timeout(READY_TIMEOUT, check).await {
        Ok(Ok(_)) => Ok(Json(json!({"database": "ok"}))),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "readiness check failed");
            Err(ApiError::ServiceUnavailable("database-unavailable"))
        }
        Err(_) => {
            tracing::warn!("readiness check timed out");
            Err(ApiError::ServiceUnavailable("database-timeout"))
        }
    }
}
