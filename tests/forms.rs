// SPDX-License-Identifier: AGPL-3.0-only

//! The operator's side of signup forms: the model and the authenticated CRUD API.
//!
//! Every request here goes over HTTP with a hand-written JSON body, on purpose. The whole point
//! of `FormTarget` is the variants it does NOT have, and a Rust-level test cannot exercise a
//! variant that does not exist — it would not compile. The attack surface is an editor token
//! POSTing JSON, so that is what these tests use.

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::workspaces::roles::Role;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// A signed-in client for a fresh user, who is `role` in `ws`.
async fn client(pool: &PgPool, email: &str, ws: Uuid, role: Role) -> TestApp {
    let id = create_user(pool, email, PW, false).await;
    add_member(pool, ws, id, role).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    app
}

/// A workspace and a signed-in admin of it. Admin, not editor: these tests also create custom
/// fields, which takes admin, and `Role::Admin > Role::Editor` so it writes forms too.
async fn workspace(pool: &PgPool, slug: &str) -> (Uuid, TestApp) {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let ws = create_workspace(pool, owner, slug).await;
    let admin = client(pool, &format!("admin-{slug}@example.com"), ws, Role::Admin).await;
    (ws, admin)
}

fn forms_path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/forms")
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

/// The whole 422 body, so two refusals can be compared byte for byte.
fn body_of(r: &axum_test::TestResponse) -> Value {
    r.json::<Value>()
}

