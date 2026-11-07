// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::{Router, http::StatusCode, routing::post};
use common::{TestApp, test_server};
use nm::error::{ApiJson, ApiResult};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;

#[sqlx::test]
async fn unknown_api_route_is_problem_404(pool: PgPool) {
    let r = TestApp::new(pool).await.server.get("/api/v1/nope").await;
    r.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(r.header("content-type"), "application/problem+json");
    assert_eq!(r.json::<Value>()["type"], "urn:nm:problem:not-found");
}

#[sqlx::test]
async fn unknown_non_api_route_stays_plain_404(pool: PgPool) {
    let r = TestApp::new(pool).await.server.get("/nope").await;
    r.assert_status(StatusCode::NOT_FOUND);
    assert!(r.headers().get("content-type").is_none());
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

#[sqlx::test]
async fn json_rejections_are_problems(pool: PgPool) {
    let _ = pool;
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
