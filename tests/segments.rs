// SPDX-License-Identifier: AGPL-3.0-only

//! Segment CRUD (spec M1b sections 7, 12, 13 and 19), asserted against real rows. Every test that
//! claims a refusal reads the table afterwards: a status code alone cannot tell "refused" from
//! "refused after writing".

mod common;

use std::{future::IntoFuture, time::Duration};

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::{
    config::Config,
    error::ApiError,
    http::rate_limit::PREVIEW_PER_MINUTE,
    segments::{compile::push_rules, load_rules, parse_new_rules},
    workspaces::roles::Role,
};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder, postgres::PgPoolOptions};
use uuid::Uuid;

async fn client(pool: &PgPool, email: &str, ws: Uuid, role: Role) -> TestApp {
    client_with(pool, email, ws, role, |_| {}).await
}

/// `client`, with the instance's configuration adjusted. The app carries its own `AppState`, so
/// two clients built here do not share a rate-limit window; tests about the limiter use one.
async fn client_with(
    pool: &PgPool,
    email: &str,
    ws: Uuid,
    role: Role,
    f: impl FnOnce(&mut Config),
) -> TestApp {
    let id = create_user(pool, email, PW, false).await;
    add_member(pool, ws, id, role).await;
    let mut app = TestApp::with_config(pool.clone(), f).await;
    app.login(email, PW).await;
    app
}

/// A workspace and a signed-in admin of it. An admin may do everything an editor may, and may
/// also define and purge custom fields, which several tests here need.
async fn workspace(pool: &PgPool, slug: &str) -> (Uuid, TestApp) {
    workspace_with(pool, slug, |_| {}).await
}

async fn workspace_with(pool: &PgPool, slug: &str, f: impl FnOnce(&mut Config)) -> (Uuid, TestApp) {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let ws = create_workspace(pool, owner, slug).await;
    let admin = client_with(
        pool,
        &format!("admin-{slug}@example.com"),
        ws,
        Role::Admin,
        f,
    )
    .await;
    (ws, admin)
}

fn segments(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/segments")
}

fn fields(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/custom-fields")
}

/// A one-rule document over a column.
fn first_name_is(name: &str) -> Value {
    json!({"match": "all", "rules": [{"field": "first_name", "op": "eq", "value": name}]})
}

