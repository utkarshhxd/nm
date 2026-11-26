// SPDX-License-Identifier: AGPL-3.0-only

// Each test binary uses a different subset of the harness.
#![allow(dead_code)]

/// An in-process SMTP server, for `tests/relay_transport.rs`.
pub mod smtp_sink;

use std::net::SocketAddr;

use axum::Router;
use axum_test::{TestRequest, TestServer};
use nm::{
    app::{self, AppState},
    config::{Config, LogFormat, SecretKey},
    http::request_meta::RequestMeta,
    workspaces::roles::Role,
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
        preview_timeout_ms: 1500,
        worker_pool_size: 5,
        worker_concurrency: 4,
        worker_campaign_concurrency: 32,
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
        TestApp::from_state(pool, state)
    }

    /// An app whose DNS checker asks `resolver` and not the host's nameservers.
    pub async fn with_resolver(
        pool: PgPool,
        resolver: nm::sending_domains::dns::Resolver,
    ) -> TestApp {
        let mut state =
            AppState::new(pool.clone(), test_config("postgres://unused")).expect("app state");
        state.resolver = std::sync::Arc::new(resolver);
        TestApp::from_state(pool, state)
    }

    fn from_state(pool: PgPool, state: AppState) -> TestApp {
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
    nm::users::create_user(pool, email, password, None, instance_admin)
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
        // A 200 can also be `mfa_required`, after which `/me` would only answer 401.
        r.assert_json(&json!({"status":"ok"}));
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
    pub fn put(&self, p: &str) -> TestRequest {
        self.with_csrf(self.server.put(p))
    }
    pub fn patch(&self, p: &str) -> TestRequest {
        self.with_csrf(self.server.patch(p))
    }
    pub fn delete(&self, p: &str) -> TestRequest {
        self.with_csrf(self.server.delete(p))
    }
}

/// Creates a workspace through the library (so it is audited like a real one), owned by `owner`.
pub async fn create_workspace(pool: &PgPool, owner: Uuid, slug: &str) -> Uuid {
    nm::workspaces::create_workspace(
        pool,
        &RequestMeta::default(),
        owner,
        nm::workspaces::NewWorkspace {
            slug: slug.to_owned(),
            name: slug.to_owned(),
            timezone: None,
        },
    )
    .await
    .expect("create workspace")
    .id
}

/// Adds a membership directly, without an audit row.
pub async fn add_member(pool: &PgPool, ws: Uuid, user: Uuid, role: Role) {
    sqlx::query("insert into memberships (workspace_id, user_id, role) values ($1, $2, $3)")
        .bind(ws)
        .bind(user)
        .bind(role)
        .execute(pool)
        .await
        .expect("add member");
}

/// Mints an API key of `role` as the signed-in `owner`.
pub async fn api_key(owner: &TestApp, ws: Uuid, role: &str) -> String {
    let r = owner
        .post(&format!("/api/v1/workspaces/{ws}/api-keys"))
        .json(&json!({"name": "ci", "role": role}))
        .await;
    r.assert_status(axum::http::StatusCode::CREATED);
    r.json::<Value>()["key"].as_str().unwrap().to_owned()
}

/// A bearer-authenticated POST straight into the router. There is no HTTP client in between, so a
/// streamed body reaches the caller as the server produces it, instead of being collected first
/// the way `axum-test` collects every response.
pub async fn post_raw(
    state: &AppState,
    key: &str,
    uri: &str,
    body: &Value,
) -> axum::response::Response {
    post_router(&app::router(state.clone()), key, uri, body).await
}

/// `post_raw` against a router built earlier. `app::router` merges the OpenAPI document and the
/// Swagger UI and allocates about 3.8 MB doing it, so a test that measures heap builds it once,
/// before its baseline.
pub async fn post_router(
    router: &Router,
    key: &str,
    uri: &str,
    body: &Value,
) -> axum::response::Response {
    use tower::ServiceExt;
    let req = axum::http::Request::post(uri)
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    router.clone().oneshot(req).await.unwrap()
}
