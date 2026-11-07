// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test]
async fn healthz_ok(pool: PgPool) {
    TestApp::new(pool)
        .await
        .server
        .get("/healthz")
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn readyz_reports_database(pool: PgPool) {
    let r = TestApp::new(pool).await.server.get("/readyz").await;
    r.assert_status_ok();
    r.assert_json(&json!({"database":"ok"}));
}

#[sqlx::test]
async fn readyz_503_when_database_down(pool: PgPool) {
    let app = TestApp::new(pool.clone()).await;
    pool.close().await;
    let r = app.server.get("/readyz").await;
    r.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    // Also pins that no Postgres error text reaches the body.
    r.assert_json(&json!({"database": "unavailable"}));
}
