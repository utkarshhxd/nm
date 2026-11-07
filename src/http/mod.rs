// SPDX-License-Identifier: AGPL-3.0-only

use axum::Router;

use crate::app::AppState;

mod health;

/// Every route module adds its `.merge(...)` here.
pub fn routes() -> Router<AppState> {
    Router::new().merge(health::routes())
}
