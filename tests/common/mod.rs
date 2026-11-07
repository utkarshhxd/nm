// SPDX-License-Identifier: AGPL-3.0-only

// Each test binary uses a different subset of the harness.
#![allow(dead_code)]

use std::net::SocketAddr;

use axum::Router;
use axum_test::{TestRequest, TestServer};
use nm::{
    app::{self, AppState},
    config::{Config, LogFormat, SecretKey},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

pub fn test_config(database_url: &str) -> Config {
    Config {
        database_url: database_url.to_owned(),
        listen: "127.0.0.1:0".parse().unwrap(),
        public_url: "http://localhost:8080".parse().unwrap(),
        secret_key: SecretKey([7u8; 32]),
        auto_migrate: false,
        trust_proxy_headers: false,
        metrics_listen: None,
        log_format: LogFormat::Pretty,
    }
}

pub struct TestApp {
    pub server: TestServer,
    pub pool: PgPool,
    pub state: AppState,
    pub csrf: Option<String>,
}

impl TestApp {
    pub async fn new(pool: PgPool) -> TestApp {
        TestApp::with_config(pool, |_| {}).await
    }

    pub async fn with_config(pool: PgPool, f: impl FnOnce(&mut Config)) -> TestApp {
        // `#[sqlx::test]` pools carry no URL; the config's URL is unused by the router.
        let mut config = test_config("postgres://unused");
        f(&mut config);
        let state = AppState::new(pool.clone(), config).expect("app state");
        let server = test_server(app::router(state.clone()));
        TestApp {
            server,
            pool,
            state,
            csrf: None,
        }
    }
}

/// Serves `router` the way `app::serve` does, with `ConnectInfo<SocketAddr>`.
/// axum-test must use real HTTP for that, so handlers see the client address
/// 127.0.0.1 (the port varies per connection). Cookies are kept between requests.
pub fn test_server(router: Router) -> TestServer {
    TestServer::builder()
        .save_cookies()
        .build(router.into_make_service_with_connect_info::<SocketAddr>())
}

pub const PW: &str = "correct horse battery";

pub async fn create_user(pool: &PgPool, email: &str, password: &str, instance_admin: bool) -> Uuid {
    nm::users::create_user(pool, email, password, instance_admin)
        .await
        .expect("create user")
}

impl TestApp {
    /// Signs in, then reads the CSRF token from `/me`; later `get`/`post`/... send it.
    pub async fn login(&mut self, email: &str, password: &str) {
        let r = self
            .server
            .post("/api/v1/auth/login")
            .json(&json!({"email": email, "password": password}))
            .await;
        r.assert_status_ok();
        let me = self.server.get("/api/v1/me").await;
        me.assert_status_ok();
        self.csrf = Some(
            me.json::<Value>()["csrf_token"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }

    fn with_csrf(&self, req: TestRequest) -> TestRequest {
        match &self.csrf {
            Some(t) => req.add_header("x-csrf-token", t.as_str()),
            None => req,
        }
    }

    pub fn get(&self, p: &str) -> TestRequest {
        self.with_csrf(self.server.get(p))
    }
    pub fn post(&self, p: &str) -> TestRequest {
        self.with_csrf(self.server.post(p))
    }
    pub fn patch(&self, p: &str) -> TestRequest {
        self.with_csrf(self.server.patch(p))
    }
    pub fn delete(&self, p: &str) -> TestRequest {
        self.with_csrf(self.server.delete(p))
    }
}
