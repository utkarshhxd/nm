// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::{
    contacts::{
        ContactFilter,
        selection::{MAX_EXCLUDED, Selection, issue},
    },
    workspaces::roles::Role,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::{NoContext, Timestamp, Uuid};

async fn client(pool: &PgPool, email: &str, ws: Uuid, role: Role) -> TestApp {
    let id = create_user(pool, email, PW, false).await;
    add_member(pool, ws, id, role).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    app
}

/// A workspace and a signed-in editor of it.
async fn workspace(pool: &PgPool, slug: &str) -> (Uuid, TestApp) {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let ws = create_workspace(pool, owner, slug).await;
    let editor = client(
        pool,
        &format!("editor-{slug}@example.com"),
        ws,
        Role::Editor,
    )
    .await;
    (ws, editor)
}

fn base(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}")
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

async fn contact(app: &TestApp, ws: Uuid, email: &str) -> Uuid {
    let r = app
        .post(&format!("{}/contacts", base(ws)))
        .json(&json!({"email": email}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn contacts(app: &TestApp, ws: Uuid, prefix: &str, n: usize) -> Vec<Uuid> {
    let mut ids = vec![];
    for i in 0..n {
        ids.push(contact(app, ws, &format!("{prefix}{i}@ex.test")).await);
    }
    ids
}

async fn tag(app: &TestApp, ws: Uuid, name: &str) -> Uuid {
    let r = app
        .post(&format!("{}/tags", base(ws)))
        .json(&json!({"name": name}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn list(app: &TestApp, ws: Uuid, name: &str, double_optin: bool) -> Uuid {
    let r = app
        .post(&format!("{}/lists", base(ws)))
        .json(&json!({"name": name, "double_optin": double_optin}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn select(app: &TestApp, ws: Uuid, filter: Value) -> String {
    let r = app
        .post(&format!("{}/contacts/selection", base(ws)))
        .json(&filter)
        .await;
    r.assert_status_ok();
    r.json::<Value>()["selection_token"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn bulk(app: &TestApp, ws: Uuid, action: &str, body: Value) -> axum_test::TestResponse {
    app.post(&format!("{}/contacts/bulk/{action}", base(ws)))
        .json(&body)
        .await
}

async fn affected(r: axum_test::TestResponse) -> u64 {
    r.assert_status_ok();
    r.json::<Value>()["affected"].as_u64().unwrap()
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

async fn tagged(pool: &PgPool, tag: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar("select contact_id from contact_tags where tag_id = $1")
        .bind(tag)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// The review-focus case. A contact that lands between the click and the action must not be
/// acted on, whatever the action is.
#[sqlx::test]
async fn contacts_created_after_as_of_are_not_included(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let before = contacts(&app, ws, "before", 3).await;
    let token = select(&app, ws, json!({})).await;
    let after = contacts(&app, ws, "after", 2).await;

    let n = affected(
        bulk(
            &app,
            ws,
            "tag",
            json!({"selection_token": token, "tag_id": vip}),
        )
        .await,
    )
    .await;
    assert_eq!(n, 3);
    let mut got = tagged(&pool, vip).await;
    got.sort();
    let mut want = before.clone();
    want.sort();
    assert_eq!(got, want, "only the contacts that existed at the click");

    // Deleting is the case that cannot be undone: the late arrivals must survive it.
    let n = affected(bulk(&app, ws, "delete", json!({"selection_token": token})).await).await;
    assert_eq!(n, 3);
    let left: Vec<Uuid> = sqlx::query_scalar("select id from contacts order by id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(left, after);
}

#[sqlx::test]
async fn excluded_ids_are_not_affected(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let ids = contacts(&app, ws, "c", 4).await;
    let token = select(&app, ws, json!({})).await;
    let n = affected(
        bulk(
            &app,
            ws,
            "tag",
            json!({"selection_token": token, "tag_id": vip, "excluded_ids": [ids[0], ids[3]]}),
        )
        .await,
    )
    .await;
    assert_eq!(n, 2);
    let mut got = tagged(&pool, vip).await;
    got.sort();
    assert_eq!(got, vec![ids[1], ids[2]]);
}

#[sqlx::test]
async fn token_from_another_workspace_is_404(pool: PgPool) {
    let (ws_a, app_a) = workspace(&pool, "acme").await;
    let (ws_b, app_b) = workspace(&pool, "other").await;
    contacts(&app_a, ws_a, "a", 2).await;
    let victim = contacts(&app_b, ws_b, "b", 2).await;
    let token_b = select(&app_b, ws_b, json!({})).await;

    // An editor of A presents B's token against A: not found, and B's contacts are untouched.
    let r = bulk(&app_a, ws_a, "delete", json!({"selection_token": token_b})).await;
    r.assert_status_not_found();
    // And against B itself, where A's editor is no member.
    let r = bulk(&app_a, ws_b, "delete", json!({"selection_token": token_b})).await;
    r.assert_status_not_found();
    assert_eq!(count(&pool, "select count(*) from contacts").await, 4);
    assert_eq!(victim.len(), 2);
}

/// A token for `ws` as of `age` ago, signed by this instance.
fn token_aged(app: &TestApp, ws: Uuid, age: std::time::Duration) -> String {
    let then = std::time::SystemTime::now() - age;
    let secs = then
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let sel = Selection {
        workspace_id: ws,
        filter: ContactFilter::default(),
        as_of: Uuid::new_v7(Timestamp::from_unix(NoContext, secs, 0)),
    };
    issue(&app.state.secrets, &sel)
}

#[sqlx::test]
async fn expired_token_is_422(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    contacts(&app, ws, "c", 2).await;
    let hour = std::time::Duration::from_secs(60 * 60);

    let old = token_aged(&app, ws, hour + std::time::Duration::from_secs(60));
    let r = bulk(&app, ws, "delete", json!({"selection_token": old})).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem_type(&r), "urn:nm:problem:selection-expired");
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);

    // Just inside the hour is accepted. It selects nothing: both contacts were created after
    // that token's `as_of`, which is the snapshot bound doing its job on an old token.
    let fresh = token_aged(&app, ws, hour - std::time::Duration::from_secs(60));
    let n = affected(bulk(&app, ws, "delete", json!({"selection_token": fresh})).await).await;
    assert_eq!(n, 0);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);
}

#[sqlx::test]
async fn a_tampered_token_is_rejected(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    contacts(&app, ws, "c", 2).await;
    let token = select(&app, ws, json!({})).await;
    let (payload, sig) = token.split_once('.').unwrap();

    // Widen the filter's tag, keep the old signature.
    let mut v: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    v["filter"]["tag_id"] = json!(Uuid::now_v7());
    let forged_payload = format!(
        "{}.{sig}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&v).unwrap())
    );
    // Push the bound into the future, to take in contacts created later.
    v["as_of"] = json!(Uuid::max());
    let forged_bound = format!(
        "{}.{sig}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&v).unwrap())
    );
    // A signature from the right payload cut short.
    let truncated = format!("{payload}.{}", &sig[..sig.len() - 2]);
    for bad in [
        forged_payload,
        forged_bound,
        truncated,
        "not-a-token".into(),
    ] {
        let r = bulk(&app, ws, "delete", json!({"selection_token": bad})).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(problem_type(&r), "urn:nm:problem:selection-invalid");
    }
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);
}

#[sqlx::test]
async fn bulk_writes_exactly_one_audit_row(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let news = list(&app, ws, "news", false).await;
    let ids = contacts(&app, ws, "c", 5).await;
    for id in &ids {
        app.post(&format!("{}/contacts/{id}/lists", base(ws)))
            .json(&json!({"list_id": news}))
            .await
            .assert_status_success();
    }
    let token = select(&app, ws, json!({"list_id": news})).await;
    let audit_before = count(&pool, "select count(*) from audit_log").await;

    let n = affected(
        bulk(
            &app,
            ws,
            "unsubscribe",
            json!({"selection_token": token, "list_id": news, "excluded_ids": [ids[0]]}),
        )
        .await,
    )
    .await;
    assert_eq!(n, 4);

    // Exactly one row in all, not one per contact.
    assert_eq!(
        count(&pool, "select count(*) from audit_log").await,
        audit_before + 1
    );
    let (action, meta): (String, Value) = sqlx::query_as(
        "select action, metadata from audit_log where action like 'contacts.bulk_%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(action, "contacts.bulk_unsubscribed");
    assert_eq!(meta["affected"], 4);
    assert_eq!(meta["excluded"], 1);
    assert_eq!(meta["filter"]["list_id"], json!(news));
    assert!(meta["as_of"].as_str().unwrap().parse::<Uuid>().is_ok());
}

#[sqlx::test]
async fn bulk_is_editor_and_above(pool: PgPool) {
    let (ws, editor) = workspace(&pool, "acme").await;
    let viewer = client(&pool, "viewer@example.com", ws, Role::Viewer).await;
    let vip = tag(&editor, ws, "vip").await;
    let news = list(&editor, ws, "news", false).await;
    contacts(&editor, ws, "c", 2).await;
    let token = select(&editor, ws, json!({})).await;

    // A viewer cannot even take a selection, and a token they were handed is no use to them.
    viewer
        .post(&format!("{}/contacts/selection", base(ws)))
        .json(&json!({}))
        .await
        .assert_status_forbidden();
    for (action, extra) in [
        ("tag", json!({"tag_id": vip})),
        ("untag", json!({"tag_id": vip})),
        ("subscribe", json!({"list_id": news})),
        ("unsubscribe", json!({"list_id": news})),
        ("delete", json!({})),
    ] {
        let mut body = extra;
        body["selection_token"] = json!(token);
        bulk(&viewer, ws, action, body)
            .await
            .assert_status_forbidden();
    }
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);

    // An editor can, and delete writes no suppression: it is not erasure.
    let n = affected(bulk(&editor, ws, "delete", json!({"selection_token": token})).await).await;
    assert_eq!(n, 2);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 0);
    assert_eq!(count(&pool, "select count(*) from suppressions").await, 0);
}

#[sqlx::test]
async fn response_reports_the_count_actually_affected(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let double = list(&app, ws, "double", true).await;
    let ids = contacts(&app, ws, "c", 4).await;
    let all = select(&app, ws, json!({})).await;
    let tag_body = |t: &str| json!({"selection_token": t, "tag_id": vip});

    // One is tagged already: three newly tagged, not four.
    app.put(&format!("{}/contacts/{}/tags/{vip}", base(ws), ids[0]))
        .await
        .assert_status_success();
    assert_eq!(
        affected(bulk(&app, ws, "tag", tag_body(&all)).await).await,
        3
    );
    // Doing it again changes nothing.
    assert_eq!(
        affected(bulk(&app, ws, "tag", tag_body(&all)).await).await,
        0
    );
    assert_eq!(
        affected(bulk(&app, ws, "untag", tag_body(&all)).await).await,
        4
    );
    assert_eq!(
        affected(bulk(&app, ws, "untag", tag_body(&all)).await).await,
        0
    );

    // Subscribing to a double-opt-in list writes `pending`, and a second go writes nothing.
    let sub = json!({"selection_token": all, "list_id": double});
    assert_eq!(
        affected(bulk(&app, ws, "subscribe", sub.clone()).await).await,
        4
    );
    assert_eq!(affected(bulk(&app, ws, "subscribe", sub).await).await, 0);
    let pending = count(
        &pool,
        "select count(*) from list_memberships where status = 'pending'",
    )
    .await;
    assert_eq!(pending, 4);

    // Unsubscribing one by hand first leaves three for the bulk action to do.
    app.delete(&format!("{}/contacts/{}/lists/{double}", base(ws), ids[1]))
        .await
        .assert_status_success();
    let unsub = json!({"selection_token": all, "list_id": double});
    assert_eq!(
        affected(bulk(&app, ws, "unsubscribe", unsub).await).await,
        3
    );
    // The row stays, with the status: an unsubscribe is never a deleted row.
    let kept = count(
        &pool,
        "select count(*) from list_memberships where status = 'unsubscribed'",
    )
    .await;
    assert_eq!(kept, 4);
}

/// Every field of the filter survives the token and narrows the action.
#[sqlx::test]
async fn all_four_filter_fields_apply(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let done = tag(&app, ws, "done").await;
    let news = list(&app, ws, "news", false).await;
    let ids = [
        contact(&app, ws, "match-a@ex.test").await,
        contact(&app, ws, "match-b@ex.test").await,
        contact(&app, ws, "other@ex.test").await,
    ];
    for id in &ids {
        app.post(&format!("{}/contacts/{id}/lists", base(ws)))
            .json(&json!({"list_id": news}))
            .await
            .assert_status_success();
    }
    // `match-b` is unsubscribed, so `list_status` takes it out; `other` fails the search.
    app.delete(&format!("{}/contacts/{}/lists/{news}", base(ws), ids[1]))
        .await
        .assert_status_success();
    for id in &ids {
        app.put(&format!("{}/contacts/{id}/tags/{vip}", base(ws)))
            .await
            .assert_status_success();
    }
    app.delete(&format!("{}/contacts/{}/tags/{vip}", base(ws), ids[0]))
        .await
        .assert_status_success();
    app.put(&format!("{}/contacts/{}/tags/{vip}", base(ws), ids[0]))
        .await
        .assert_status_success();

    let token = select(
        &app,
        ws,
        json!({"q": "match", "list_id": news, "list_status": "subscribed", "tag_id": vip}),
    )
    .await;
    let n = affected(
        bulk(
            &app,
            ws,
            "tag",
            json!({"selection_token": token, "tag_id": done}),
        )
        .await,
    )
    .await;
    assert_eq!(n, 1);
    assert_eq!(tagged(&pool, done).await, vec![ids[0]]);

    // A `list_status` with no list, and a one-letter search, are refused at the door.
    for bad in [json!({"list_status": "subscribed"}), json!({"q": "ab"})] {
        app.post(&format!("{}/contacts/selection", base(ws)))
            .json(&bad)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[sqlx::test]
async fn excluded_ids_are_bounded(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    contacts(&app, ws, "c", 2).await;
    let token = select(&app, ws, json!({})).await;
    let ids = |n: usize| (0..n).map(|_| Uuid::now_v7()).collect::<Vec<_>>();

    let r = bulk(
        &app,
        ws,
        "delete",
        json!({"selection_token": token, "excluded_ids": ids(MAX_EXCLUDED + 1)}),
    )
    .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);

    // Exactly the cap is fine.
    let r = bulk(
        &app,
        ws,
        "delete",
        json!({"selection_token": token, "excluded_ids": ids(MAX_EXCLUDED)}),
    )
    .await;
    assert_eq!(affected(r).await, 2);
}

#[sqlx::test]
async fn a_tag_or_list_of_another_workspace_is_404(pool: PgPool) {
    let (ws_a, app_a) = workspace(&pool, "acme").await;
    let (ws_b, app_b) = workspace(&pool, "other").await;
    let foreign_tag = tag(&app_b, ws_b, "theirs").await;
    let foreign_list = list(&app_b, ws_b, "theirs", false).await;
    contacts(&app_a, ws_a, "c", 2).await;
    let token = select(&app_a, ws_a, json!({})).await;
    for (action, extra) in [
        ("tag", json!({"tag_id": foreign_tag})),
        ("untag", json!({"tag_id": foreign_tag})),
        ("subscribe", json!({"list_id": foreign_list})),
        ("unsubscribe", json!({"list_id": foreign_list})),
    ] {
        let mut body = extra;
        body["selection_token"] = json!(token);
        bulk(&app_a, ws_a, action, body)
            .await
            .assert_status_not_found();
    }
    assert_eq!(
        count(
            &pool,
            "select count(*) from audit_log where action like 'contacts.bulk_%'"
        )
        .await,
        0
    );
}