/// A one-rule document over the custom field `plan`.
fn plan_is(value: &str) -> Value {
    json!({"match": "all", "rules": [{"field": "attr:plan", "op": "eq", "value": value}]})
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

async fn create(app: &TestApp, ws: Uuid, name: &str, rules: Value) -> Uuid {
    let r = app
        .post(&segments(ws))
        .json(&json!({"name": name, "rules": rules}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn create_field(app: &TestApp, ws: Uuid, key: &str) -> Uuid {
    create_field_of(app, ws, key, "text").await
}

async fn create_field_of(app: &TestApp, ws: Uuid, key: &str, kind: &str) -> Uuid {
    let r = app
        .post(&fields(ws))
        .json(&json!({"key": key, "label": key, "kind": kind}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

/// `(name, description, rules, version)` straight from the table, or `None` once it is gone.
async fn row(pool: &PgPool, id: Uuid) -> Option<(String, String, Value, i32)> {
    sqlx::query_as("select name, description, rules, version from segments where id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from segments")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn create_then_read_round_trips_and_stores_the_validated_document(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let r = app
        .post(&segments(ws))
        .json(&json!({"name": "  Ann  ", "description": "d", "rules": first_name_is("Ann")}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let created: Value = r.json();
    let id = id_of(&r);
    assert_eq!(created["name"], "Ann", "the name is trimmed");
    assert_eq!(created["version"], 1);
    assert_eq!(created["rules"], first_name_is("Ann"));
    assert!(
        created["created_by"].is_string(),
        "a user's write is attributed"
    );
    // UUIDv7: version nibble 7.
    assert_eq!(id.get_version_num(), 7);

    let got = app.get(&format!("{}/{id}", segments(ws))).await;
    got.assert_status_ok();
    assert_eq!(got.json::<Value>(), created);
    let stored = row(&pool, id).await.unwrap();
    assert_eq!(stored.2, first_name_is("Ann"));
}

#[sqlx::test]
async fn list_is_cursor_paginated_newest_first_and_per_workspace(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (ws2, app2) = workspace(&pool, "globex").await;
    let a = create(&app, ws, "a", first_name_is("a")).await;
    let b = create(&app, ws, "b", first_name_is("b")).await;
    let c = create(&app, ws, "c", first_name_is("c")).await;
    create(&app2, ws2, "theirs", first_name_is("t")).await;

    let r = app.get(&format!("{}?limit=2", segments(ws))).await;
    r.assert_status_ok();
    let page1: Value = r.json();
    let ids: Vec<&str> = page1["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [c.to_string(), b.to_string()]);
    let cursor = page1["next_cursor"].as_str().expect("a second page exists");

    let r = app
        .get(&format!("{}?limit=2&cursor={cursor}", segments(ws)))
        .await;
    let page2: Value = r.json();
    let ids: Vec<&str> = page2["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [a.to_string()],
        "no row repeats and none is another workspace's"
    );
    assert!(page2["next_cursor"].is_null());

    app.get(&format!("{}?cursor=nope", segments(ws)))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test]
async fn a_viewer_reads_and_an_editor_writes(pool: PgPool) {
    let (ws, admin) = workspace(&pool, "acme").await;
    let viewer = client(&pool, "viewer@example.com", ws, Role::Viewer).await;
    let editor = client(&pool, "editor@example.com", ws, Role::Editor).await;
    let id = create(&admin, ws, "keep", first_name_is("a")).await;
    let one = format!("{}/{id}", segments(ws));

    // A viewer reads.
    viewer.get(&segments(ws)).await.assert_status_ok();
    viewer.get(&one).await.assert_status_ok();

    // A viewer writes nothing, and nothing is written.
    let r = viewer
        .post(&segments(ws))
        .json(&json!({"name": "x", "rules": first_name_is("x")}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(problem_type(&r), "urn:nm:problem:insufficient-role");
    viewer
        .patch(&one)
        .json(&json!({"name": "renamed"}))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    viewer
        .delete(&one)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    assert_eq!(count(&pool).await, 1);
    assert_eq!(row(&pool, id).await.unwrap().0, "keep");

    // An editor writes.
    let made = create(&editor, ws, "by editor", first_name_is("e")).await;
    editor
        .patch(&format!("{}/{made}", segments(ws)))
        .json(&json!({"description": "x"}))
        .await
        .assert_status_ok();
    editor
        .delete(&format!("{}/{made}", segments(ws)))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(row(&pool, made).await.is_none());
}

/// 404, never 403: a caller who cannot see the workspace must not learn that it exists, and a
/// segment of another workspace must be indistinguishable from one that is not there. The data is
/// read afterwards, because a handler that wrote before checking would still answer 404.
#[sqlx::test]
async fn another_workspaces_segment_is_a_404_on_every_verb_and_is_untouched(pool: PgPool) {
    let (ws1, admin1) = workspace(&pool, "acme").await;
    let (ws2, admin2) = workspace(&pool, "globex").await;
    // A role that could write if the workspace were its own.
    let outsider = client(&pool, "outsider@example.com", ws2, Role::Editor).await;
    let theirs = create(&admin1, ws1, "theirs", first_name_is("a")).await;
    let before = row(&pool, theirs).await.unwrap();

    // 1. Their id through MY workspace's path: I am a member of ws2, the id belongs to ws1.
    let mine = format!("{}/{theirs}", segments(ws2));
    for r in [
        admin2.get(&mine).await,
        admin2.patch(&mine).json(&json!({"name": "hijacked"})).await,
        admin2.delete(&mine).await,
    ] {
        r.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(problem_type(&r), "urn:nm:problem:not-found");
    }
    // 2. Their workspace's path, by someone who is not a member of it.
    let theirs_path = format!("{}/{theirs}", segments(ws1));
    for r in [
        outsider.get(&segments(ws1)).await,
        outsider.get(&theirs_path).await,
        outsider
            .post(&segments(ws1))
            .json(&json!({"name": "in", "rules": first_name_is("x")}))
            .await,
        outsider
            .patch(&theirs_path)
            .json(&json!({"name": "hijacked"}))
            .await,
        outsider.delete(&theirs_path).await,
    ] {
        r.assert_status(StatusCode::NOT_FOUND);
    }
    assert_eq!(row(&pool, theirs).await.unwrap(), before);
    assert_eq!(count(&pool).await, 1);

    // The same refusal for a viewer of the other workspace, who must not see a 403 either.
    let viewer2 = client(&pool, "viewer2@example.com", ws2, Role::Viewer).await;
    viewer2
        .delete(&theirs_path)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn names_are_unique_per_workspace_and_a_clash_is_409(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (ws2, app2) = workspace(&pool, "globex").await;
    let first = create(&app, ws, "dup", first_name_is("a")).await;
    let other = create(&app, ws, "other", first_name_is("b")).await;

    let r = app
        .post(&segments(ws))
        .json(&json!({"name": "dup", "rules": first_name_is("c")}))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:segment-name-taken");
    let r = app
        .patch(&format!("{}/{other}", segments(ws)))
        .json(&json!({"name": "dup"}))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:segment-name-taken");
    assert_eq!(row(&pool, other).await.unwrap().0, "other");
    assert_eq!(row(&pool, first).await.unwrap().0, "dup");

    // Another workspace may reuse the name.
    create(&app2, ws2, "dup", first_name_is("a")).await;
}

#[sqlx::test]
async fn bodies_reject_unknown_fields_and_bad_names(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let id = create(&app, ws, "keep", first_name_is("a")).await;
    let one = format!("{}/{id}", segments(ws));

    for body in [
        json!({"name": "x", "rules": first_name_is("a"), "version": 9}),
        json!({"name": "x", "rules": first_name_is("a"), "id": Uuid::nil()}),
    ] {
        app.post(&segments(ws))
            .json(&body)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
    for body in [
        json!({"version": 9}),
        json!({"name": "x", "workspace_id": ws}),
    ] {
        app.patch(&one)
            .json(&body)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
    for name in ["", "   ", "a\nb"] {
        let r = app
            .post(&segments(ws))
            .json(&json!({"name": name, "rules": first_name_is("a")}))
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(problem_type(&r), "urn:nm:problem:validation");
    }
    // A name has nothing to be cleared to.
    let r = app.patch(&one).json(&json!({"name": null})).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count(&pool).await, 1);
    assert_eq!(row(&pool, id).await.unwrap().0, "keep");
}

#[sqlx::test]
async fn a_refused_rule_document_is_422_with_its_own_code_and_writes_nothing(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    for (doc, code) in [
        (json!({"match": "all", "rules": []}), "empty-rules"),
        (
            json!({"match": "all", "rules": [{"field": "emial", "op": "eq", "value": "a"}]}),
            "unknown-field",
        ),
        (
            json!({"match": "all", "rules": [{"field": "attr:nope", "op": "eq", "value": "a"}]}),
            "unknown-field",
        ),
        (json!({"match": "all"}), "malformed-rules"),
        (json!("everyone"), "malformed-rules"),
        (json!(null), "malformed-rules"),
    ] {
        let r = app
            .post(&segments(ws))
            .json(&json!({"name": "n", "rules": doc}))
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(problem_type(&r), format!("urn:nm:problem:{code}"), "{doc}");
    }
    assert_eq!(count(&pool).await, 0);
}

/// RFC 7386 would merge `{"match": "any"}` into the stored document: the connective flips and the
/// old rules array stays. Every shape of partial document must be refused instead, and the stored
/// document must be exactly what it was.
#[sqlx::test]
async fn rules_are_replaced_as_a_whole_and_a_partial_document_is_422(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let original = json!({"match": "all", "rules": [
        {"field": "first_name", "op": "eq", "value": "Ann"},
        {"field": "last_name", "op": "eq", "value": "Lee"},
    ]});
    let id = create(&app, ws, "s", original.clone()).await;
    let one = format!("{}/{id}", segments(ws));

    for partial in [
        json!({"match": "any"}),
        json!({"rules": [{"field": "first_name", "op": "eq", "value": "Bob"}]}),
        json!({}),
        json!(null),
        json!([]),
    ] {
        let r = app.patch(&one).json(&json!({"rules": partial})).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            problem_type(&r),
            "urn:nm:problem:malformed-rules",
            "{partial}"
        );
        let stored = row(&pool, id).await.unwrap();
        assert_eq!(stored.2, original, "{partial} changed the stored document");
        assert_eq!(stored.3, 1, "{partial} bumped the version");
    }

    // A whole document replaces the old one outright: nothing of the old array survives.
    let whole =
        json!({"match": "any", "rules": [{"field": "email", "op": "contains", "value": "x"}]});
    let r = app.patch(&one).json(&json!({"rules": whole})).await;
    r.assert_status_ok();
    let stored = row(&pool, id).await.unwrap();
    assert_eq!(stored.2, whole);
    assert_eq!(stored.3, 2);
}

#[sqlx::test]
async fn version_moves_on_a_rule_change_and_not_on_a_rename_or_a_no_op_save(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let doc = json!({"match": "all", "rules": [
        {"field": "first_name", "op": "eq", "value": "Ann"},
        {"field": "email_domain", "op": "eq", "value": "acme.com"},
    ]});
    let id = create(&app, ws, "s", doc.clone()).await;
    let one = format!("{}/{id}", segments(ws));
    let version = || async { row(&pool, id).await.unwrap().3 };
    assert_eq!(version().await, 1);

    // A rename, a description change and clearing it: not a change to what the segment means.
    app.patch(&one)
        .json(&json!({"name": "renamed"}))
        .await
        .assert_status_ok();
    app.patch(&one)
        .json(&json!({"description": "about"}))
        .await
        .assert_status_ok();
    let r = app.patch(&one).json(&json!({"description": null})).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["description"], "");
    assert_eq!(version().await, 1, "a rename bumped the version");

    // Saving the same rules again, with the keys in another order: not a change either.
    let reordered = json!({"rules": [
        {"value": "Ann", "op": "eq", "field": "first_name"},
        {"value": "acme.com", "field": "email_domain", "op": "eq"},
    ], "match": "all"});
    app.patch(&one)
        .json(&json!({"rules": reordered, "name": "again"}))
        .await
        .assert_status_ok();
    app.patch(&one)
        .json(&json!({"rules": doc}))
        .await
        .assert_status_ok();
    assert_eq!(version().await, 1, "a no-op save bumped the version");

    // A real change to the rules does, each time.
    let changed =
        json!({"match": "all", "rules": [{"field": "first_name", "op": "eq", "value": "Bob"}]});
    let r = app.patch(&one).json(&json!({"rules": changed})).await;
    assert_eq!(r.json::<Value>()["version"], 2);
    let r = app
        .patch(&one)
        .json(&json!({"rules": {"match": "any", "rules": changed["rules"]}}))
        .await;
    assert_eq!(
        r.json::<Value>()["version"],
        3,
        "a connective change is a change"
    );
    assert_eq!(version().await, 3);
}

/// Archiving a field must not break a segment that names it, and must not let a new rule name it.
#[sqlx::test]
async fn archiving_a_field_keeps_stored_segments_loadable_but_not_new_rules_savable(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let field = create_field(&app, ws, "plan").await;
    let id = create(&app, ws, "pro", plan_is("pro")).await;
    let one = format!("{}/{id}", segments(ws));
    let mut conn = pool.acquire().await.unwrap();
    load_rules(&mut conn, ws, id)
        .await
        .expect("loads while live");

    app.delete(&format!("{}/{field}", fields(ws)))
        .await
        .assert_status_ok();

    // Reading: still resolves, so the segment still works.
    let group = load_rules(&mut conn, ws, id)
        .await
        .expect("a stored segment still loads once its field is archived");
    assert_eq!(group.rules.len(), 1);
    app.get(&one).await.assert_status_ok();

    // Writing: a new document may not name it, on create, on patch, and in the parser's own entry.
    let r = app
        .post(&segments(ws))
        .json(&json!({"name": "new", "rules": plan_is("free")}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem_type(&r), "urn:nm:problem:unknown-field");
    let r = app
        .patch(&one)
        .json(&json!({"rules": plan_is("free")}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem_type(&r), "urn:nm:problem:unknown-field");
    let err = parse_new_rules(&mut conn, ws, &plan_is("free"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ApiError::UnprocessableDetail {
                code: "unknown-field",
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(count(&pool).await, 1);
    assert_eq!(row(&pool, id).await.unwrap().2, plan_is("pro"));

    // Renaming it is not a write of the rules, so it still works.
    app.patch(&one)
        .json(&json!({"name": "renamed"}))
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn load_rules_refuses_an_unknown_or_another_workspaces_segment(pool: PgPool) {
    let (ws1, admin1) = workspace(&pool, "acme").await;
    let (ws2, _admin2) = workspace(&pool, "globex").await;
    let id = create(&admin1, ws1, "s", first_name_is("a")).await;
    let mut conn = pool.acquire().await.unwrap();
    load_rules(&mut conn, ws1, id).await.expect("its own");
    for (ws, id) in [(ws2, id), (ws1, Uuid::now_v7())] {
        let err = load_rules(&mut conn, ws, id).await.unwrap_err();
        assert!(matches!(err, ApiError::NotFound), "{err:?}");
    }
}

#[sqlx::test]
async fn purge_is_refused_while_a_live_segment_names_the_field_and_lists_it(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (ws2, app2) = workspace(&pool, "globex").await;
    let field = create_field(&app, ws, "plan").await;
    create_field(&app2, ws2, "plan").await;
    let contact = app
        .post(&format!("/api/v1/workspaces/{ws}/contacts"))
        .json(&json!({"email": "a@ex.test", "attributes": {"plan": "pro"}}))
        .await;
    contact.assert_status(StatusCode::CREATED);

    // One segment names it at the top level, one only inside a nested group.
    create(&app, ws, "flat", plan_is("pro")).await;
    let nested = json!({"match": "all", "rules": [
        {"field": "first_name", "op": "eq", "value": "a"},
        {"group": {"match": "any", "rules": [
            {"field": "last_name", "op": "eq", "value": "b"},
            {"group": {"match": "all", "rules": [
                {"field": "attr:plan", "op": "is_set"}]}},
        ]}},
    ]});
    let deep = create(&app, ws, "deep", nested).await;
    // Segments that must NOT block: another key that merely starts the same, the text `attr:plan`
    // as a VALUE of another field, and (below) another workspace's segment on its own `plan`.
    create_field(&app, ws, "plan2").await;
    create(
        &app,
        ws,
        "prefix",
        json!({"match": "all", "rules": [{"field": "attr:plan2", "op": "eq", "value": "x"}]}),
    )
    .await;
    create(&app, ws, "value only", first_name_is("attr:plan")).await;
    create(&app2, ws2, "theirs", plan_is("pro")).await;

    let purge = format!("{}/{field}/purge", fields(ws));
    app.delete(&format!("{}/{field}", fields(ws)))
        .await
        .assert_status_ok();

    let r = app.post(&purge).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:field-in-use");
    let detail = r.json::<Value>()["detail"].as_str().unwrap().to_owned();
    assert!(
        detail.contains("`flat`") && detail.contains("`deep`"),
        "{detail}"
    );
    assert!(
        !detail.contains("and more"),
        "two segments are not a truncated list: {detail}"
    );
    assert!(
        !detail.contains("prefix") && !detail.contains("value only"),
        "{detail}"
    );
    assert!(
        !detail.contains("theirs"),
        "another workspace's segment: {detail}"
    );
    // Refused means nothing was removed.
    let attrs: Value =
        sqlx::query_scalar("select attributes from contacts where email = 'a@ex.test'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attrs, json!({"plan": "pro"}));

    // Still refused for the nested one alone; then free once none is left.
    app.delete(&format!("{}/{}", segments(ws), {
        sqlx::query_scalar::<_, Uuid>("select id from segments where name = 'flat'")
            .fetch_one(&pool)
            .await
            .unwrap()
    }))
    .await
    .assert_status(StatusCode::NO_CONTENT);
    let r = app.post(&purge).await;
    r.assert_status(StatusCode::CONFLICT);
    let detail = r.json::<Value>()["detail"].as_str().unwrap().to_owned();
    assert!(
        detail.contains("`deep`") && !detail.contains("`flat`"),
        "{detail}"
    );
    app.delete(&format!("{}/{deep}", segments(ws)))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let r = app.post(&purge).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["affected"], 1);
}

#[sqlx::test]
async fn writes_are_audited(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let id = create(&app, ws, "s", first_name_is("a")).await;
    let one = format!("{}/{id}", segments(ws));
    app.patch(&one)
        .json(&json!({"description": "d", "rules": first_name_is("b")}))
        .await
        .assert_status_ok();
    app.delete(&one).await.assert_status(StatusCode::NO_CONTENT);
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "select action, metadata from audit_log where workspace_id = $1 and action like 'segment.%' \
         order by id",
    )
    .bind(ws)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("segment.created".to_owned(), json!({"name": "s"})),
            (
                "segment.updated".to_owned(),
                json!({"fields": ["description", "rules"], "version": 2})
            ),
            ("segment.deleted".to_owned(), json!({})),
        ]
    );
}

/// The router merges in `http::routes()`, inside the CSRF layer. A write without the token must be
/// refused, and refused before anything is written.
#[sqlx::test]
async fn segment_writes_are_csrf_protected(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let id = create(&app, ws, "s", first_name_is("a")).await;
    let one = format!("{}/{id}", segments(ws));
    // `app.server` is the bare client: it carries the session cookie but not the CSRF header.
    for r in [
        app.server
            .post(&segments(ws))
            .json(&json!({"name": "n", "rules": first_name_is("a")}))
            .await,
        app.server.patch(&one).json(&json!({"name": "n"})).await,
        app.server.delete(&one).await,
    ] {
        r.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(problem_type(&r), "urn:nm:problem:csrf");
    }
    assert_eq!(count(&pool).await, 1);
    assert_eq!(row(&pool, id).await.unwrap().0, "s");
}

/// Eleven are fetched and ten are listed, so ten is the largest list that is complete.
#[sqlx::test]
async fn purge_refusal_lists_ten_and_says_when_there_are_more(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let field = create_field(&app, ws, "plan").await;
    for i in 0..11 {
        create(&app, ws, &format!("n{i:02}"), plan_is("pro")).await;
    }
    app.delete(&format!("{}/{field}", fields(ws)))
        .await
        .assert_status_ok();
    let purge = format!("{}/{field}/purge", fields(ws));

    let r = app.post(&purge).await;
    r.assert_status(StatusCode::CONFLICT);
    let detail = r.json::<Value>()["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains("`n09`"), "{detail}");
    assert!(
        !detail.contains("`n10`"),
        "an eleventh name was listed: {detail}"
    );
    assert!(detail.contains(", and more"), "{detail}");

    // Exactly ten is a complete list, and must not claim otherwise.
    sqlx::query("delete from segments where name = 'n10'")
        .execute(&pool)
        .await
        .unwrap();
    let r = app.post(&purge).await;
    r.assert_status(StatusCode::CONFLICT);
    let detail = r.json::<Value>()["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains("`n09`"), "{detail}");
    assert!(
        !detail.contains("and more"),
        "ten is not a truncated list: {detail}"
    );
}

/// `jsonb` compares numbers by value, text compares their spelling. `1` and `1.0` are one
/// predicate, so resaving it must not bump the version. The stored document is rewritten to the
/// spelling that was sent, which is correct: what the rule means did not change.
#[sqlx::test]
async fn a_number_respelled_is_the_same_rule_and_does_not_bump_the_version(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    create_field_of(&app, ws, "seats", "number").await;
    let rule = |v: &str| {
        format!(r#"{{"match":"all","rules":[{{"field":"attr:seats","op":"eq","value":{v}}}]}}"#)
    };
    let id = create(&app, ws, "s", serde_json::from_str(&rule("1")).unwrap()).await;
    let one = format!("{}/{id}", segments(ws));
    let spelled = || async {
        sqlx::query_scalar::<_, String>(
            "select (rules->'rules'->0->'value')::text from segments where id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert_eq!(spelled().await, "1");

    // `serde_json::Value` would keep `1.0` as a float, but the text is what is sent here.
    let r = app
        .patch(&one)
        .content_type("application/json")
        .bytes(format!(r#"{{"rules":{}}}"#, rule("1.0")).into())
        .await;
    r.assert_status_ok();
    assert_eq!(
        spelled().await,
        "1.0",
        "the stored spelling follows the save"
    );
    assert_eq!(
        row(&pool, id).await.unwrap().3,
        1,
        "a respelled number bumped the version"
    );

    // A different number is a different rule.
    let r = app
        .patch(&one)
        .content_type("application/json")
        .bytes(format!(r#"{{"rules":{}}}"#, rule("2")).into())
        .await;
    assert_eq!(r.json::<Value>()["version"], 2);
}

/// A client that sends the whole object on every save must be able to rename a segment whose
/// field was archived since. Only a document that differs from the stored one is held to the
/// no-archived-field rule.
#[sqlx::test]
async fn resending_the_stored_rules_is_accepted_after_the_field_is_archived(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let field = create_field(&app, ws, "plan").await;
    let doc = plan_is("pro");
    let id = create(&app, ws, "pro", doc.clone()).await;
    let one = format!("{}/{id}", segments(ws));
    app.delete(&format!("{}/{field}", fields(ws)))
        .await
        .assert_status_ok();

    let r = app
        .patch(&one)
        .json(&json!({"name": "renamed", "rules": doc}))
        .await;
    r.assert_status_ok();
    let stored = row(&pool, id).await.unwrap();
    assert_eq!((stored.0.as_str(), stored.3), ("renamed", 1));
    assert_eq!(stored.2, doc);

    // One value different, and it is a new document: refused, and nothing changed.
    let r = app
        .patch(&one)
        .json(&json!({"name": "again", "rules": plan_is("free")}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem_type(&r), "urn:nm:problem:unknown-field");
    let stored = row(&pool, id).await.unwrap();
    assert_eq!((stored.0.as_str(), stored.2), ("renamed", doc));
}

/// A save holds its definitions `for share`, so an archive (and so the purge that can only follow
/// it) waits for it, and the purge guard then sees what the save wrote. The save's own call is
/// `parse_new_rules`; holding its transaction open is what a slow save looks like from outside.
#[sqlx::test]
async fn a_save_in_flight_holds_its_definitions_so_an_archive_waits(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let field = create_field(&app, ws, "plan").await;
    let mut tx = pool.begin().await.unwrap();
    let doc = plan_is("pro");
    nm::segments::parse_new_rules(&mut tx, ws, &doc)
        .await
        .expect("parses while the field is live");

    let archive = app.delete(&format!("{}/{field}", fields(ws)));
    let mut archive = std::pin::pin!(archive.into_future());
    tokio::select! {
        _ = &mut archive => panic!("the archive did not wait for the save in flight"),
        () = tokio::time::sleep(Duration::from_millis(600)) => {}
    }
    let archived: bool =
        sqlx::query_scalar("select archived_at is not null from custom_fields where id = $1")
            .bind(field)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!archived);

    // The save commits (here: writes its segment), the archive proceeds, the purge is refused.
    sqlx::query(
        "insert into segments (id, workspace_id, name, rules) values ($1, $2, 'saved', $3)",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(&doc)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    archive.await.assert_status_ok();
    let r = app.post(&format!("{}/{field}/purge", fields(ws))).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:field-in-use");
}

/// An empty PATCH changes nothing, so it writes nothing: an audit row saying "updated" with no
/// field would be a statement the log must never make.
#[sqlx::test]
async fn an_empty_patch_writes_nothing(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (ws2, app2) = workspace(&pool, "globex").await;
    let id = create(&app, ws, "s", first_name_is("a")).await;
    let one = format!("{}/{id}", segments(ws));
    let stamp = || async {
        sqlx::query_scalar::<_, String>("select updated_at::text from segments where id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let before = stamp().await;

    let r = app.patch(&one).json(&json!({})).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["name"], "s");
    assert_eq!(stamp().await, before, "an empty patch moved updated_at");
    let updates: i64 =
        sqlx::query_scalar("select count(*) from audit_log where action = 'segment.updated'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(updates, 0, "an empty patch wrote an audit row");

    // It still answers 404 for a segment that is not the caller's.
    app2.patch(&format!("{}/{id}", segments(ws2)))
        .json(&json!({}))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    app.patch(&format!("{}/{}", segments(ws), Uuid::now_v7()))
        .json(&json!({}))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

/// Renaming a foreign segment to a name one of ITS siblings holds would, if the UPDATE were not
/// scoped to the workspace, trip the unique constraint and answer 409: which says the foreign
/// workspace has a segment of that name. The answer must be the same 404 as for any other id.
#[sqlx::test]
async fn a_foreign_rename_cannot_probe_for_a_sibling_name(pool: PgPool) {
    let (ws1, admin1) = workspace(&pool, "acme").await;
    let (ws2, admin2) = workspace(&pool, "globex").await;
    let target = create(&admin1, ws1, "target", first_name_is("a")).await;
    create(&admin1, ws1, "sibling", first_name_is("b")).await;

    let r = admin2
        .patch(&format!("{}/{target}", segments(ws2)))
        .json(&json!({"name": "sibling"}))
        .await;
    r.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(problem_type(&r), "urn:nm:problem:not-found");
    assert_eq!(row(&pool, target).await.unwrap().0, "target");
}

/// The generated client is typed from this document. It must not offer `null` for what the server
/// answers 422 to, and must for what `null` clears.
#[test]
fn the_patch_schema_is_nullable_only_where_null_means_something() {
    let doc = serde_json::to_value(nm::openapi::spec()).unwrap();
    let schema = &doc["components"]["schemas"]["UpdateSegment"];
    let props = &schema["properties"];
    let nullable = |field: &str| {
        let t = &props[field]["type"];
        t == "null"
            || t.as_array().is_some_and(|a| a.iter().any(|x| x == "null"))
            || props[field]["oneOf"]
                .as_array()
                .is_some_and(|a| a.iter().any(|x| x["type"] == "null"))
    };
    assert!(!nullable("name"), "{props}");
    assert!(!nullable("rules"), "{props}");
    assert!(nullable("description"), "{props}");
    let required = &schema["required"];
    assert!(
        required.is_null() || required.as_array().is_some_and(Vec::is_empty),
        "{required}"
    );
}

/// A purge takes `for update` on its own (archived) field row, then reads `segments` with a plain
/// select (no row lock), then updates `contacts`. A save takes `for share` on the NON-archived
/// definitions, then writes `segments`. They share no lock, so neither can wait on the other and
/// there is no cycle to deadlock on. This holds a save open and shows the purge of another field
/// neither waits for it nor fails: a purge that started locking more than its own field (the
/// workspace's definitions, say) would queue behind the save and meet it in a cycle.
#[sqlx::test]
async fn a_purge_does_not_wait_for_a_save_in_flight(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let old = create_field(&app, ws, "old").await;
    create_field(&app, ws, "plan").await;
    app.delete(&format!("{}/{old}", fields(ws)))
        .await
        .assert_status_ok();

    let mut tx = pool.begin().await.unwrap();
    let doc = plan_is("pro");
    nm::segments::parse_new_rules(&mut tx, ws, &doc)
        .await
        .expect("a save holds its definitions");
    sqlx::query(
        "insert into segments (id, workspace_id, name, rules) values ($1, $2, 'saving', $3)",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(&doc)
    .execute(&mut *tx)
    .await
    .unwrap();

    let purge = app.post(&format!("{}/{old}/purge", fields(ws)));
    let r = tokio::time::timeout(Duration::from_secs(3), purge.into_future())
        .await
        .expect("the purge waited for a save that shares no lock with it");
    r.assert_status_ok();
    tx.commit().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// `POST /segments/preview` (spec M1b sections 5, 6, 12 and 13).

fn preview_path(ws: Uuid) -> String {
    format!("{}/preview", segments(ws))
}

async fn preview(app: &TestApp, ws: Uuid, rules: &Value) -> axum_test::TestResponse {
    app.post(&preview_path(ws))
        .json(&json!({ "rules": rules }))
        .await
}

async fn insert_contact(pool: &PgPool, ws: Uuid, email: &str, first_name: &str, attrs: Value) {
    sqlx::query(
        "insert into contacts (id, workspace_id, email, first_name, attributes) \
         values ($1, $2, $3, $4, $5::jsonb)",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(email)
    .bind(first_name)
    .bind(attrs)
    .execute(pool)
    .await
    .expect("insert contact");
}

/// The budget the previews below are given, far under the 1500 ms the spec budgets in production
/// (`Config::preview_timeout_ms`'s default, pinned in `config`'s own tests). The expensive
/// document is then about 180x over the budget instead of 12x, so the timeout assertions do not
/// depend on how fast the machine is.
const TEST_TIMEOUT_MS: u64 = 100;

/// A document the count cannot finish inside its budget, built only out of what the rule language
/// allows: 50 `not_contains` rules (the parser's maximum, in groups of at most 20) against 10 000
/// contacts whose `first_name` is 400 `a`s. Each rule asks for 200 `a`s followed by a letter that
/// is not there, so `ilike` restarts at every position of every row; every rule is true of every
/// row, so `all` cannot short-circuit and every one of them is evaluated. **Measured on the dev
/// container: 18.3 s uncapped.**
///
/// Every contact matches, so the true count is the whole fixture: the planner's estimate can be
/// held against a number that is known, which is what catches an `EXPLAIN` of the `count(*)`
/// (`Plan Rows: 1` at every node).
///
/// `first_name`, not an attribute: an `attr:` rule is answered with `null` before any query runs.
fn expensive_rules() -> Value {
    let rule = |i: usize| {
        json!({
            "field": "first_name",
            "op": "not_contains",
            "value": format!("{}b{i}", "a".repeat(200)),
        })
    };
    // `all`, not `any`: every rule is true of every row, so an `or` would short-circuit on the
    // first one and the document would cost a fiftieth of what it must.
    let group = |from: usize, to: usize| json!({"group": {"match": "all", "rules": (from..to).map(rule).collect::<Vec<_>>()}});
    json!({"match": "all", "rules": [group(0, 20), group(20, 40), group(40, 50)]})
}

/// The workspace under test, its signed-in admin (previewing with `TEST_TIMEOUT_MS`), 10 000
/// contacts in it — and **a second workspace with 10 000 contacts of its own**. The second one is
/// what holds the fallback honest: both queries on that path have to say which workspace they are
/// counting, and an `EXPLAIN` that forgets reports about 20 000 rows, which the bound in the
/// tests below rejects.
async fn expensive_fixture(pool: &PgPool) -> (Uuid, TestApp) {
    let (ws, app) = workspace_with(pool, "acme", |c| c.preview_timeout_ms = TEST_TIMEOUT_MS).await;
    let (other, _) = workspace(pool, "globex").await;
    for workspace_id in [ws, other] {
        sqlx::query(
            "insert into contacts (id, workspace_id, email, first_name) \
             select gen_random_uuid(), $1, 'c' || g || '@' || $1 || '.example', \
                    repeat('a', 400) \
             from generate_series(1, 10000) g",
        )
        .bind(workspace_id)
        .execute(pool)
        .await
        .expect("fill contacts");
    }
    // So the fallback's estimate is a real one. Without statistics the planner answers from its
    // defaults, and these tests could not tell a row estimate from a made-up one.
    sqlx::query("analyze contacts")
        .execute(pool)
        .await
        .expect("analyze");
    (ws, app)
}

/// A pool whose connections plan in parallel. On this fixture Postgres 16 then puts a `Gather` on
/// top, whose `Plan Rows` is the whole estimate, over a `Parallel Seq Scan` whose figure is **per
/// worker** — the under-report the fallback must not read.
async fn parallel_pool(pool: &PgPool) -> PgPool {
    PgPoolOptions::new()
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::raw_sql(
                    "set parallel_setup_cost = 0; set parallel_tuple_cost = 0; \
                     set min_parallel_table_scan_size = 0",
                )
                .execute(conn)
                .await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .expect("a pool that plans in parallel")
}

/// A cheap document is counted exactly, by a viewer, and only within the workspace asked about:
/// the compiler emits no `workspace_id` clause of its own, so the count query must write one.
#[sqlx::test]
async fn a_cheap_preview_is_exact_and_counts_only_this_workspaces_contacts(pool: PgPool) {
    let (ws, _admin) = workspace(&pool, "acme").await;
    let (other, _) = workspace(&pool, "globex").await;
    insert_contact(&pool, ws, "a@acme.com", "Ann", json!({})).await;
    insert_contact(&pool, ws, "b@acme.com", "Ann", json!({})).await;
    insert_contact(&pool, ws, "c@acme.com", "Bob", json!({})).await;
    insert_contact(&pool, other, "d@globex.com", "Ann", json!({})).await;

    let viewer = client(&pool, "viewer@acme.com", ws, Role::Viewer).await;
    let r = preview(&viewer, ws, &first_name_is("Ann")).await;
    r.assert_status_ok();
    let body: Value = r.json();
    assert_eq!(body["count"], 2, "the other workspace's Ann is not counted");
    assert_eq!(body["exact"], true);
    assert!(body["took_ms"].is_u64(), "{body}");
    assert_eq!(
        r.header("ratelimit-limit").to_str().unwrap(),
        PREVIEW_PER_MINUTE.to_string(),
        "the headers are on the answer, not only on the refusal"
    );

    // Nothing was saved: a preview is a read of an unsaved document.
    assert_eq!(count(&pool).await, 0);
}

/// The count runs under `SET LOCAL statement_timeout = '1500ms'`, so an expensive document
/// answers with the planner's estimate instead of hanging.
///
/// `exact: false` on its own would pass vacuously if the timeout never engaged — `SET LOCAL`
/// outside a transaction is a WARNING that leaves the timeout at `0` — so this also asserts
/// *when* the answer came back: at the budget, not at the 18.3 s the count really needs, and not
/// before the budget either, which is what an implementation that treats any error as "estimate"
/// would do.
#[sqlx::test]
async fn an_expensive_preview_times_out_and_falls_back_to_an_estimate(pool: PgPool) {
    let (ws, app) = expensive_fixture(&pool).await;

    let r = preview(&app, ws, &expensive_rules()).await;
    r.assert_status_ok();
    let body: Value = r.json();
    assert_eq!(body["exact"], false, "{body}");
    assert!(
        body["count"]
            .as_i64()
            .is_some_and(|n| (5_000..=15_000).contains(&n)),
        "the fallback reports the planner's row estimate for THIS workspace, near its 10 000 \
         contacts: not the one row an `EXPLAIN` of the `count(*)` reports, and not the 20 000 of \
         a query that forgot its workspace: {body}"
    );
    let took = body["took_ms"].as_u64().expect("took_ms");
    assert!(
        (TEST_TIMEOUT_MS..TEST_TIMEOUT_MS + 2_000).contains(&took),
        "the preview must have run until the {TEST_TIMEOUT_MS} ms timeout cut it off: not \
         finished early, not run to completion (18.3 s uncapped), and not under a timeout other \
         than the configured one. took_ms = {took}"
    );
}

/// `SET LOCAL` reverts when the transaction ends, including the transaction `statement_timeout`
/// aborted. The connection goes back to the pool with the server's own setting.
#[sqlx::test]
async fn a_timed_out_preview_leaves_no_timeout_behind_on_the_pool(pool: PgPool) {
    let (ws, app) = expensive_fixture(&pool).await;
    let r = preview(&app, ws, &expensive_rules()).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["exact"], false, "the preview timed out");

    // Held at the same time, so these are three different connections, the timed-out one among
    // them. The pool `#[sqlx::test]` hands out allows 20.
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(pool.acquire().await.expect("acquire"));
    }
    for conn in &mut held {
        let timeout: String = sqlx::query_scalar("show statement_timeout")
            .fetch_one(&mut **conn)
            .await
            .expect("show statement_timeout");
        assert_eq!(
            timeout, "0",
            "the preview's timeout outlived its transaction"
        );
    }
    drop(held);

    // And behaviourally: a statement that outlasts the preview's budget is not cut off.
    sqlx::query("select pg_sleep(1.8)")
        .execute(&pool)
        .await
        .expect("a normal query is not subject to the preview's 1500 ms timeout");
}

/// Section 6 ships no attribute index, so the planner has no statistics for `->>` and its
/// estimate measured 66x low. A document naming any attribute is answered with no number at all,
/// at any depth, and never with a planner figure.
#[sqlx::test]
async fn a_document_filtering_on_an_attribute_is_never_given_a_number(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    create_field(&app, ws, "plan").await;
    insert_contact(&pool, ws, "a@acme.com", "Ann", json!({"plan": "pro"})).await;
    insert_contact(&pool, ws, "b@acme.com", "Bob", json!({"plan": "pro"})).await;

    // An exact count here would be 2, and a planner estimate a number of its own; both are wrong.
    for doc in [
        plan_is("pro"),
        json!({"match": "all", "rules": [
            {"field": "first_name", "op": "eq", "value": "Ann"},
            {"group": {"match": "any", "rules": [{"field": "attr:plan", "op": "is_set"}]}},
        ]}),
    ] {
        let r = preview(&app, ws, &doc).await;
        r.assert_status_ok();
        let body: Value = r.json();
        assert!(body["count"].is_null(), "{doc} got a number: {body}");
        assert_eq!(body["exact"], false, "{body}");
    }
}

/// A preview is how a new rule is written, so it resolves like a save and not like a load: an
/// archived field may not be named.
#[sqlx::test]
async fn previewing_a_rule_on_an_archived_field_is_refused(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let field = create_field(&app, ws, "plan").await;
    preview(&app, ws, &plan_is("pro")).await.assert_status_ok();

    app.delete(&format!("{}/{field}", fields(ws)))
        .await
        .assert_status_ok();

    let r = preview(&app, ws, &plan_is("pro")).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem_type(&r), "urn:nm:problem:unknown-field");
    // The refusal still reports the quota it spent. A document is refused *after* the window has
    // been counted against, and the rule builder calling this on a debounce is exactly the client
    // that needs to know how little is left; the endpoint's contract promises these headers on
    // every answer past the check, not only on the 429.
    assert_eq!(
        r.header("ratelimit-limit").to_str().unwrap(),
        PREVIEW_PER_MINUTE.to_string(),
        "a 422 dropped the headers the contract promises"
    );
    assert!(r.maybe_header("ratelimit-remaining").is_some(), "{:?}", r);
}

/// Preview is the first endpoint outside sign-in with a rate limit: an arbitrary document, a
/// viewer, up to 1.5 s of a pooled connection, called on a debounce as someone types.
#[sqlx::test]
async fn preview_is_rate_limited_per_workspace_and_reports_its_headers(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (other, _) = workspace(&pool, "globex").await;
    // The same signed-in admin in both workspaces, so what differs is the workspace alone.
    sqlx::query(
        "insert into memberships (workspace_id, user_id, role) \
         select $1, user_id, 'admin' from memberships where workspace_id = $2",
    )
    .bind(other)
    .bind(ws)
    .execute(&pool)
    .await
    .expect("second membership");

    for i in 1..=PREVIEW_PER_MINUTE {
        let r = preview(&app, ws, &first_name_is("Ann")).await;
        r.assert_status_ok();
        assert_eq!(
            r.header("ratelimit-remaining").to_str().unwrap(),
            (PREVIEW_PER_MINUTE - i).to_string(),
            "request {i} of {PREVIEW_PER_MINUTE}"
        );
    }
    let r = preview(&app, ws, &first_name_is("Ann")).await;
    r.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(problem_type(&r), "urn:nm:problem:rate-limited");
    assert_eq!(r.header("ratelimit-remaining").to_str().unwrap(), "0");
    assert_eq!(
        r.header("ratelimit-limit").to_str().unwrap(),
        PREVIEW_PER_MINUTE.to_string()
    );
    let reset: u64 = r
        .header("ratelimit-reset")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&reset), "reset = {reset}");
    let retry: u64 = r.header("retry-after").to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry), "retry-after = {retry}");

    // The window is per workspace: the same person, one workspace along, is not refused.
    preview(&app, other, &first_name_is("Ann"))
        .await
        .assert_status_ok();
}

/// The fallback reads the **top** node. Under a parallel plan a scan node's `Plan Rows` is per
/// worker, so a leaf read under-reports by the worker count — here about 4 100 against 10 000.
/// The pool, not the fixture, is what makes the plan parallel.
#[sqlx::test]
async fn the_fallbacks_estimate_is_the_top_nodes_row_count(pool: PgPool) {
    let (ws, _app) = expensive_fixture(&pool).await;
    let parallel = parallel_pool(&pool).await;
    let mut app =
        TestApp::with_config(parallel.clone(), |c| c.preview_timeout_ms = TEST_TIMEOUT_MS).await;
    app.login("admin-acme@example.com", PW).await;

    // The precondition this test rests on: the plan really is parallel, and its leaf really does
    // report a different, smaller number than the top. Without this the bound below would pass
    // whichever node the code read.
    let mut conn = pool.acquire().await.expect("acquire");
    let group = parse_new_rules(&mut conn, ws, &expensive_rules())
        .await
        .expect("the document parses");
    let mut q = QueryBuilder::<Postgres>::new(
        "explain (format json) select 1 from contacts where workspace_id = ",
    );
    q.push_bind(ws).push(" and ");
    push_rules(&mut q, &group);
    let plan: Value = q
        .build_query_scalar()
        .fetch_one(&parallel)
        .await
        .expect("explain");
    let top = &plan[0]["Plan"];
    assert_eq!(top["Node Type"], "Gather", "not a parallel plan: {top}");
    let top_rows = top["Plan Rows"].as_i64().expect("the top estimate");
    let leaf = top["Plans"][0]["Plan Rows"]
        .as_i64()
        .expect("a leaf estimate");
    assert!(
        leaf < 5_000 && (5_000..=15_000).contains(&top_rows),
        "the leaf must be outside the bound this test asserts, or it proves nothing: \
         leaf {leaf}, top {top_rows}"
    );

    let r = preview(&app, ws, &expensive_rules()).await;
    r.assert_status_ok();
    let body: Value = r.json();
    assert_eq!(body["exact"], false, "{body}");
    assert_eq!(
        body["count"], top_rows,
        "the estimate is the top node's, not the per-worker leaf's ({leaf}): {body}"
    );
    parallel.close().await;
}

/// The limit is checked **after** `authorize`, so someone who is not in the workspace cannot
/// spend its quota and deny previews to the people who are. One app, two sessions: the window
/// lives in the app's state, so this only means anything on a single app.
#[sqlx::test]
async fn a_non_member_is_refused_and_spends_none_of_the_workspaces_quota(pool: PgPool) {
    let (ws, mut app) = workspace(&pool, "acme").await;
    create_user(&pool, "outsider@example.com", PW, false).await;
    app.login("outsider@example.com", PW).await;

    for i in 0..=PREVIEW_PER_MINUTE {
        let r = preview(&app, ws, &first_name_is("Ann")).await;
        r.assert_status(StatusCode::NOT_FOUND);
        assert!(
            r.maybe_header("ratelimit-limit").is_none(),
            "request {i} told a non-member about the workspace's quota"
        );
    }

    app.login("admin-acme@example.com", PW).await;
    let r = preview(&app, ws, &first_name_is("Ann")).await;
    r.assert_status_ok();
    assert_eq!(
        r.header("ratelimit-remaining").to_str().unwrap(),
        (PREVIEW_PER_MINUTE - 1).to_string(),
        "the member's first preview must be the window's first"
    );
}
