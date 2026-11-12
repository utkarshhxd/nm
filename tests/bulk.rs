// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::{
    contacts::{
        ContactFilter,
        selection::{MAX_EXCLUDED, MAX_IDS, Selection, issue},
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
        ids: None,
        issued: 0,
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
    let vip = tag(&app, ws, "vip").await;
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

    // Exactly the cap passes the check and the action runs over the two that match. Tag rather
    // than delete: ten thousand ids that are not in the match are exactly the case the delete
    // bound refuses conservatively, which
    // `excluding_an_id_outside_the_match_is_refused_conservatively` pins on its own.
    let r = bulk(
        &app,
        ws,
        "tag",
        json!({"selection_token": token, "tag_id": vip, "excluded_ids": ids(MAX_EXCLUDED)}),
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

async fn status_of(pool: &PgPool, contact: Uuid, list: Uuid) -> Option<String> {
    sqlx::query_scalar("select status from list_memberships where contact_id = $1 and list_id = $2")
        .bind(contact)
        .bind(list)
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn set_status(pool: &PgPool, contact: Uuid, list: Uuid, status: &str) {
    sqlx::query("update list_memberships set status = $3 where contact_id = $1 and list_id = $2")
        .bind(contact)
        .bind(list)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
}

/// Bulk subscribe adds the missing and touches nothing else: no promoting `pending`, and above
/// all no reviving `unsubscribed`.
#[sqlx::test]
async fn bulk_subscribe_never_changes_an_existing_membership(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let single = list(&app, ws, "single", false).await;
    let double = list(&app, ws, "double", true).await;
    let ids = contacts(&app, ws, "c", 4).await; // [none, pending, subscribed, unsubscribed]

    for l in [single, double] {
        for i in [1, 2, 3] {
            app.post(&format!("{}/contacts/{}/lists", base(ws), ids[i]))
                .json(&json!({"list_id": l}))
                .await
                .assert_status_success();
        }
        set_status(&pool, ids[1], l, "pending").await;
        set_status(&pool, ids[2], l, "subscribed").await;
        set_status(&pool, ids[3], l, "unsubscribed").await;
    }
    let token = select(&app, ws, json!({})).await;

    for (l, new_status) in [(single, "subscribed"), (double, "pending")] {
        let body = json!({"selection_token": token, "list_id": l});
        // Only the contact with no row is added: the count says so.
        assert_eq!(affected(bulk(&app, ws, "subscribe", body).await).await, 1);
        let mut got = vec![];
        for id in &ids {
            got.push(status_of(&pool, *id, l).await.unwrap());
        }
        assert_eq!(got, [new_status, "pending", "subscribed", "unsubscribed"]);
    }
}

/// The audit log outlives an erasure, so a search term, which can be an address, must not be in it.
#[sqlx::test]
async fn the_audit_row_does_not_hold_the_search_text(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    contact(&app, ws, "zebediah.quux@ex.test").await;
    let token = select(&app, ws, json!({"q": "zebediah.quux@ex.test"})).await;
    let body = json!({"selection_token": token, "tag_id": vip});
    assert_eq!(affected(bulk(&app, ws, "tag", body).await).await, 1);

    let (meta,): (String,) =
        sqlx::query_as("select metadata::text from audit_log where action like 'contacts.bulk_%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    for needle in ["zebediah", "quux", "ex.test"] {
        assert!(!meta.contains(needle), "{needle} leaked into {meta}");
    }
    let meta: Value = serde_json::from_str(&meta).unwrap();
    assert_eq!(
        meta["filter"]["q"], "<redacted>",
        "a reader can still tell a search was used"
    );
    assert_eq!(meta["affected"], 1);
}

/// `affected` alone cannot be read: the caller needs `selected` to say "N of M", and on a
/// double-opt-in list needs `status` to know the people are not yet mailable.
#[sqlx::test]
async fn response_carries_selected_and_the_resulting_status(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let single = list(&app, ws, "single", false).await;
    let double = list(&app, ws, "double", true).await;
    let ids = contacts(&app, ws, "c", 6).await;
    let token = select(&app, ws, json!({})).await;
    contacts(&app, ws, "late", 3).await; // after the token: in neither number
    for id in &ids[..4] {
        app.put(&format!("{}/contacts/{id}/tags/{vip}", base(ws)))
            .await
            .assert_status_success();
    }

    let r = bulk(
        &app,
        ws,
        "tag",
        json!({"selection_token": token, "tag_id": vip}),
    )
    .await;
    r.assert_status_ok();
    let v: Value = r.json();
    assert_eq!((&v["selected"], &v["affected"]), (&json!(6), &json!(2)));
    assert!(v.get("status").is_none(), "only subscribe has a status");

    for (l, status) in [(double, "pending"), (single, "subscribed")] {
        let body = json!({"selection_token": token, "list_id": l, "excluded_ids": [ids[0]]});
        let v: Value = bulk(&app, ws, "subscribe", body.clone()).await.json();
        assert_eq!((&v["selected"], &v["affected"]), (&json!(5), &json!(5)));
        assert_eq!(v["status"], status);
        // Again: all five selected, none of them changed.
        let v: Value = bulk(&app, ws, "subscribe", body).await.json();
        assert_eq!((&v["selected"], &v["affected"]), (&json!(5), &json!(0)));
        assert_eq!(v["status"], status);
    }

    let v: Value = bulk(&app, ws, "delete", json!({"selection_token": token}))
        .await
        .json();
    assert_eq!((&v["selected"], &v["affected"]), (&json!(6), &json!(6)));
    let meta: Value =
        sqlx::query_scalar("select metadata from audit_log where action = 'contacts.bulk_deleted'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(meta["selected"], 6);
}

/// Ticked rows, no token. The same skip-unsubscribed rule as a filter selection, and the same two
/// numbers: three of four were added, and the one who unsubscribed was not brought back.
#[sqlx::test]
async fn an_id_list_subscribes_the_new_and_skips_an_unsubscribed_contact(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (_, other) = workspace(&pool, "other").await;
    let single = list(&app, ws, "single", false).await;
    let ids = contacts(&app, ws, "c", 4).await;
    app.post(&format!("{}/contacts/{}/lists", base(ws), ids[3]))
        .json(&json!({"list_id": single}))
        .await
        .assert_status_success();
    set_status(&pool, ids[3], single, "unsubscribed").await;
    let outsider = Uuid::now_v7(); // not a contact of this workspace at all
    drop(other);

    let mut named = ids.clone();
    named.push(outsider);
    let r = bulk(
        &app,
        ws,
        "subscribe",
        json!({"ids": named, "list_id": single}),
    )
    .await;
    r.assert_status_ok();
    let v: Value = r.json();
    assert_eq!((&v["selected"], &v["affected"]), (&json!(4), &json!(3)));
    assert_eq!(v["status"], "subscribed");
    assert_eq!(
        status_of(&pool, ids[3], single).await.unwrap(),
        "unsubscribed"
    );
    for id in &ids[..3] {
        assert_eq!(status_of(&pool, *id, single).await.unwrap(), "subscribed");
    }
}

/// An id of another workspace's contact selects nothing: the workspace scope is in the statement.
#[sqlx::test]
async fn an_id_list_cannot_reach_another_workspaces_contacts(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (ws2, app2) = workspace(&pool, "other").await;
    let theirs = contacts(&app2, ws2, "t", 2).await;
    let vip = tag(&app, ws, "vip").await;
    let r = bulk(&app, ws, "tag", json!({"ids": theirs, "tag_id": vip})).await;
    let v: Value = r.json();
    assert_eq!((&v["selected"], &v["affected"]), (&json!(0), &json!(0)));
    assert!(tagged(&pool, vip).await.is_empty());
}

#[sqlx::test]
async fn exactly_one_of_ids_and_a_token(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let ids = contacts(&app, ws, "c", 2).await;
    let token = select(&app, ws, json!({})).await;
    for body in [
        json!({"ids": ids, "selection_token": token, "tag_id": vip}),
        json!({"tag_id": vip}),
        json!({"ids": ids, "excluded_ids": [ids[0]], "tag_id": vip}),
    ] {
        let r = bulk(&app, ws, "tag", body).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
    assert!(tagged(&pool, vip).await.is_empty());
}

#[sqlx::test]
async fn ids_are_bounded(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    contacts(&app, ws, "c", 2).await;
    let ids = |n: usize| (0..n).map(|_| Uuid::now_v7()).collect::<Vec<_>>();
    let r = bulk(
        &app,
        ws,
        "tag",
        json!({"ids": ids(MAX_IDS + 1), "tag_id": vip}),
    )
    .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    // Exactly the cap is accepted (and matches nothing here).
    let r = bulk(&app, ws, "tag", json!({"ids": ids(MAX_IDS), "tag_id": vip})).await;
    assert_eq!(affected(r).await, 0);
}

/// One row, saying it was an id list and how many, with none of the ids in it.
#[sqlx::test]
async fn an_id_list_writes_one_audit_row_without_the_ids(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let ids = contacts(&app, ws, "c", 3).await;
    let before = count(&pool, "select count(*) from audit_log").await;
    assert_eq!(
        affected(bulk(&app, ws, "tag", json!({"ids": ids, "tag_id": vip})).await).await,
        3
    );
    assert_eq!(
        count(&pool, "select count(*) from audit_log").await,
        before + 1
    );
    let (meta,): (String,) = sqlx::query_as(
        "select metadata::text from audit_log where action = 'contacts.bulk_tagged'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    for id in &ids {
        assert!(!meta.contains(&id.to_string()), "{id} leaked into {meta}");
    }
    let meta: Value = serde_json::from_str(&meta).unwrap();
    assert_eq!(
        (&meta["by"], &meta["ids"], &meta["selected"]),
        (&json!("ids"), &json!(3), &json!(3))
    );
}

/// `selected` is the live count under the token's own bound, so a later signup is not in it.
#[sqlx::test]
async fn creating_a_selection_reports_how_many_it_matched(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    contacts(&app, ws, "match", 3).await;
    contacts(&app, ws, "other", 2).await;
    let r = app
        .post(&format!("{}/contacts/selection", base(ws)))
        .json(&json!({"q": "match"}))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["selected"], 3);
    let all = app
        .post(&format!("{}/contacts/selection", base(ws)))
        .json(&json!({}))
        .await;
    assert_eq!(all.json::<Value>()["selected"], 5);
}

/// The irreversible one. Workspace A names workspace B's contacts to `bulk/delete`: they must
/// survive, and neither number may reveal that those ids exist somewhere else. The workspace is
/// scoped in both the `sel` CTE and the delete statement, so a one-layer regression would not
/// redden this; removing both does.
#[sqlx::test]
async fn an_id_list_cannot_delete_another_workspaces_contacts(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (ws2, app2) = workspace(&pool, "other").await;
    let mine = contacts(&app, ws, "m", 1).await;
    let theirs = contacts(&app2, ws2, "t", 3).await;

    let r = bulk(&app, ws, "delete", json!({"ids": theirs})).await;
    r.assert_status_ok();
    let v: Value = r.json();
    assert_eq!((&v["selected"], &v["affected"]), (&json!(0), &json!(0)));
    assert_eq!(count(&pool, "select count(*) from contacts").await, 4);

    // The same request with one of its own is scoped, not rejected.
    let r = bulk(&app, ws, "delete", json!({"ids": [theirs[0], mine[0]]})).await;
    let v: Value = r.json();
    assert_eq!((&v["selected"], &v["affected"]), (&json!(1), &json!(1)));
    assert_eq!(count(&pool, "select count(*) from contacts").await, 3);
    assert_eq!(count_in(&pool, ws2).await, 3);
}

/// An empty list is not a selection: 422, and no audit row for an action that did nothing.
#[sqlx::test]
async fn an_empty_id_list_is_422_and_writes_no_audit_row(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let before = count(&pool, "select count(*) from audit_log").await;
    let r = bulk(&app, ws, "tag", json!({"ids": [], "tag_id": vip})).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count(&pool, "select count(*) from audit_log").await, before);
}

async fn count_in(pool: &PgPool, ws: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from contacts where workspace_id = $1")
        .bind(ws)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// An unknown key in a filter must be a refusal. Ignored, `{"tag_id_": ...}` is a filter with
/// every field empty, which matches every contact in the workspace; handed to `bulk/delete` that
/// deletes them all.
#[sqlx::test]
async fn a_selection_filter_with_an_unknown_field_is_422_not_everything(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    contacts(&app, ws, "c", 3).await;
    for bad in [
        json!({"tag_id_": vip}),
        json!({"tag": vip}),
        json!({"tag_id": vip, "extra": 1}),
    ] {
        let r = app
            .post(&format!("{}/contacts/selection", base(ws)))
            .json(&bad)
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert!(r.json::<Value>().get("selection_token").is_none());
    }
    assert_eq!(count(&pool, "select count(*) from contacts").await, 3);
}

/// The typo that would have run the action on exactly the people the user spared: with
/// `excluded_ids` defaulting to empty, an ignored `excluded_id` deletes the excluded contact.
#[sqlx::test]
async fn a_misspelt_excluded_ids_is_422_and_the_spared_contact_survives(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let ids = contacts(&app, ws, "c", 3).await;
    let token = select(&app, ws, json!({})).await;

    let r = bulk(
        &app,
        ws,
        "delete",
        json!({"selection_token": token, "excluded_id": [ids[0]]}),
    )
    .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 3);
    let spared: i64 = sqlx::query_scalar("select count(*) from contacts where id = $1")
        .bind(ids[0])
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(spared, 1, "the contact the user excluded must still exist");
    assert_eq!(
        count(
            &pool,
            "select count(*) from audit_log where action like 'contacts.bulk_%'"
        )
        .await,
        0
    );
}

/// Each of the three bulk body shapes refuses an unknown field, on every route that takes it.
#[sqlx::test]
async fn every_bulk_body_refuses_an_unknown_field(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let news = list(&app, ws, "news", false).await;
    contacts(&app, ws, "c", 2).await;
    let token = select(&app, ws, json!({})).await;
    for (action, extra) in [
        ("delete", json!({})),                     // BulkTarget
        ("tag", json!({"tag_id": vip})),           // BulkTag
        ("untag", json!({"tag_id": vip})),         // BulkTag
        ("subscribe", json!({"list_id": news})),   // BulkList
        ("unsubscribe", json!({"list_id": news})), // BulkList
    ] {
        let mut body = extra;
        body["selection_token"] = json!(token);
        body["bogus"] = json!(1);
        bulk(&app, ws, action, body)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);
    assert_eq!(count(&pool, "select count(*) from contact_tags").await, 0);
    assert_eq!(
        count(&pool, "select count(*) from list_memberships").await,
        0
    );
}

/// Spelling the fields out changed nothing for a correct client: `excluded_ids` present, empty or
/// absent all work, on a flat body as before.
#[sqlx::test]
async fn valid_bodies_are_accepted_with_or_without_excluded_ids(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let ids = contacts(&app, ws, "c", 3).await;
    let token = select(&app, ws, json!({})).await;
    let with = json!({"selection_token": token, "tag_id": vip, "excluded_ids": [ids[0]]});
    assert_eq!(affected(bulk(&app, ws, "tag", with).await).await, 2);
    let empty = json!({"selection_token": token, "tag_id": vip, "excluded_ids": []});
    assert_eq!(affected(bulk(&app, ws, "tag", empty).await).await, 1);
    let absent = json!({"selection_token": token, "tag_id": vip});
    assert_eq!(affected(bulk(&app, ws, "untag", absent).await).await, 3);
    let absent = json!({"selection_token": token});
    assert_eq!(affected(bulk(&app, ws, "delete", absent).await).await, 3);
}

/// Tags every contact in `ids` with `tag`, as a second editor would.
async fn apply_tag(app: &TestApp, ws: Uuid, tag: Uuid, ids: &[Uuid]) {
    for id in ids {
        app.put(&format!("{}/contacts/{id}/tags/{tag}", base(ws)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
}

/// One contact tagged `churn`, a token for `tag_id = churn`, then the other three tagged too.
/// Every one of the four existed before `as_of`, so the snapshot does not exclude them: only the
/// issue-time count can tell that the view widened.
async fn widened(pool: &PgPool, slug: &str) -> (Uuid, TestApp, Uuid, Vec<Uuid>, String) {
    let (ws, app) = workspace(pool, slug).await;
    let churn = tag(&app, ws, "churn").await;
    let ids = contacts(&app, ws, "c", 4).await;
    apply_tag(&app, ws, churn, &ids[..1]).await;

    let r = app
        .post(&format!("{}/contacts/selection", base(ws)))
        .json(&json!({"tag_id": churn}))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["selected"].as_u64().unwrap(), 1);
    let token = r.json::<Value>()["selection_token"]
        .as_str()
        .unwrap()
        .to_owned();

    apply_tag(&app, ws, churn, &ids[1..]).await;
    (ws, app, churn, ids, token)
}

/// The snapshot freezes creation time, not the filter: `push_filter` reads tags, memberships and
/// names as they are when the action runs. A contact that existed at `as_of` and was tagged
/// afterwards therefore matches. On `bulk/delete` that is unrecoverable and the person confirmed
/// a smaller number, so it is refused and the transaction rolls back.
#[sqlx::test]
async fn a_delete_whose_view_widened_is_refused_and_deletes_nothing(pool: PgPool) {
    let (ws, app, _churn, _ids, token) = widened(&pool, "acme").await;

    let r = bulk(&app, ws, "delete", json!({"selection_token": token})).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:selection-widened");
    // The refusal names both numbers: a count with no denominator cannot explain itself.
    let detail = r.json::<Value>()["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains('1') && detail.contains('4'), "{detail}");

    // Nothing was deleted, and no audit row claims otherwise.
    assert_eq!(count(&pool, "select count(*) from contacts").await, 4);
    assert_eq!(
        count(
            &pool,
            "select count(*) from audit_log where action = 'contacts.bulk_deleted'"
        )
        .await,
        0
    );
}

/// The reversible kinds proceed, because they can be undone and the gap is already their subject.
/// The response carries the issue-time count beside the resolved one, so the UI can say the view
/// widened rather than report four as a plain success.
#[sqlx::test]
async fn a_widened_untag_proceeds_and_reports_both_counts(pool: PgPool) {
    let (ws, app, churn, _ids, token) = widened(&pool, "acme").await;

    let r = bulk(
        &app,
        ws,
        "untag",
        json!({"selection_token": token, "tag_id": churn}),
    )
    .await;
    r.assert_status_ok();
    let body = r.json::<Value>();
    assert_eq!(body["issued"].as_u64().unwrap(), 1);
    assert_eq!(body["selected"].as_u64().unwrap(), 4);
    assert_eq!(body["affected"].as_u64().unwrap(), 4);
    assert_eq!(tagged(&pool, churn).await.len(), 0);
}

/// An id list names its contacts, so it has no filter to re-run and can only shrink. The widening
/// check must never fire on one.
#[sqlx::test]
async fn an_id_list_delete_is_never_refused_as_widened(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let ids = contacts(&app, ws, "c", 3).await;
    let body = json!({"ids": [ids[0], ids[1]]});
    assert_eq!(affected(bulk(&app, ws, "delete", body).await).await, 2);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 1);
}

/// N1: `selected` is counted after the exclusions and `issued` was counted without them, so
/// comparing the two directly let a widening of up to `excluded_ids.len()` through. Here the
/// token stands for two contacts, a third is tagged into the view afterwards, and one of the two
/// is unticked — the confirmation reads "Delete 1 contact". Before the adjustment this deleted
/// the contact nobody ever selected and answered 200.
#[sqlx::test]
async fn an_exclusion_cannot_buy_room_for_a_widening(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let churn = tag(&app, ws, "churn").await;
    let ids = contacts(&app, ws, "c", 4).await;
    apply_tag(&app, ws, churn, &ids[..2]).await;

    let r = app
        .post(&format!("{}/contacts/selection", base(ws)))
        .json(&json!({"tag_id": churn}))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["selected"].as_u64().unwrap(), 2);
    let token = r.json::<Value>()["selection_token"]
        .as_str()
        .unwrap()
        .to_owned();

    // A third contact, which was never in the view, is tagged into it.
    apply_tag(&app, ws, churn, &ids[2..3]).await;

    let body = json!({"selection_token": token, "excluded_ids": [ids[0]]});
    let r = bulk(&app, ws, "delete", body).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:selection-widened");
    assert_eq!(count(&pool, "select count(*) from contacts").await, 4);
}

/// The other half of that adjustment: an excluded id that was in the match and has since left it
/// is counted on both sides and cancels out, so the ordinary case — which is the only one the UI
/// can produce, because it only ever excludes rows drawn from the matching view — is not refused.
#[sqlx::test]
async fn an_excluded_id_that_left_the_match_is_not_a_false_refusal(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let churn = tag(&app, ws, "churn").await;
    let ids = contacts(&app, ws, "c", 3).await;
    apply_tag(&app, ws, churn, &ids).await;

    let token = select(&app, ws, json!({"tag_id": churn})).await;
    // The contact the user unticked loses the tag before the action runs.
    app.delete(&format!("{}/contacts/{}/tags/{churn}", base(ws), ids[0]))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let body = json!({"selection_token": token, "excluded_ids": [ids[0]]});
    assert_eq!(affected(bulk(&app, ws, "delete", body).await).await, 2);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 1);
}

/// The documented over-refusal, pinned on purpose rather than found later as a bug: an id that
/// was **never** in the match is the one thing that makes the bound conservative. Only an API
/// client can do this; the UI excludes rows it is showing.
#[sqlx::test]
async fn excluding_an_id_outside_the_match_is_refused_conservatively(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let churn = tag(&app, ws, "churn").await;
    let ids = contacts(&app, ws, "c", 3).await;
    apply_tag(&app, ws, churn, &ids[..2]).await;

    let token = select(&app, ws, json!({"tag_id": churn})).await;
    // ids[2] carries no tag, so it is not in the match at all.
    let body = json!({"selection_token": token, "excluded_ids": [ids[2]]});
    let r = bulk(&app, ws, "delete", body).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 3);
}

/// A subscribe is reversible and still refuses, because the axis is not reversibility: putting
/// someone on a list is what makes them mailable, and unsubscribing afterwards does not un-send.
#[sqlx::test]
async fn a_widened_subscribe_is_refused_and_adds_nobody(pool: PgPool) {
    let (ws, app, _churn, _ids, token) = widened(&pool, "acme").await;
    let news = list(&app, ws, "News", false).await;

    let body = json!({"selection_token": token, "list_id": news});
    let r = bulk(&app, ws, "subscribe", body).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:selection-widened");
    // The refusal names what did not happen, not what did.
    let detail = r.json::<Value>()["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains("nobody was added"), "{detail}");
    // No membership, and no audit row claiming one: the refusal rolls back both, so the log
    // cannot say a subscribe happened that did not.
    assert_eq!(
        count(&pool, "select count(*) from list_memberships").await,
        0
    );
    assert_eq!(
        count(
            &pool,
            "select count(*) from audit_log where action = 'contacts.bulk_subscribed'"
        )
        .await,
        0
    );
}
