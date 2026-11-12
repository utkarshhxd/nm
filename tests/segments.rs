// SPDX-License-Identifier: AGPL-3.0-only

//! Segment CRUD (spec M1b sections 7, 12, 13 and 19), asserted against real rows. Every test that
//! claims a refusal reads the table afterwards: a status code alone cannot tell "refused" from
//! "refused after writing".

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::{
    error::ApiError,
    segments::{load_rules, parse_new_rules},
    workspaces::roles::Role,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

async fn client(pool: &PgPool, email: &str, ws: Uuid, role: Role) -> TestApp {
    let id = create_user(pool, email, PW, false).await;
    add_member(pool, ws, id, role).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    app
}

/// A workspace and a signed-in admin of it. An admin may do everything an editor may, and may
/// also define and purge custom fields, which several tests here need.
async fn workspace(pool: &PgPool, slug: &str) -> (Uuid, TestApp) {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let ws = create_workspace(pool, owner, slug).await;
    let admin = client(pool, &format!("admin-{slug}@example.com"), ws, Role::Admin).await;
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
    let r = app
        .post(&fields(ws))
        .json(&json!({"key": key, "label": key, "kind": "text"}))
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