async fn create_list(app: &TestApp, ws: Uuid, name: &str) -> Uuid {
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/lists"))
        .json(&json!({"name": name}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn create_custom_field(app: &TestApp, ws: Uuid, key: &str) -> Uuid {
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/custom-fields"))
        .json(&json!({"key": key, "label": key, "kind": "text"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

/// A complete, valid request body. Tests override one key at a time, so a refusal is attributable
/// to the thing under test and not to some other field being wrong.
fn new_form(slug: &str, list: Uuid) -> Value {
    json!({
        "slug": slug,
        "name": "Newsletter signup",
        "list_ids": [list],
        "fields": [{"target": {"kind": "email"}, "required": true, "label": "Email"}],
        "headline": "Join the list",
        "button_label": "Subscribe",
    })
}

async fn count_forms(pool: &PgPool) -> i64 {
    sqlx::query_scalar("select count(*) from forms")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// `(action, target_id, metadata)` of every `form.*` audit row in the workspace, oldest first.
async fn form_audit(pool: &PgPool, ws: Uuid) -> Vec<(String, Option<String>, Value)> {
    sqlx::query_as(
        "select action, target_id, metadata from audit_log \
         where workspace_id = $1 and action like 'form.%' order by id",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn a_form_needs_at_least_one_list(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news").await;

    let mut body = new_form("news", list);
    body["list_ids"] = json!([]);
    let r = app.post(&forms_path(ws)).json(&body).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem_type(&r), "urn:nm:problem:validation");
    assert_eq!(r.json::<Value>()["errors"][0]["field"], "list_ids");
    assert_eq!(count_forms(&pool).await, 0);

    // The control: the same body with the list back is accepted, so the refusal above is the
    // empty list and not something else in the body.
    let r = app
        .post(&forms_path(ws))
        .json(&new_form("news", list))
        .await;
    r.assert_status(StatusCode::CREATED);

    // A PATCH cannot empty it either.
    let id = id_of(&r);
    let r = app
        .patch(&format!("{}/{id}", forms_path(ws)))
        .json(&json!({"list_ids": []}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}

/// The `form_lists` composite foreign key is what refuses this, which is why the answer must be
/// identical to the one for an id that belongs to nobody: the database cannot tell the two apart
/// and neither may the response.
#[sqlx::test]
async fn a_form_naming_another_workspaces_list_is_refused(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (other_ws, other) = workspace(&pool, "globex").await;
    let mine = create_list(&app, ws, "news").await;
    let theirs = create_list(&other, other_ws, "news").await;

    let mut body = new_form("news", mine);
    body["list_ids"] = json!([theirs]);
    let stolen = app.post(&forms_path(ws)).json(&body).await;
    stolen.assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    let mut body = new_form("news", mine);
    body["list_ids"] = json!([Uuid::now_v7()]);
    let nobodys = app.post(&forms_path(ws)).json(&body).await;
    nobodys.assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    // Nothing may disclose that `theirs` exists somewhere and the random id does not.
    assert_eq!(body_of(&stolen), body_of(&nobodys));
    assert_eq!(count_forms(&pool).await, 0);

    // The other workspace's own list still works there, so the refusal is cross-workspace reach
    // and not that list being unusable.
    let r = other
        .post(&forms_path(other_ws))
        .json(&new_form("news", theirs))
        .await;
    r.assert_status(StatusCode::CREATED);
}

/// `FormTarget` has no `Lists` variant, so this cannot be written against the Rust type at all —
/// a test that builds a `FormTarget` and asserts it is not `Lists` asserts nothing, and one that
/// tries to build `FormTarget::Lists` does not compile. The only way to make this test able to
/// fail is to go through the API with a raw body naming `"lists"`: adding the variant back, or
/// giving the enum a catch-all, turns the 422 below into a 201.
#[sqlx::test]
async fn a_form_field_targeting_lists_is_refused(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news").await;

    let mut body = new_form("news", list);
    body["fields"] = json!([
        {"target": {"kind": "email"}, "required": true, "label": "Email"},
        {"target": {"kind": "lists"}, "required": false, "label": "Lists"},
    ]);
    let r = app.post(&forms_path(ws)).json(&body).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem_type(&r), "urn:nm:problem:validation");
    // Refused, not quietly dropped: no form exists and no submitter can name a list.
    assert_eq!(count_forms(&pool).await, 0);

    // The control. The same two-field body with a target the enum does have is accepted, so the
    // refusal is the word `lists` and not the shape of the request.
    let mut body = new_form("news", list);
    body["fields"] = json!([
        {"target": {"kind": "email"}, "required": true, "label": "Email"},
        {"target": {"kind": "first_name"}, "required": false, "label": "First name"},
    ]);
    let r = app.post(&forms_path(ws)).json(&body).await;
    r.assert_status(StatusCode::CREATED);
    assert_eq!(
        r.json::<Value>()["fields"][1]["target"]["kind"],
        "first_name"
    );

    // And `lists` is refused on the way in through PATCH too, not only on create.
    let id = id_of(&r);
    let r = app
        .patch(&format!("{}/{id}", forms_path(ws)))
        .json(&json!({"fields": [
            {"target": {"kind": "email"}, "required": true, "label": "Email"},
            {"target": {"kind": "lists"}, "required": false, "label": "Lists"},
        ]}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test]
async fn a_form_field_may_target_a_custom_field(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news").await;
    create_custom_field(&app, ws, "company_size").await;

    let mut body = new_form("news", list);
    body["fields"] = json!([
        {"target": {"kind": "email"}, "required": true, "label": "Email"},
        {"target": {"kind": "custom", "value": "company_size"}, "required": false,
         "label": "Company size"},
    ]);
    let r = app.post(&forms_path(ws)).json(&body).await;
    r.assert_status(StatusCode::CREATED);
    assert_eq!(
        r.json::<Value>()["fields"][1]["target"],
        json!({"kind": "custom", "value": "company_size"})
    );
}

/// A form collecting into a custom field that is not there collects into nowhere, and the
/// operator finds out once the submissions have happened. Another workspace's field is as unknown
/// as one that never existed, and says so in the same bytes.
///
/// The two cases are compared with **one** key, asked for before and after another workspace
/// creates it. Comparing two different key strings would only prove the message omits the key,
/// which is not the property: the property is that the same key reads the same whether or not it
/// exists somewhere the caller cannot see.
#[sqlx::test]
async fn a_form_field_targeting_an_unknown_custom_field_is_refused(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (other_ws, other) = workspace(&pool, "globex").await;
    let list = create_list(&app, ws, "news").await;

    let body = {
        let mut body = new_form("news", list);
        body["fields"] = json!([
            {"target": {"kind": "email"}, "required": true, "label": "Email"},
            {"target": {"kind": "custom", "value": "shared_key"}, "required": false, "label": "x"},
        ]);
        body
    };
    // Nowhere at all.
    let nobodys = app.post(&forms_path(ws)).json(&body).await;
    nobodys.assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    create_custom_field(&other, other_ws, "shared_key").await;

    // Now it exists — in a workspace this caller is not a member of.
    let theirs = app.post(&forms_path(ws)).json(&body).await;
    theirs.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body_of(&theirs), body_of(&nobodys));
    assert_eq!(count_forms(&pool).await, 0);

    // The control: in the workspace that owns it, the very same body is accepted — so the
    // refusals above are "not yours", not "no custom target ever works".
    let theirs_list = create_list(&other, other_ws, "news").await;
    let mut ok = body.clone();
    ok["list_ids"] = json!([theirs_list]);
    let r = other.post(&forms_path(other_ws)).json(&ok).await;
    r.assert_status(StatusCode::CREATED);
}

/// `page::esc` escapes `& < > " '`; it is not a URL sanitiser and lets `javascript:` through
/// untouched. So the scheme is checked on the way in: a URL that could execute cannot be stored,
/// and Task 9 inherits a column it can put in a `Location` header without re-deciding this.
#[sqlx::test]
async fn a_redirect_url_must_be_an_absolute_http_url(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news").await;

    for bad in [
        "javascript:alert(1)",
        "JavaScript:alert(1)",
        "data:text/html,<script>alert(1)</script>",
        "/thanks",
        "thanks",
        "ftp://example.test/thanks",
        "",
    ] {
        let mut body = new_form("news", list);
        body["redirect_url"] = json!(bad);
        let r = app.post(&forms_path(ws)).json(&body).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            r.json::<Value>()["errors"][0]["field"],
            "redirect_url",
            "{bad:?}"
        );
    }
    assert_eq!(count_forms(&pool).await, 0);

    let mut last = None;
    for good in [
        "https://example.test/thanks",
        "http://example.test/thanks?a=1",
    ] {
        let mut body = new_form(&format!("ok-{}", good.len()), list);
        body["redirect_url"] = json!(good);
        let r = app.post(&forms_path(ws)).json(&body).await;
        r.assert_status(StatusCode::CREATED);
        assert_eq!(r.json::<Value>()["redirect_url"], good);
        last = Some(id_of(&r));
    }

    // PATCH is the same door and takes the same check.
    let id = last.expect("a form was created");
    let r = app
        .patch(&format!("{}/{id}", forms_path(ws)))
        .json(&json!({"redirect_url": "javascript:alert(1)"}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test]
async fn two_forms_in_one_workspace_cannot_share_a_slug(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news").await;

    let r = app
        .post(&forms_path(ws))
        .json(&new_form("signup", list))
        .await;
    r.assert_status(StatusCode::CREATED);
    let first = id_of(&r);

    let mut body = new_form("signup", list);
    body["name"] = json!("A different form, the same slug");
    let r = app.post(&forms_path(ws)).json(&body).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:form-slug-taken");
    assert_eq!(count_forms(&pool).await, 1);

    // Renaming a second form onto the first one's slug is the same conflict.
    let r = app
        .post(&forms_path(ws))
        .json(&new_form("other", list))
        .await;
    r.assert_status(StatusCode::CREATED);
    let second = id_of(&r);
    let r = app
        .patch(&format!("{}/{second}", forms_path(ws)))
        .json(&json!({"slug": "signup"}))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:form-slug-taken");
    // The first form kept its slug.
    let r = app.get(&format!("{}/{first}", forms_path(ws))).await;
    assert_eq!(r.json::<Value>()["slug"], "signup");
}

#[sqlx::test]
async fn the_same_slug_in_two_workspaces_is_fine(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (other_ws, other) = workspace(&pool, "globex").await;
    let mine = create_list(&app, ws, "news").await;
    let theirs = create_list(&other, other_ws, "news").await;

    let r = app
        .post(&forms_path(ws))
        .json(&new_form("signup", mine))
        .await;
    r.assert_status(StatusCode::CREATED);
    let r = other
        .post(&forms_path(other_ws))
        .json(&new_form("signup", theirs))
        .await;
    r.assert_status(StatusCode::CREATED);
    assert_eq!(count_forms(&pool).await, 2);
}

#[sqlx::test]
async fn another_workspaces_form_is_404_not_403(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (other_ws, other) = workspace(&pool, "globex").await;
    let theirs = create_list(&other, other_ws, "news").await;
    let r = other
        .post(&forms_path(other_ws))
        .json(&new_form("signup", theirs))
        .await;
    r.assert_status(StatusCode::CREATED);
    let their_form = id_of(&r);

    // Reaching for it through our own workspace's path: the id exists, but not here.
    for r in [
        app.get(&format!("{}/{their_form}", forms_path(ws))).await,
        app.delete(&format!("{}/{their_form}", forms_path(ws)))
            .await,
    ] {
        r.assert_status(StatusCode::NOT_FOUND);
        assert_eq!(problem_type(&r), "urn:nm:problem:not-found");
    }
    let r = app
        .patch(&format!("{}/{their_form}", forms_path(ws)))
        .json(&json!({"name": "mine now"}))
        .await;
    r.assert_status(StatusCode::NOT_FOUND);

    // Reaching through their workspace's path is the same answer: not a member is not-found too.
    let r = app
        .get(&format!("{}/{their_form}", forms_path(other_ws)))
        .await;
    r.assert_status(StatusCode::NOT_FOUND);

    // A real id in the right workspace proves the 404s above are the workspace and not the id.
    let r = other
        .get(&format!("{}/{their_form}", forms_path(other_ws)))
        .await;
    r.assert_status_ok();
    // And their form is untouched by our DELETE.
    assert_eq!(count_forms(&pool).await, 1);
}

#[sqlx::test]
async fn a_viewer_may_read_forms_and_an_editor_may_write_them(pool: PgPool) {
    let (ws, admin) = workspace(&pool, "acme").await;
    let list = create_list(&admin, ws, "news").await;
    let editor = client(&pool, "editor@example.com", ws, Role::Editor).await;
    let viewer = client(&pool, "viewer@example.com", ws, Role::Viewer).await;

    let r = editor
        .post(&forms_path(ws))
        .json(&new_form("signup", list))
        .await;
    r.assert_status(StatusCode::CREATED);
    let id = id_of(&r);

    // The viewer reads both the collection and the one form.
    let r = viewer.get(&forms_path(ws)).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["data"][0]["slug"], "signup");
    let r = viewer.get(&format!("{}/{id}", forms_path(ws))).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["list_ids"][0], json!(list));

    // And writes nothing.
    let r = viewer
        .post(&forms_path(ws))
        .json(&new_form("another", list))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    let r = viewer
        .patch(&format!("{}/{id}", forms_path(ws)))
        .json(&json!({"name": "renamed"}))
        .await;
    r.assert_status(StatusCode::FORBIDDEN);
    let r = viewer.delete(&format!("{}/{id}", forms_path(ws))).await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(count_forms(&pool).await, 1);

    // The editor may delete it.
    let r = editor.delete(&format!("{}/{id}", forms_path(ws))).await;
    r.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(count_forms(&pool).await, 0);
}

#[sqlx::test]
async fn a_slug_is_lowercase_url_safe_and_bounded(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news").await;

    for bad in [
        "a",
        "-leading",
        "Upper",
        "with space",
        "with_underscore",
        "white/space",
        "..",
        &"a".repeat(64),
    ] {
        let r = app.post(&forms_path(ws)).json(&new_form(bad, list)).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(r.json::<Value>()["errors"][0]["field"], "slug", "{bad:?}");
    }
    for good in ["ab", "0-9", &"a".repeat(63)] {
        let r = app.post(&forms_path(ws)).json(&new_form(good, list)).await;
        r.assert_status(StatusCode::CREATED);
    }
}

/// `fields` is the form's whole contract with a stranger: it must always collect an address, and
/// never ask for the same thing twice.
#[sqlx::test]
async fn a_form_always_collects_email_exactly_once(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let list = create_list(&app, ws, "news").await;

    for fields in [
        json!([]),
        json!([{"target": {"kind": "first_name"}, "required": true, "label": "First name"}]),
        json!([
            {"target": {"kind": "email"}, "required": true, "label": "Email"},
            {"target": {"kind": "email"}, "required": true, "label": "Email again"},
        ]),
    ] {
        let mut body = new_form("news", list);
        body["fields"] = fields.clone();
        let r = app.post(&forms_path(ws)).json(&body).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            r.json::<Value>()["errors"][0]["field"],
            "fields",
            "{fields}"
        );
    }
    assert_eq!(count_forms(&pool).await, 0);
}

/// A PATCH replaces the lists it names and leaves everything else alone.
#[sqlx::test]
async fn updating_the_lists_replaces_them(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let news = create_list(&app, ws, "news").await;
    let offers = create_list(&app, ws, "offers").await;

    let r = app
        .post(&forms_path(ws))
        .json(&new_form("signup", news))
        .await;
    r.assert_status(StatusCode::CREATED);
    let id = id_of(&r);

    let r = app
        .patch(&format!("{}/{id}", forms_path(ws)))
        .json(&json!({"list_ids": [offers]}))
        .await;
    r.assert_status_ok();
    let v = r.json::<Value>();
    assert_eq!(v["list_ids"], json!([offers]));
    // Untouched by a PATCH that named only `list_ids`.
    assert_eq!(v["slug"], "signup");
    assert_eq!(v["headline"], "Join the list");

    // A list named twice lands once.
    let r = app
        .patch(&format!("{}/{id}", forms_path(ws)))
        .json(&json!({"list_ids": [news, offers, news]}))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["list_ids"], json!([news, offers]));
}

/// Deleting a list the form points at takes the form's reference with it. The form survives — and
/// is then a form with no lists, which Task 9 must not accept submissions for.
#[sqlx::test]
async fn deleting_a_list_leaves_the_form_without_it(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let news = create_list(&app, ws, "news").await;
    let r = app
        .post(&forms_path(ws))
        .json(&new_form("signup", news))
        .await;
    r.assert_status(StatusCode::CREATED);
    let id = id_of(&r);

    let r = app
        .delete(&format!("/api/v1/workspaces/{ws}/lists/{news}"))
        .await;
    r.assert_status(StatusCode::NO_CONTENT);

    let r = app.get(&format!("{}/{id}", forms_path(ws))).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["list_ids"], json!([]));
}

/// Every form write leaves an audit row carrying the form's **whole definition**, not a list of
/// the keys the request named.
///
/// The reason is `consent_log.source_ref`, which holds a form id. A consent row therefore says
/// "this person submitted form X" and nothing more; if an operator later edits X's lists or its
/// copy, and the only record of the edit is "list_ids changed", then nothing anywhere says what X
/// asked for at the moment the person agreed — and the consent log is append-only precisely so
/// that evidence survives.
///
/// The load-bearing assertion is the last one: the `form.created` row still names the list the
/// form had at creation, **after** a later edit has moved the form to another list. A
/// metadata-is-the-key-names implementation passes every other assertion here and fails that one.
#[sqlx::test]
async fn form_writes_are_audited_with_the_definition_in_force(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let news = create_list(&app, ws, "news").await;
    let offers = create_list(&app, ws, "offers").await;

    assert!(form_audit(&pool, ws).await.is_empty(), "nothing yet");

    let r = app
        .post(&forms_path(ws))
        .json(&new_form("signup", news))
        .await;
    r.assert_status(StatusCode::CREATED);
    let id = id_of(&r);

    // An edit that changes both what the form collects into and what it says.
    let r = app
        .patch(&format!("{}/{id}", forms_path(ws)))
        .json(&json!({"list_ids": [offers], "headline": "Different words"}))
        .await;
    r.assert_status_ok();

    let r = app.delete(&format!("{}/{id}", forms_path(ws))).await;
    r.assert_status(StatusCode::NO_CONTENT);

    let rows = form_audit(&pool, ws).await;
    let actions: Vec<&str> = rows.iter().map(|(a, _, _)| a.as_str()).collect();
    assert_eq!(actions, ["form.created", "form.updated", "form.deleted"]);
    for (action, target, _) in &rows {
        assert_eq!(target.as_deref(), Some(id.to_string().as_str()), "{action}");
    }

    let (created, updated, deleted) = (&rows[0].2, &rows[1].2, &rows[2].2);
    // Each row carries the definition as it stood when that write happened.
    assert_eq!(created["list_ids"], json!([news]));
    assert_eq!(created["headline"], "Join the list");
    assert_eq!(updated["list_ids"], json!([offers]));
    assert_eq!(updated["headline"], "Different words");
    // The delete row is the last definition, read before the row stopped existing.
    assert_eq!(deleted["list_ids"], json!([offers]));
    assert_eq!(deleted["slug"], "signup");
    // Every row says what the form collected, so a consent row pointing here can be read.
    for (action, _, meta) in &rows {
        assert_eq!(
            meta["fields"][0]["target"],
            json!({"kind": "email"}),
            "{action}"
        );
    }

    // The point of the whole exercise: the edit did not rewrite history. `form.created` still
    // names `news`, which is what the people who submitted before the edit actually agreed to.
    assert_ne!(created["list_ids"], updated["list_ids"]);
    assert_eq!(created["list_ids"], json!([news]));
}

/// A refused write leaves no audit row: the log records what happened, not what was attempted.
#[sqlx::test]
async fn a_refused_form_write_is_not_audited(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (other_ws, other) = workspace(&pool, "globex").await;
    let list = create_list(&app, ws, "news").await;
    let theirs = create_list(&other, other_ws, "news").await;

    // A cross-workspace list: refused by the foreign key, inside the transaction.
    let mut body = new_form("signup", list);
    body["list_ids"] = json!([theirs]);
    app.post(&forms_path(ws))
        .json(&body)
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    // A viewer: refused before anything is written.
    let viewer = client(&pool, "viewer@example.com", ws, Role::Viewer).await;
    viewer
        .post(&forms_path(ws))
        .json(&new_form("signup", list))
        .await
        .assert_status(StatusCode::FORBIDDEN);

    assert!(form_audit(&pool, ws).await.is_empty());

    // The control: a write that succeeds is audited, so the emptiness above is the refusals and
    // not a query that never matches anything.
    app.post(&forms_path(ws))
        .json(&new_form("signup", list))
        .await
        .assert_status(StatusCode::CREATED);
    assert_eq!(form_audit(&pool, ws).await.len(), 1);
}
