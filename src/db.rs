// SPDX-License-Identifier: AGPL-3.0-only

use sqlx::{PgPool, postgres::PgPoolOptions};

use crate::config::Config;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub async fn connect(cfg: &Config) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(20)
        .connect(&cfg.database_url)
        .await
}
