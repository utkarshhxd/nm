// SPDX-License-Identifier: AGPL-3.0-only

//! The subscriber's side of the product: the pages an email link leads to. Every route here is
//! unauthenticated and authenticated by its own HMAC token instead, is CSRF-exempt by
//! construction (`http::csrf` gates `/api/v1/` only), is rate-limited by client IP, and renders
//! server-side HTML that works with JavaScript disabled.

use axum::Router;

use crate::app::AppState;

pub mod confirm;
pub mod page;
pub mod token;

/// Merged into `http::routes()`, where axum matches it ahead of the SPA fallback.
pub fn routes() -> Router<AppState> {
    Router::new().merge(confirm::routes())
}
