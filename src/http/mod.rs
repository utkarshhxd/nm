// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Router, routing::any};

use crate::{app::AppState, error::ApiError};

pub mod csrf;
mod health;
pub mod rate_limit;
pub mod request_meta;
pub mod security_headers;
pub mod web_ui;

/// Every route module adds its `.merge(...)` here.
pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(health::routes())
        .merge(crate::auth::routes())
        .merge(crate::workspaces::routes())
        .merge(crate::custom_fields::routes::routes())
        .merge(crate::contacts::routes::routes())
        .merge(crate::contacts::import::routes::routes())
        .merge(crate::lists::routes::routes())
        .merge(crate::forms::routes::routes())
        .merge(crate::segments::routes::routes())
        .merge(crate::tags::routes::routes())
        .merge(crate::suppressions::routes::routes())
        .merge(crate::relay::routes::routes())
        .merge(crate::sending_domains::routes::routes())
        .merge(crate::template::routes::routes())
        .merge(crate::brand::routes::routes())
        .merge(crate::audit::routes::routes())
        .merge(crate::consent::routes::routes())
        // The subscriber's pages, at top-level paths and with no session. Merged here like every
        // other module, which puts them inside the CSRF layer `app::router` wraps around this —
        // `http::csrf` gates `/api/v1/` only, so they are exempt by construction and need no
        // exception. axum matches a declared route before `fallback`, so the SPA below does not
        // shadow them.
        .merge(crate::public::routes())
        .merge(crate::openapi::docs())
        // Catch-all for unknown `/api/*` paths. A route rather than `Router::fallback`
        // so route modules may still be merged freely (only one fallback is allowed),
        // and metrics see the bounded template `/api/{*rest}`, never the raw path.
        .route("/api/{*rest}", any(api_not_found))
        // The embedded SPA answers whatever the routes above did not claim. Set here, inside
        // `routes()`, so it is covered by the CSRF layer `app::router` wraps around this.
        .fallback(web_ui::fallback)
}

async fn api_not_found() -> ApiError {
    ApiError::NotFound
}
