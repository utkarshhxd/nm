// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::Method;
use common::TestApp;
use metrics_exporter_prometheus::PrometheusBuilder;
use sqlx::PgPool;

// The recorder is process-global, so this file holds exactly one test.
#[sqlx::test]
async fn labels_are_route_templates_and_bounded_methods(pool: PgPool) {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::set_global_recorder(recorder).expect("no other recorder in this binary");
    let app = TestApp::new(pool).await;

    app.server.get("/healthz").await.assert_status_ok();
    // An unknown API path is a 404 whatever `web/dist` holds, so its status can be asserted.
    app.server
        .get("/api/v1/nope")
        .await
        .assert_status_not_found();
    // The SPA fallback has no `MatchedPath`; its status depends on the build artifact, so
    // this request is here only to show the raw URI never becomes a label.
    app.server.get("/no/such/route").await;
    for m in ["FOO1", "FOO2"] {
        app.server
            .method(Method::from_bytes(m.as_bytes()).unwrap(), "/healthz")
            .await;
    }

    let out = handle.render();
    let counters: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with("http_requests_total{"))
        .collect();
    let has = |needle: &str| counters.iter().any(|l| l.contains(needle));
    assert!(
        has(r#"method="GET",route="/healthz",status="200""#),
        "{out}"
    );
    assert!(
        has(r#"method="GET",route="/api/{*rest}",status="404""#),
        "{out}"
    );
    assert!(has(r#"method="GET",route="unmatched""#), "{out}");
    assert!(has(r#"method="OTHER""#), "{out}");
    assert!(
        !out.contains("FOO"),
        "raw method leaked into labels:\n{out}"
    );
}
