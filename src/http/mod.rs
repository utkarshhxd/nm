// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Router, routing::any};

use crate::{app::AppState, error::ApiError};

mod health;
pub mod security_headers;

/// Every route module adds its `.merge(...)` here.
pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(health::routes())
        .merge(crate::openapi::docs())
        // Catch-all for unknown `/api/*` paths. A route rather than `Router::fallback`
        // so route modules may still be merged freely (only one fallback is allowed),
        // and metrics see the bounded template `/api/{*rest}`, never the raw path.
        .route("/api/{*rest}", any(api_not_found))
}

async fn api_not_found() -> ApiError {
    ApiError::NotFound
}
