// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::{
    Router,
    http::StatusCode,
    routing::{get, post},
};
use common::{TestApp, test_server};
use nm::{
    error::{ApiJson, ApiPath, ApiQuery, ApiResult},
    http::web_ui::{AssetSource, EmbeddedAssets},
    pagination::PageParams,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test]
async fn unknown_api_route_is_problem_404(pool: PgPool) {
    let r = TestApp::new(pool).await.server.get("/api/v1/nope").await;
    r.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(r.header("content-type"), "application/problem+json");
    assert_eq!(r.json::<Value>()["type"], "urn:nm:problem:not-found");
}

/// The branch is on the embedded build, not on the response: `web/dist` is a build artifact
/// that the `rust` CI job never produces, so both arms are reachable and each is exact.
#[sqlx::test]
async fn unknown_non_api_route_falls_back_to_spa(pool: PgPool) {
    let r = TestApp::new(pool).await.server.get("/nope").await;
    if EmbeddedAssets.get("index.html").is_some() {
        r.assert_status_ok();
        assert!(
            r.header("content-type")
                .to_str()
                .unwrap()
                .starts_with("text/html"),
            "{:?}",
            r.header("content-type")
        );
        assert_eq!(r.header("cache-control"), "no-cache");
    } else {
        r.assert_status(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(r.text(), "web UI not built; run pnpm --dir web build");
    }
}

/// Bare `/api` is not under `/api/`, so the wildcard 404 route misses it and it reaches the
/// SPA fallback. It must still be a problem+json 404, not `index.html` with a 200.
#[sqlx::test]
async fn api_paths_never_fall_back_to_index(pool: PgPool) {
    let app = TestApp::new(pool).await;
    for path in ["/api", "/api/", "/api/v1/nope", "/api/v1/nope/deeper"] {
        let r = app.server.get(path).await;
        r.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(
            r.header("content-type"),
            "application/problem+json",
            "{path}"
        );
        assert_eq!(
            r.json::<Value>()["type"],
            "urn:nm:problem:not-found",
            "{path}"
        );
    }
}

#[sqlx::test]
async fn security_headers_everywhere(pool: PgPool) {
    let app = TestApp::new(pool).await;
    for path in [
        "/healthz",
        "/readyz",
        "/nope",
        "/api/v1/nope",
        "/api/v1/openapi.json",
    ] {
        let r = app.server.get(path).await;
        assert_eq!(r.header("x-frame-options"), "DENY", "{path}");
        assert_eq!(r.header("x-content-type-options"), "nosniff", "{path}");
        assert_eq!(
            r.header("referrer-policy"),
            "strict-origin-when-cross-origin",
            "{path}"
        );
        assert_eq!(
            r.header("content-security-policy"),
            "default-src 'self'; img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; \
             frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
            "{path}"
        );
    }
}

#[sqlx::test]
async fn openapi_served(pool: PgPool) {
    let r = TestApp::new(pool)
        .await
        .server
        .get("/api/v1/openapi.json")
        .await;
    r.assert_status_ok();
    let json = r.json::<Value>();
    assert!(json["openapi"].as_str().unwrap().starts_with("3.1"));
    assert_eq!(json["info"]["title"], "NM API");
    assert_eq!(json["info"]["version"], env!("CARGO_PKG_VERSION"));
}

/// Swagger UI is served from the binary and references nothing off-origin.
#[sqlx::test]
async fn swagger_ui_is_vendored_and_self_contained(pool: PgPool) {
    let app = TestApp::new(pool).await;
    let index = app.server.get("/api/v1/docs/").await;
    index.assert_status_ok();
    let html = index.text();
    assert!(html.contains("swagger-ui"), "index is Swagger UI");
    let initializer = app.server.get("/api/v1/docs/swagger-initializer.js").await;
    initializer.assert_status_ok();
    let init_js = initializer.text();
    assert!(init_js.contains("/api/v1/openapi.json"));
    assert!(
        init_js.contains(r#""validatorUrl":"none""#)
            || init_js.contains(r#""validatorUrl": "none""#)
    );
    for (name, body) in [("index", &html), ("initializer", &init_js)] {
        assert!(!body.contains("unpkg.com"), "{name}");
        assert!(!body.contains("jsdelivr"), "{name}");
        assert!(!body.contains("https://"), "{name}");
    }
    app.server
        .get("/api/v1/docs/swagger-ui-bundle.js")
        .await
        .assert_status_ok();
    // The global policy must not be loosened for the docs page.
    assert!(
        !index
            .header("content-security-policy")
            .to_str()
            .unwrap()
            .contains("unsafe-eval")
    );
}

#[derive(Deserialize)]
struct Body {
    #[allow(dead_code)]
    name: String,
}

async fn echo(ApiJson(_): ApiJson<Body>) -> ApiResult<&'static str> {
    Ok("ok")
}

#[tokio::test]
async fn json_rejections_are_problems() {
    let server = test_server(Router::new().route("/echo", post(echo)));

    let r = server
        .post("/echo")
        .text(r#"{"name":"x"}"#)
        .content_type("text/plain")
        .await;
    r.assert_status(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(r.header("content-type"), "application/problem+json");
    assert_eq!(
        r.json::<Value>()["type"],
        "urn:nm:problem:unsupported-media-type"
    );

    let r = server.post("/echo").json(&json!({"name": 1})).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json::<Value>()["type"], "urn:nm:problem:validation");

    let r = server
        .post("/echo")
        .text("{not json")
        .content_type("application/json")
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json::<Value>()["type"], "urn:nm:problem:validation");

    server
        .post("/echo")
        .json(&json!({"name": "ok"}))
        .await
        .assert_status_ok();
}

async fn list(ApiQuery(p): ApiQuery<PageParams>) -> ApiResult<String> {
    Ok(p.limit().to_string())
}

async fn one(ApiPath(_): ApiPath<Uuid>) -> ApiResult<&'static str> {
    Ok("ok")
}

#[tokio::test]
async fn query_and_path_rejections_are_problems() {
    let server = test_server(
        Router::new()
            .route("/list", get(list))
            .route("/one/{id}", get(one)),
    );

    for bad in ["abc", "-1"] {
        let r = server.get(&format!("/list?limit={bad}")).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(r.header("content-type"), "application/problem+json");
        let v = r.json::<Value>();
        assert_eq!(v["type"], "urn:nm:problem:validation");
        assert_eq!(v["errors"][0]["field"], "query");
        assert!(
            !r.text().contains(bad),
            "request content must not be echoed"
        );
    }
    server.get("/list?limit=7").await.assert_text("7");

    let r = server.get("/one/not-a-uuid").await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.header("content-type"), "application/problem+json");
    let v = r.json::<Value>();
    assert_eq!(v["type"], "urn:nm:problem:validation");
    assert!(!r.text().contains("not-a-uuid"));

    server
        .get(&format!("/one/{}", Uuid::now_v7()))
        .await
        .assert_status_ok();
}
