// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use std::net::SocketAddr;

use axum::{Router, extract::ConnectInfo, routing::get};

/// Pins the harness property that handlers see `ConnectInfo`, as in production.
#[tokio::test]
async fn harness_supplies_connect_info() {
    let router = Router::new().route(
        "/ip",
        get(|ConnectInfo(addr): ConnectInfo<SocketAddr>| async move { addr.ip().to_string() }),
    );
    let r = common::test_server(router).get("/ip").await;
    r.assert_status_ok();
    r.assert_text("127.0.0.1");
}
