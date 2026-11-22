// SPDX-License-Identifier: AGPL-3.0-only

use sqlx::{PgPool, postgres::PgPoolOptions};

use crate::config::Config;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// The API's pool. Twenty, and every instance-wide cap in `app::AppState` is sized against that
/// number.
pub async fn connect(cfg: &Config) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(20)
        .connect(&cfg.database_url)
        .await
}

/// The worker's pool, which is **its own and not the one above**.
///
/// `/readyz` reads an exhausted pool as not ready (`http::health`), and a tick has
/// `worker_concurrency` sends in flight plus the row each one records. A worker drawing on the
/// API's twenty would therefore make *sending* able to take the product down — and in `nm all`
/// the two run in one process, so it would be the same pool by default. `tests/worker_pool.rs`
/// saturates this one and asks the API's for a connection.
pub async fn connect_worker(cfg: &Config) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(cfg.worker_pool_size)
        .connect(&cfg.database_url)
        .await
}
