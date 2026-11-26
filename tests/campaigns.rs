// SPDX-License-Identifier: AGPL-3.0-only

//! Migration 0033: `campaigns`, `messages` and `jobs.campaign_id`. Raw SQL on purpose: these are
//! the database's guarantees, and a handler that happens to check first proves nothing.

use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(pool: &PgPool, slug: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into workspaces (id, slug, name) values ($1, $2, $2)")
        .bind(id)
        .bind(slug)
        .execute(pool)
        .await
        .expect("insert workspace");
    id
}

async fn campaign(pool: &PgPool, ws: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into campaigns (id, workspace_id, name) values ($1, $2, 'c')")
        .bind(id)
        .bind(ws)
        .execute(pool)
        .await
        .expect("insert campaign");
    id
}

async fn message(
    pool: &PgPool,
    ws: Uuid,
    campaign: Uuid,
    contact: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "insert into messages (id, workspace_id, campaign_id, contact_id) values ($1, $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(campaign)
    .bind(contact)
    .execute(pool)
    .await
    .map(|_| ())
}

async fn job(
    pool: &PgPool,
    ws: Uuid,
    kind: &str,
    campaign: Option<Uuid>,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into jobs (id, workspace_id, kind, payload, campaign_id)
         values ($1, $2, $3, '{}', $4)",
    )
    .bind(id)
    .bind(ws)
    .bind(kind)
    .bind(campaign)
    .execute(pool)
    .await
    .map(|_| id)
}

/// SQLSTATE and constraint name, which is what a later task matches on.
fn refusal(err: &sqlx::Error) -> (String, String) {
    let db = err
        .as_database_error()
        .unwrap_or_else(|| panic!("not a database error: {err}"));
    (
        db.code().map(|c| c.into_owned()).unwrap_or_default(),
        db.constraint().unwrap_or_default().to_owned(),
    )
}

#[sqlx::test]
async fn a_second_message_for_one_contact_in_one_campaign_is_refused(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let c = campaign(&pool, ws).await;
    let contact = Uuid::now_v7();

    message(&pool, ws, c, contact).await.expect("first message");
    let err = message(&pool, ws, c, contact).await.expect_err("second");
    assert_eq!(
        refusal(&err),
        ("23505".into(), "messages_campaign_contact_key".into())
    );

    // Control: the same contact in another campaign is a different recipient row.
    let other = campaign(&pool, ws).await;
    message(&pool, ws, other, contact)
        .await
        .expect("other campaign");
}

#[sqlx::test]
async fn a_message_cannot_name_another_workspaces_campaign(pool: PgPool) {
    let a = workspace(&pool, "a").await;
    let b = workspace(&pool, "b").await;
    let campaign_of_a = campaign(&pool, a).await;

    let err = message(&pool, b, campaign_of_a, Uuid::now_v7())
        .await
        .expect_err("cross-workspace message");
    assert_eq!(
        refusal(&err),
        ("23503".into(), "messages_campaign_fk".into())
    );

    // Control: the same row under its own workspace is accepted.
    message(&pool, a, campaign_of_a, Uuid::now_v7())
        .await
        .expect("own workspace");
}

#[sqlx::test]
async fn a_campaign_with_messages_cannot_be_deleted(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let with = campaign(&pool, ws).await;
    let without = campaign(&pool, ws).await;
    message(&pool, ws, with, Uuid::now_v7())
        .await
        .expect("message");

    let err = sqlx::query("delete from campaigns where id = $1")
        .bind(with)
        .execute(&pool)
        .await
        .expect_err("delete with messages");
    assert_eq!(
        refusal(&err),
        ("23503".into(), "messages_campaign_fk".into())
    );

    // Control: a campaign with no messages deletes.
    sqlx::query("delete from campaigns where id = $1")
        .bind(without)
        .execute(&pool)
        .await
        .expect("delete without messages");
}

#[sqlx::test]
async fn a_campaign_job_must_name_its_campaign(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let c = campaign(&pool, ws).await;

    for kind in ["campaign_launch", "campaign_test"] {
        let err = job(&pool, ws, kind, None).await.expect_err(kind);
        assert_eq!(
            refusal(&err),
            ("23514".into(), "jobs_campaign_shape".into())
        );
        job(&pool, ws, kind, Some(c)).await.expect(kind);
    }

    // Control: every other kind keeps a null campaign.
    job(&pool, ws, "confirmation_email_other", None)
        .await
        .expect("other kind");
}

/// Invariant 9 for `jobs`: the key is composite on `workspace_id`, so a launch job of one workspace
/// cannot point the worker at another workspace's campaign.
#[sqlx::test]
async fn a_campaign_job_cannot_name_another_workspaces_campaign(pool: PgPool) {
    let a = workspace(&pool, "a").await;
    let b = workspace(&pool, "b").await;
    let campaign_of_a = campaign(&pool, a).await;

    let err = job(&pool, b, "campaign_launch", Some(campaign_of_a))
        .await
        .expect_err("cross-workspace campaign job");
    assert_eq!(refusal(&err), ("23503".into(), "jobs_campaign_fk".into()));

    // Control: the same job under its own workspace is accepted.
    job(&pool, a, "campaign_launch", Some(campaign_of_a))
        .await
        .expect("own workspace");
}

#[sqlx::test]
async fn deleting_a_draft_takes_its_fresh_test_jobs_with_it(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let c = campaign(&pool, ws).await;
    let j = job(&pool, ws, "campaign_test", Some(c)).await.expect("job");

    // Control: the retention trigger refuses a direct delete of a job younger than a day, so the
    // cascade below is admitted by `pg_trigger_depth() > 1` and by nothing else.
    sqlx::query("delete from jobs where id = $1")
        .bind(j)
        .execute(&pool)
        .await
        .expect_err("a fresh job is retained");

    sqlx::query("delete from campaigns where id = $1")
        .bind(c)
        .execute(&pool)
        .await
        .expect("delete the draft");
    let left: i64 = sqlx::query_scalar("select count(*) from jobs where id = $1")
        .bind(j)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(left, 0, "the test job went with its campaign");
}

// ---------------------------------------------------------------------------------------------
// The API (task R3). Everything above this line is the schema's own guarantees, in raw SQL.

mod common;

use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::workspaces::roles::Role;
use serde_json::{Value, json};

struct Tenant {
    id: Uuid,
    admin: TestApp,
    editor: TestApp,
    viewer: TestApp,
    domain: Uuid,
    template: Uuid,
    list: Uuid,
}

const ADDRESS: &str = "1 Main St, Springfield";

async fn tenant(pool: &PgPool, slug: &str) -> Tenant {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let id = create_workspace(pool, owner, slug).await;
    let member = |role: Role| {
        let email = format!("{role:?}-{slug}@example.com").to_lowercase();
        async move {
            let user = create_user(pool, &email, PW, false).await;
            add_member(pool, id, user, role).await;
            let mut app = TestApp::new(pool.clone()).await;
            app.login(&email, PW).await;
            app
        }
    };
    let (admin, editor, viewer) = (
        member(Role::Admin).await,
        member(Role::Editor).await,
        member(Role::Viewer).await,
    );

    // A brand kit with a postal address, a sending domain whose checks pass, a template, a list.
    admin
        .put(&format!("/api/v1/workspaces/{id}/brand-kit"))
        .json(&json!({"logo_url": null, "palette": {}, "font_stack": null,
                      "footer": {"address": ADDRESS}}))
        .await
        .assert_status_ok();
    let r = admin
        .post(&format!("/api/v1/workspaces/{id}/sending-domains"))
        .json(&json!({"domain": "mail.example.com", "signing": "relay"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let domain = id_of(&r);
    sqlx::query("update sending_domains set checks = $2 where id = $1")
        .bind(domain)
        .bind(
            json!({"spf": {"status": "pass"}, "dkim": {"status": "pass"},
                     "dmarc": {"status": "pass"}, "return_path": {"status": "pass"}}),
        )
        .execute(pool)
        .await
        .expect("checks");
    let r = editor
        .post(&format!("/api/v1/workspaces/{id}/templates"))
        .json(
            &json!({"name": "welcome", "kind": "blocks", "tree": {"blocks": [
            {"type": "Text", "runs": [{"type": "Tag", "source": "{{ first_name }}"}]}]}}),
        )
        .await;
    r.assert_status(StatusCode::CREATED);
    let template = id_of(&r);
    let r = admin
        .post(&format!("/api/v1/workspaces/{id}/lists"))
        .json(&json!({"name": "news", "double_optin": false}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let list = id_of(&r);
    Tenant {
        id,
        admin,
        editor,
        viewer,
        domain,
        template,
        list,
    }
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

fn campaigns_path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/campaigns")
}

fn campaign_path(ws: Uuid, id: Uuid) -> String {
    format!("{}/{id}", campaigns_path(ws))
}

fn verb(ws: Uuid, id: Uuid, verb: &str) -> String {
    format!("{}/{verb}", campaign_path(ws, id))
}

fn error_fields(r: &axum_test::TestResponse) -> Vec<String> {
    r.json::<Value>()["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("no errors array: {}", r.text()))
        .iter()
        .map(|e| e["field"].as_str().unwrap().to_owned())
        .collect()
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

/// The body of a complete campaign: everything `prepare` asks for.
fn complete(t: &Tenant, name: &str) -> Value {
    json!({
        "name": name,
        "list_ids": [t.list],
        "sending_domain_id": t.domain,
        "from_local_part": "news",
        "from_name": "Acme",
        "reply_to": "hello@example.com",
        "variants": [{"subject": "Hi {{ first_name }}", "template_id": t.template,
                      "from_name": null}],
    })
}

async fn draft(t: &Tenant, name: &str) -> Uuid {
    let r = t
        .editor
        .post(&campaigns_path(t.id))
        .json(&complete(t, name))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

/// A second blocks template, for a second variant.
async fn second_template(t: &Tenant) -> Uuid {
    let r = t
        .editor
        .post(&format!("/api/v1/workspaces/{}/templates", t.id))
        .json(&json!({"name": format!("second-{}", Uuid::now_v7()), "kind": "blocks", "tree": {"blocks": [
            {"type": "Text", "runs": [{"type": "Tag", "source": "{{ last_name }}"}]}]}}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

/// Two variants of one campaign at `percent`.
async fn ab_draft(t: &Tenant, percent: i64) -> Uuid {
    let second = second_template(t).await;
    let mut body = complete(t, "ab");
    body["variants"] = json!([
        {"subject": "One", "template_id": t.template, "from_name": null},
        {"subject": "Two", "template_id": second, "from_name": null},
    ]);
    body["ab_test_percent"] = json!(percent);
    let r = t.editor.post(&campaigns_path(t.id)).json(&body).await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

fn schedule_body(local: Option<&str>, mode: &str) -> Value {
    json!({"mode": mode, "zone": "America/New_York", "local": local})
}

fn in_days_at(days: i64, hour: u32) -> String {
    format!(
        "{}T{hour:02}:00",
        (Utc::now() + Duration::days(days)).format("%Y-%m-%d")
    )
}

async fn set_state(pool: &PgPool, id: Uuid, state: &str) {
    sqlx::query("update campaigns set state = $2 where id = $1")
        .bind(id)
        .bind(state)
        .execute(pool)
        .await
        .expect("state");
}

async fn state_of(pool: &PgPool, id: Uuid) -> String {
    sqlx::query_scalar("select state from campaigns where id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("state")
}

async fn jobs_of(pool: &PgPool, id: Uuid, kind: &str) -> Vec<(Uuid, DateTime<Utc>, Value)> {
    sqlx::query_as(
        "select id, run_after, payload from jobs where campaign_id = $1 and kind = $2 \
         order by id",
    )
    .bind(id)
    .bind(kind)
    .fetch_all(pool)
    .await
    .expect("jobs")
}

#[sqlx::test]
async fn another_workspaces_campaign_is_404(pool: PgPool) {
    let mine = tenant(&pool, "acme").await;
    let theirs = tenant(&pool, "other").await;
    let id = draft(&mine, "secret-launch").await;

    // Every route, asked for through the other workspace's own path by its own ADMIN, so a role
    // gate cannot be what answers: 404, never 403 and never the campaign.
    let none = json!({});
    let sched = schedule_body(Some(&in_days_at(2, 9)), "instant");
    let test = json!({"addresses": ["a@example.com"]});
    let winner = json!({"variant": 0});
    let responses = [
        theirs.admin.get(&campaign_path(theirs.id, id)).await,
        theirs
            .admin
            .patch(&campaign_path(theirs.id, id))
            .json(&json!({"name": "stolen"}))
            .await,
        theirs.admin.delete(&campaign_path(theirs.id, id)).await,
        theirs
            .admin
            .post(&verb(theirs.id, id, "preview"))
            .json(&none)
            .await,
        theirs.admin.post(&verb(theirs.id, id, "estimate")).await,
        theirs
            .admin
            .post(&verb(theirs.id, id, "test"))
            .json(&test)
            .await,
        theirs
            .admin
            .post(&verb(theirs.id, id, "schedule"))
            .json(&sched)
            .await,
        theirs.admin.post(&verb(theirs.id, id, "unschedule")).await,
        theirs.admin.post(&verb(theirs.id, id, "pause")).await,
        theirs.admin.post(&verb(theirs.id, id, "resume")).await,
        theirs.admin.post(&verb(theirs.id, id, "cancel")).await,
        theirs
            .admin
            .post(&verb(theirs.id, id, "winner"))
            .json(&winner)
            .await,
    ];
    for r in responses {
        r.assert_status(StatusCode::NOT_FOUND);
        assert!(!r.text().contains("secret-launch"), "leaked: {}", r.text());
    }
    // Asked for through MY path by a member of the other workspace: the membership lookup fails.
    theirs
        .viewer
        .get(&campaign_path(mine.id, id))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // And nothing above changed it.
    assert_eq!(state_of(&pool, id).await, "draft");

    // THE LIST: its `where` is a statement of its own, and a 404 on the by-id route is no comfort
    // when this one hands over every name.
    let r = theirs.viewer.get(&campaigns_path(theirs.id)).await;
    r.assert_status_ok();
    assert!(
        r.json::<Value>()["data"].as_array().unwrap().is_empty(),
        "{}",
        r.text()
    );
    assert!(!r.text().contains("secret-launch"));
    // The control: the owning workspace does see it, so the assertion above is about the filter.
    let r = mine.viewer.get(&campaigns_path(mine.id)).await;
    assert!(r.text().contains("secret-launch"), "{}", r.text());
}

#[sqlx::test]
async fn every_mutating_verb_is_role_gated(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    let none = json!({});
    let sched = schedule_body(Some(&in_days_at(2, 9)), "instant");
    let test = json!({"addresses": ["a@example.com"]});
    let winner = json!({"variant": 0});
    let forbidden = StatusCode::FORBIDDEN;

    // Viewer: create, patch, delete, test, pause.
    t.viewer
        .post(&campaigns_path(t.id))
        .json(&complete(&t, "v"))
        .await
        .assert_status(forbidden);
    t.viewer
        .patch(&campaign_path(t.id, id))
        .json(&json!({"name": "renamed"}))
        .await
        .assert_status(forbidden);
    t.viewer
        .delete(&campaign_path(t.id, id))
        .await
        .assert_status(forbidden);
    t.viewer
        .post(&verb(t.id, id, "test"))
        .json(&test)
        .await
        .assert_status(forbidden);
    t.viewer
        .post(&verb(t.id, id, "pause"))
        .await
        .assert_status(forbidden);
    // A viewer may read.
    t.viewer
        .get(&campaign_path(t.id, id))
        .await
        .assert_status_ok();
    t.viewer.get(&campaigns_path(t.id)).await.assert_status_ok();
    t.viewer
        .post(&verb(t.id, id, "estimate"))
        .await
        .assert_status_ok();
    t.viewer
        .post(&verb(t.id, id, "preview"))
        .json(&none)
        .await
        .assert_status_ok();

    // Editor: schedule, unschedule, resume, cancel and winner are the admin's.
    t.editor
        .post(&verb(t.id, id, "schedule"))
        .json(&sched)
        .await
        .assert_status(forbidden);
    t.editor
        .post(&verb(t.id, id, "unschedule"))
        .await
        .assert_status(forbidden);
    t.editor
        .post(&verb(t.id, id, "resume"))
        .await
        .assert_status(forbidden);
    t.editor
        .post(&verb(t.id, id, "cancel"))
        .await
        .assert_status(forbidden);
    t.editor
        .post(&verb(t.id, id, "winner"))
        .json(&winner)
        .await
        .assert_status(forbidden);
    assert_eq!(
        state_of(&pool, id).await,
        "draft",
        "a refused verb changed nothing"
    );

    // Editor: pause is theirs.
    set_state(&pool, id, "sending").await;
    let r = t.editor.post(&verb(t.id, id, "pause")).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["state"], "paused");
    // ...and so is the rest of the draft's life.
    let other = draft(&t, "e").await;
    t.editor
        .patch(&campaign_path(t.id, other))
        .json(&json!({"name": "renamed"}))
        .await
        .assert_status_ok();
    t.editor
        .post(&verb(t.id, other, "test"))
        .json(&test)
        .await
        .assert_status(StatusCode::ACCEPTED);
    t.editor
        .delete(&campaign_path(t.id, other))
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

#[sqlx::test]
async fn content_is_editable_only_in_draft(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    t.admin
        .post(&verb(t.id, id, "schedule"))
        .json(&schedule_body(Some(&in_days_at(2, 9)), "instant"))
        .await
        .assert_status_ok();
    let r = t
        .editor
        .patch(&campaign_path(t.id, id))
        .json(&json!({"name": "renamed"}))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:campaign-state");
    // Unscheduled, it is a draft again and editable.
    t.admin
        .post(&verb(t.id, id, "unschedule"))
        .await
        .assert_status_ok();
    t.editor
        .patch(&campaign_path(t.id, id))
        .json(&json!({"name": "renamed"}))
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn repeating_a_transition_is_200_and_a_wrong_one_is_409(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    let sched = schedule_body(Some(&in_days_at(2, 9)), "instant");
    let state =
        |r: &axum_test::TestResponse| r.json::<Value>()["state"].as_str().unwrap().to_owned();
    let conflict = |r: axum_test::TestResponse| {
        r.assert_status(StatusCode::CONFLICT);
        assert_eq!(problem_type(&r), "urn:nm:problem:campaign-state");
    };

    // draft: pausing, resuming, cancelling and picking a winner are wrong moves; unscheduling a
    // draft is a repeat (it is already where unscheduling goes).
    conflict(t.editor.post(&verb(t.id, id, "pause")).await);
    conflict(t.admin.post(&verb(t.id, id, "resume")).await);
    conflict(t.admin.post(&verb(t.id, id, "cancel")).await);
    conflict(
        t.admin
            .post(&verb(t.id, id, "winner"))
            .json(&json!({"variant": 0}))
            .await,
    );
    assert_eq!(
        state(&t.admin.post(&verb(t.id, id, "unschedule")).await),
        "draft"
    );

    // schedule twice: 200 both times, and one launch job.
    let first = t.admin.post(&verb(t.id, id, "schedule")).json(&sched).await;
    first.assert_status_ok();
    let again = t.admin.post(&verb(t.id, id, "schedule")).json(&sched).await;
    again.assert_status_ok();
    assert_eq!(
        first.json::<Value>()["schedule"],
        again.json::<Value>()["schedule"]
    );
    assert_eq!(jobs_of(&pool, id, "campaign_launch").await.len(), 1);
    // scheduled: pausing and resuming are wrong.
    conflict(t.editor.post(&verb(t.id, id, "pause")).await);
    conflict(t.admin.post(&verb(t.id, id, "resume")).await);

    // sending <-> paused, each repeated.
    set_state(&pool, id, "sending").await;
    for _ in 0..2 {
        assert_eq!(
            state(&t.editor.post(&verb(t.id, id, "pause")).await),
            "paused"
        );
    }
    for _ in 0..2 {
        assert_eq!(
            state(&t.admin.post(&verb(t.id, id, "resume")).await),
            "sending"
        );
    }
    // sending: scheduling and unscheduling are wrong.
    conflict(t.admin.post(&verb(t.id, id, "schedule")).json(&sched).await);
    conflict(t.admin.post(&verb(t.id, id, "unschedule")).await);

    // cancel, twice; then nothing moves a cancelled campaign.
    for _ in 0..2 {
        assert_eq!(
            state(&t.admin.post(&verb(t.id, id, "cancel")).await),
            "cancelled"
        );
    }
    conflict(t.editor.post(&verb(t.id, id, "pause")).await);
    conflict(t.admin.post(&verb(t.id, id, "resume")).await);
    conflict(t.admin.post(&verb(t.id, id, "schedule")).json(&sched).await);
    for finished in ["sent", "failed"] {
        set_state(&pool, id, finished).await;
        conflict(t.admin.post(&verb(t.id, id, "cancel")).await);
    }
}

#[sqlx::test]
async fn cancelling_works_from_scheduled_sending_and_paused(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    for from in ["scheduled", "sending", "paused"] {
        let id = draft(&t, "c").await;
        set_state(&pool, id, from).await;
        let r = t.admin.post(&verb(t.id, id, "cancel")).await;
        r.assert_status_ok();
        let v = r.json::<Value>();
        assert_eq!(v["state"], "cancelled", "{from}");
        assert!(
            v["finished_at"].is_string(),
            "{from}: a cancelled campaign has an end"
        );
    }
}

#[sqlx::test]
async fn scheduling_without_a_postal_address_is_409(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    t.admin
        .put(&format!("/api/v1/workspaces/{}/brand-kit", t.id))
        .json(&json!({"logo_url": null, "palette": {}, "font_stack": null,
                      "footer": {"address": "   "}}))
        .await
        .assert_status_ok();
    let r = t
        .admin
        .post(&verb(t.id, id, "schedule"))
        .json(&schedule_body(None, "instant"))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:postal-address-required");
    // A test send is refused the same way, and nothing was queued by either.
    let r = t
        .editor
        .post(&verb(t.id, id, "test"))
        .json(&json!({"addresses": ["a@example.com"]}))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:postal-address-required");
    assert_eq!(state_of(&pool, id).await, "draft");
    assert!(jobs_of(&pool, id, "campaign_test").await.is_empty());
}

#[sqlx::test]
async fn scheduling_from_an_unverified_unacknowledged_domain_is_409(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    sqlx::query("update sending_domains set checks = '{}' where id = $1")
        .bind(t.domain)
        .execute(&pool)
        .await
        .unwrap();
    let r = t
        .admin
        .post(&verb(t.id, id, "schedule"))
        .json(&schedule_body(None, "instant"))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:sending-domain-unverified");
    assert!(r.text().contains("mail.example.com"), "{}", r.text());
    // An admin's acknowledgement is the other way through the gate.
    sqlx::query("update sending_domains set send_unverified_acknowledged_at = now() where id = $1")
        .bind(t.domain)
        .execute(&pool)
        .await
        .unwrap();
    t.admin
        .post(&verb(t.id, id, "schedule"))
        .json(&schedule_body(None, "instant"))
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn scheduling_a_template_that_no_longer_compiles_names_the_variant(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = ab_draft(&t, 50).await;
    let second: Uuid = sqlx::query_scalar("select id from templates where name like 'second-%'")
        .fetch_one(&pool)
        .await
        .unwrap();
    // The second variant's template names a field nobody has, as a hand-edit would leave it.
    sqlx::query("update templates set tree = $2 where id = $1")
        .bind(second)
        .bind(json!({"blocks": [{"type": "Text", "runs": [{"type": "Tag", "source": "{{ nope }}"}]}]}))
        .execute(&pool)
        .await
        .unwrap();
    let r = t
        .admin
        .post(&verb(t.id, id, "schedule"))
        .json(&schedule_body(None, "instant"))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["variants[1].template_id"]);
    // A template that is gone is refused the same way.
    sqlx::query("delete from templates where id = $1")
        .bind(t.template)
        .execute(&pool)
        .await
        .unwrap();
    let r = t
        .admin
        .post(&verb(t.id, id, "schedule"))
        .json(&schedule_body(None, "instant"))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["variants[0].template_id"]);
    assert_eq!(state_of(&pool, id).await, "draft");
}

#[sqlx::test]
async fn a_subject_naming_an_unknown_field_is_422(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let mut body = complete(&t, "c");
    body["variants"][0]["subject"] = json!("Hi {{ nope }}");
    let r = t.editor.post(&campaigns_path(t.id)).json(&body).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["variants[0].subject"]);
    // The same on an edit, and a line break in a subject is refused too.
    let id = draft(&t, "ok").await;
    for subject in ["Hi {{ nope }}", "a\r\nBcc: x@y.test"] {
        let r = t
            .editor
            .patch(&campaign_path(t.id, id))
            .json(
                &json!({"variants": [{"subject": subject, "template_id": t.template,
                                        "from_name": null}]}),
            )
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error_fields(&r), ["variants[0].subject"]);
    }
}

#[sqlx::test]
async fn a_draft_is_validated_on_every_field_it_names(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let other = tenant(&pool, "other").await;
    let ghost = Uuid::now_v7();
    let variant = |tpl: Uuid| json!({"subject": "s", "template_id": tpl, "from_name": null});
    let four = (0..4).map(|_| variant(t.template)).collect::<Vec<_>>();
    let five = (0..5).map(|_| variant(t.template)).collect::<Vec<_>>();
    for (patch, field) in [
        (json!({"name": ""}), "name"),
        (json!({"list_ids": [ghost]}), "list_ids"),
        // Another workspace's list is not this workspace's list.
        (json!({"list_ids": [other.list]}), "list_ids"),
        (json!({"segment_id": ghost}), "segment_id"),
        (json!({"sending_domain_id": ghost}), "sending_domain_id"),
        (
            json!({"sending_domain_id": other.domain}),
            "sending_domain_id",
        ),
        (json!({"from_local_part": "a b"}), "from_local_part"),
        (json!({"from_local_part": "a..b"}), "from_local_part"),
        (json!({"reply_to": "not an address"}), "reply_to"),
        (json!({"variants": []}), "variants"),
        (json!({"variants": five}), "variants"),
        (
            json!({"variants": [variant(ghost)]}),
            "variants[0].template_id",
        ),
        (
            json!({"variants": [variant(other.template)]}),
            "variants[0].template_id",
        ),
        (
            json!({"variants": four.clone(), "ab_test_percent": 9}),
            "ab_test_percent",
        ),
        (
            json!({"variants": four.clone(), "ab_test_percent": 101}),
            "ab_test_percent",
        ),
        (json!({"ab_test_percent": 50}), "ab_test_percent"),
    ] {
        let id = draft(&t, "c").await;
        let r = t.editor.patch(&campaign_path(t.id, id)).json(&patch).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error_fields(&r), [field], "{patch}");
    }
    // The boundaries are fine: four variants at 10%, and at 100% (a pure split).
    for pct in [10, 100] {
        let id = draft(&t, "c").await;
        let r = t
            .editor
            .patch(&campaign_path(t.id, id))
            .json(&json!({"variants": four.clone(), "ab_test_percent": pct}))
            .await;
        r.assert_status_ok();
        assert_eq!(r.json::<Value>()["ab_test_percent"], pct);
    }
    // Going down to one variant ends the test; null clears a nullable field.
    let id = draft(&t, "c").await;
    t.editor
        .patch(&campaign_path(t.id, id))
        .json(&json!({"variants": four, "ab_test_percent": 50}))
        .await
        .assert_status_ok();
    let r = t
        .editor
        .patch(&campaign_path(t.id, id))
        .json(&json!({"variants": [variant(t.template)], "reply_to": null}))
        .await;
    r.assert_status_ok();
    let v = r.json::<Value>();
    assert_eq!(v["ab_test_percent"], Value::Null);
    assert_eq!(v["reply_to"], Value::Null);
    // A bare draft is allowed, and shows the contract's empty shape.
    let r = t
        .editor
        .post(&campaigns_path(t.id))
        .json(&json!({"name": "bare"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let v = r.json::<Value>();
    assert_eq!(v["variants"], json!([]));
    assert_eq!(v["schedule"], Value::Null);
    assert_eq!(v["state"], "draft");
    assert_eq!(v["counts"]["total"], 0);
    // Scheduling it is refused on what is missing, not accepted.
    let r = t
        .admin
        .post(&verb(t.id, id_of(&r), "schedule"))
        .json(&schedule_body(None, "instant"))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["list_ids"]);
}

#[sqlx::test]
async fn recipient_local_with_two_variants_is_422(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = ab_draft(&t, 100).await;
    let r = t
        .admin
        .post(&verb(t.id, id, "schedule"))
        .json(&schedule_body(Some(&in_days_at(3, 9)), "recipient_local"))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["schedule.mode"]);
    // The same two variants are fine as an instant send.
    t.admin
        .post(&verb(t.id, id, "schedule"))
        .json(&schedule_body(Some(&in_days_at(3, 9)), "instant"))
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn a_schedule_is_validated_field_by_field(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    let past = in_days_at(-2, 9);
    for (body, field) in [
        (
            json!({"mode": "weekly", "zone": "UTC", "local": null}),
            "schedule.mode",
        ),
        (
            json!({"mode": "instant", "zone": "Mars/Olympus", "local": null}),
            "schedule.zone",
        ),
        (
            json!({"mode": "instant", "zone": "UTC", "local": "tomorrow"}),
            "schedule.local",
        ),
        (
            json!({"mode": "instant", "zone": "UTC", "local": past}),
            "schedule.local",
        ),
        (
            json!({"mode": "recipient_local", "zone": "UTC", "local": past}),
            "schedule.local",
        ),
    ] {
        let r = t.admin.post(&verb(t.id, id, "schedule")).json(&body).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error_fields(&r), [field], "{body}");
    }
    assert_eq!(state_of(&pool, id).await, "draft");
    // "Now" is a null `local`, and is the instant it was scheduled at.
    let r = t
        .admin
        .post(&verb(t.id, id, "schedule"))
        .json(&json!({"mode": "recipient_local", "zone": "UTC", "local": null}))
        .await;
    r.assert_status_ok();
    assert!(r.json::<Value>()["schedule"]["send_at"].is_string());
}

#[sqlx::test]
async fn recipient_local_too_late_for_the_earliest_zone_is_422(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    // The easternmost zone (UTC+14) reaches any wall-clock time up to 14 hours before UTC does, so
    // a local time of just now is long past there, though it is still ahead in New York.
    let local = (Utc::now() + Duration::hours(1))
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    let r = t
        .admin
        .post(&verb(t.id, id, "schedule"))
        .json(&json!({"mode": "recipient_local", "zone": "America/New_York", "local": local}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["schedule.local"]);
    // The same wall-clock time as an instant in New York is in the future.
    t.admin
        .post(&verb(t.id, id, "schedule"))
        .json(&json!({"mode": "instant", "zone": "America/New_York", "local": local}))
        .await
        .assert_status_ok();
    t.admin
        .post(&verb(t.id, id, "unschedule"))
        .await
        .assert_status_ok();
    // Three days on is in the future for every zone.
    t.admin
        .post(&verb(t.id, id, "schedule"))
        .json(
            &json!({"mode": "recipient_local", "zone": "America/New_York",
                      "local": in_days_at(3, 9)}),
        )
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn scheduling_enqueues_one_launch_job_at_send_at(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    let r = t
        .admin
        .post(&verb(t.id, id, "schedule"))
        .json(&schedule_body(Some(&in_days_at(2, 9)), "instant"))
        .await;
    r.assert_status_ok();
    let v = r.json::<Value>();
    assert_eq!(v["state"], "scheduled");
    assert_eq!(v["schedule"]["zone"], "America/New_York");
    assert_eq!(v["schedule"]["local"], in_days_at(2, 9));
    let send_at: DateTime<Utc> = v["schedule"]["send_at"].as_str().unwrap().parse().unwrap();
    let jobs = jobs_of(&pool, id, "campaign_launch").await;
    assert_eq!(jobs.len(), 1, "exactly one launch job");
    assert_eq!(
        jobs[0].1, send_at,
        "the job runs at the instant the API echoed"
    );
    let launch: Option<Uuid> =
        sqlx::query_scalar("select launch_job_id from campaigns where id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        launch,
        Some(jobs[0].0),
        "the campaign points at its launch job"
    );
    // Unscheduling clears the pointer and the instant; the orphaned job is the worker's to no-op.
    let r = t.admin.post(&verb(t.id, id, "unschedule")).await;
    assert_eq!(r.json::<Value>()["schedule"]["send_at"], Value::Null);
    let launch: Option<Uuid> =
        sqlx::query_scalar("select launch_job_id from campaigns where id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(launch, None);
}

/// The first spring-forward date in New York after today, found by stepping through `chrono-tz`
/// so the test never ages out: the date on which 02:30 does not exist.
fn next_skipped_new_york_morning() -> NaiveDate {
    let mut day = Utc::now().date_naive() + Duration::days(2);
    for _ in 0..800 {
        if chrono_tz::America::New_York
            .from_local_datetime(&day.and_hms_opt(2, 30, 0).unwrap())
            .earliest()
            .is_none()
        {
            return day;
        }
        day += Duration::days(1);
    }
    panic!("no spring-forward in 800 days");
}

#[sqlx::test]
async fn a_dst_schedule_echoes_the_resolved_instant(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    let day = next_skipped_new_york_morning();
    let local = format!("{day}T02:30");
    let r = t
        .admin
        .post(&verb(t.id, id, "schedule"))
        .json(&json!({"mode": "instant", "zone": "America/New_York", "local": local}))
        .await;
    r.assert_status_ok();
    let v = r.json::<Value>();
    // As typed, and as resolved: 02:30 EST is 07:30Z, which is 03:30 EDT, the hour after.
    assert_eq!(v["schedule"]["local"], local);
    let send_at: DateTime<Utc> = v["schedule"]["send_at"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        send_at,
        Utc.from_utc_datetime(&day.and_hms_opt(7, 30, 0).unwrap())
    );
    let there = send_at.with_timezone(&chrono_tz::America::New_York);
    assert_eq!(there.format("%H:%M").to_string(), "03:30");
    assert_eq!(jobs_of(&pool, id, "campaign_launch").await[0].1, send_at);
}

#[sqlx::test]
async fn a_test_send_queues_one_job_per_address_for_the_sample_contact(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    // A real subscriber, whose name must not appear anywhere in a test.
    let r = t
        .admin
        .post(&format!("/api/v1/workspaces/{}/contacts", t.id))
        .json(&json!({"email": "zelda@example.com", "first_name": "Zelda"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    t.admin
        .post(&format!(
            "/api/v1/workspaces/{}/lists/{}/members",
            t.id, t.list
        ))
        .json(&json!({"contact_id": id_of(&r)}))
        .await
        .assert_status_ok();

    let r = t
        .editor
        .post(&verb(t.id, id, "test"))
        .json(&json!({"addresses": ["One@Example.com", "two@example.com", "one@example.com"]}))
        .await;
    r.assert_status(StatusCode::ACCEPTED);
    assert_eq!(
        r.json::<Value>(),
        json!({"queued": 2}),
        "a repeated address is one job"
    );

    let jobs = jobs_of(&pool, id, "campaign_test").await;
    assert_eq!(jobs.len(), 2);
    let mut to: Vec<&str> = jobs.iter().map(|j| j.2["to"].as_str().unwrap()).collect();
    to.sort_unstable();
    assert_eq!(to, ["one@example.com", "two@example.com"]);
    for (_, _, p) in &jobs {
        let html = p["html"].as_str().unwrap();
        assert!(html.contains("Sam"), "the sample contact's name: {html}");
        assert!(html.contains("/u/test"), "the test link: {html}");
        assert!(html.contains(ADDRESS), "the postal address: {html}");
        assert!(
            !html.contains("Zelda") && !p.to_string().contains("zelda"),
            "a subscriber leaked"
        );
        assert_eq!(p["subject"], "[Test] Hi Sam");
        assert!(p["text"].as_str().unwrap().contains("/u/test"));
        assert_eq!(p["from"], "news@mail.example.com");
        assert_eq!(p["from_name"], "Acme");
        assert_eq!(p["reply_to"], "hello@example.com");
        assert_eq!(p["sending_domain_id"], json!(t.domain));
    }
    // The audit row says how many, never which.
    let meta: Value = sqlx::query_scalar(
        "select metadata from audit_log where action = 'campaign.test_sent' and workspace_id = $1",
    )
    .bind(t.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(meta["addresses"], 2);
    assert!(!meta.to_string().contains("example.com"));
    // A test is for a draft or a scheduled campaign, not a sending one.
    set_state(&pool, id, "sending").await;
    t.editor
        .post(&verb(t.id, id, "test"))
        .json(&json!({"addresses": ["one@example.com"]}))
        .await
        .assert_status(StatusCode::CONFLICT);
    // A variant the campaign does not have.
    set_state(&pool, id, "draft").await;
    let r = t
        .editor
        .post(&verb(t.id, id, "test"))
        .json(&json!({"addresses": ["one@example.com"], "variant": 3}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["variant"]);
}

#[sqlx::test]
async fn a_sixth_address_is_422_and_a_sixth_test_in_a_minute_is_429(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    let six: Vec<String> = (0..6).map(|i| format!("a{i}@example.com")).collect();
    let r = t
        .editor
        .post(&verb(t.id, id, "test"))
        .json(&json!({"addresses": six}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["addresses"]);
    assert!(jobs_of(&pool, id, "campaign_test").await.is_empty());
    // The refusal above spent one of the five requests a minute. Four more are answered, then the
    // limiter: the sixth request in the minute is a 429.
    let one = json!({"addresses": ["a@example.com"]});
    let mut statuses = Vec::new();
    for _ in 0..5 {
        statuses.push(
            t.editor
                .post(&verb(t.id, id, "test"))
                .json(&one)
                .await
                .status_code(),
        );
    }
    let accepted = StatusCode::ACCEPTED;
    assert_eq!(
        statuses,
        [
            accepted,
            accepted,
            accepted,
            accepted,
            StatusCode::TOO_MANY_REQUESTS
        ]
    );
    assert_eq!(jobs_of(&pool, id, "campaign_test").await.len(), 4);
    // It is per workspace: another workspace's editor still has its own five.
    let other = tenant(&pool, "other").await;
    let oid = draft(&other, "c").await;
    other
        .editor
        .post(&verb(other.id, oid, "test"))
        .json(&one)
        .await
        .assert_status(accepted);
}

#[sqlx::test]
async fn an_unreadable_test_address_is_422(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    for bad in [json!([]), json!(["not-an-address"])] {
        let r = t
            .editor
            .post(&verb(t.id, id, "test"))
            .json(&json!({"addresses": bad}))
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error_fields(&r), ["addresses"]);
    }
}

#[sqlx::test]
async fn the_campaign_preview_shows_the_footer(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    let r = t
        .viewer
        .post(&verb(t.id, id, "preview"))
        .json(&json!({}))
        .await;
    r.assert_status_ok();
    let v = r.json::<Value>();
    assert!(v["html"].as_str().unwrap().contains("/u/preview"), "{v}");
    assert!(v["html"].as_str().unwrap().contains(ADDRESS), "{v}");
    assert!(v["text"].as_str().unwrap().contains("/u/preview"), "{v}");
    assert_eq!(v["subject"], "Hi Sam", "no [Test] prefix on a preview");
    // A variant it does not have.
    let r = t
        .viewer
        .post(&verb(t.id, id, "preview"))
        .json(&json!({"variant": 1}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["variant"]);
}

#[sqlx::test]
async fn the_estimate_counts_a_contact_on_two_lists_once_and_skips_the_suppressed(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let list_b = id_of(
        &t.admin
            .post(&format!("/api/v1/workspaces/{}/lists", t.id))
            .json(&json!({"name": "second", "double_optin": false}))
            .await,
    );
    let mut contacts = Vec::new();
    for (email, first) in [
        ("both@example.com", "Ann"),
        ("a-only@example.com", "Bob"),
        ("b-only@example.com", "Ann"),
        ("quiet@example.com", "Ann"),
    ] {
        let r = t
            .admin
            .post(&format!("/api/v1/workspaces/{}/contacts", t.id))
            .json(&json!({"email": email, "first_name": first}))
            .await;
        contacts.push(id_of(&r));
    }
    for (list, who) in [
        (t.list, 0),
        (list_b, 0),
        (t.list, 1),
        (list_b, 2),
        (list_b, 3),
    ] {
        t.admin
            .post(&format!("/api/v1/workspaces/{}/lists/{list}/members", t.id))
            .json(&json!({"contact_id": contacts[who]}))
            .await
            .assert_status_ok();
    }
    t.admin
        .post(&format!("/api/v1/workspaces/{}/suppressions", t.id))
        .json(&json!({"scope": "address", "email": "quiet@example.com", "reason": "manual"}))
        .await
        .assert_status(StatusCode::CREATED);
    let id = draft(&t, "c").await;
    let estimate = |id: Uuid| {
        let t = &t;
        async move {
            t.viewer
                .post(&verb(t.id, id, "estimate"))
                .await
                .json::<Value>()
        }
    };
    // List A alone: both, a-only.
    assert_eq!(estimate(id).await, json!({"count": 2, "exact": true}));
    // Add list B: + b-only; `both` is on both lists and counts once; `quiet` is suppressed.
    t.editor
        .patch(&campaign_path(t.id, id))
        .json(&json!({"list_ids": [t.list, list_b]}))
        .await
        .assert_status_ok();
    assert_eq!(estimate(id).await, json!({"count": 3, "exact": true}));
    // Narrowed by a segment: first_name is Ann -> both, b-only.
    let r = t
        .admin
        .post(&format!("/api/v1/workspaces/{}/segments", t.id))
        .json(&json!({"name": "anns", "rules": {"match": "all",
              "rules": [{"field": "first_name", "op": "eq", "value": "Ann"}]}}))
        .await;
    r.assert_status(StatusCode::CREATED);
    t.editor
        .patch(&campaign_path(t.id, id))
        .json(&json!({"segment_id": id_of(&r)}))
        .await
        .assert_status_ok();
    assert_eq!(estimate(id).await, json!({"count": 2, "exact": true}));
    // No list, no audience.
    t.editor
        .patch(&campaign_path(t.id, id))
        .json(&json!({"list_ids": [], "segment_id": null}))
        .await
        .assert_status_ok();
    assert_eq!(estimate(id).await, json!({"count": 0, "exact": true}));
}

#[sqlx::test]
async fn deleting_is_draft_only(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    for state in [
        "scheduled",
        "sending",
        "paused",
        "sent",
        "cancelled",
        "failed",
    ] {
        set_state(&pool, id, state).await;
        let r = t.editor.delete(&campaign_path(t.id, id)).await;
        r.assert_status(StatusCode::CONFLICT);
        assert_eq!(problem_type(&r), "urn:nm:problem:campaign-state", "{state}");
    }
    // A draft with a test job queued goes, and its job with it.
    let d = draft(&t, "d").await;
    t.editor
        .post(&verb(t.id, d, "test"))
        .json(&json!({"addresses": ["a@example.com"]}))
        .await
        .assert_status(StatusCode::ACCEPTED);
    t.editor
        .delete(&campaign_path(t.id, d))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    t.viewer
        .get(&campaign_path(t.id, d))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert!(jobs_of(&pool, d, "campaign_test").await.is_empty());
    // A second delete is not a repeat: it is gone.
    t.editor
        .delete(&campaign_path(t.id, d))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn the_list_pages_newest_first_and_takes_either_cursor_name(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let ids = [
        draft(&t, "one").await,
        draft(&t, "two").await,
        draft(&t, "three").await,
    ];
    let r = t
        .viewer
        .get(&format!("{}?limit=2", campaigns_path(t.id)))
        .await;
    r.assert_status_ok();
    let first = r.json::<Value>();
    let names: Vec<&str> = first["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["three", "two"]);
    // The summary's shape: no content.
    assert_eq!(first["data"][0]["id"], json!(ids[2]));
    assert_eq!(first["data"][0]["counts"]["queued"], 0);
    assert!(first["data"][0].get("variants").is_none());
    let cursor = first["next_cursor"].as_str().unwrap();
    for key in ["cursor", "after"] {
        let r = t
            .viewer
            .get(&format!("{}?limit=2&{key}={cursor}", campaigns_path(t.id)))
            .await;
        let page = r.json::<Value>();
        assert_eq!(page["data"].as_array().unwrap().len(), 1, "{key}");
        assert_eq!(page["data"][0]["name"], "one");
        assert_eq!(page["next_cursor"], Value::Null);
    }
}

#[sqlx::test]
async fn the_template_list_is_a_summary(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let r = t
        .viewer
        .get(&format!("/api/v1/workspaces/{}/templates", t.id))
        .await;
    r.assert_status_ok();
    let items = r.json::<Value>()["data"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1);
    let mut keys: Vec<&str> = items[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["created_at", "id", "kind", "name", "updated_at"]);
    // The document is still one GET away.
    let r = t
        .viewer
        .get(&format!(
            "/api/v1/workspaces/{}/templates/{}",
            t.id, t.template
        ))
        .await;
    assert!(r.json::<Value>()["tree"].is_object());
}

#[sqlx::test]
async fn a_winner_is_chosen_once_after_the_test_cohort_is_resolved(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = ab_draft(&t, 20).await;
    let pick = |v: i64| {
        let t = &t;
        async move {
            t.admin
                .post(&verb(t.id, id, "winner"))
                .json(&json!({"variant": v}))
                .await
        }
    };
    // A draft has no winner to pick.
    pick(1).await.assert_status(StatusCode::CONFLICT);
    // Sending, but the test cohort is still being resolved.
    sqlx::query(
        "update campaigns set state = 'sending', phase = 1, resolved = false, total = 10, \
         sent = 4 where id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    pick(1).await.assert_status(StatusCode::CONFLICT);
    let v = t.viewer.get(&campaign_path(t.id, id)).await.json::<Value>();
    assert_eq!(v["awaiting_winner"], false);
    // Resolved, with the cohort all recorded: the screen may offer it.
    sqlx::query("update campaigns set resolved = true, sent = 10 where id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let v = t.viewer.get(&campaign_path(t.id, id)).await.json::<Value>();
    assert_eq!(v["awaiting_winner"], true);
    assert_eq!(
        v["counts"],
        json!({"total": 10, "queued": 0, "sent": 10, "failed": 0, "unknown": 0, "skipped": 0})
    );
    // A variant that does not exist.
    let r = pick(2).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["variant"]);
    let r = pick(1).await;
    r.assert_status_ok();
    let v = r.json::<Value>();
    assert_eq!(v["winner"], 1);
    assert_eq!(v["awaiting_winner"], false);
    // Phase 2: the remainder resolves from the start, with its own launch job, run now.
    let (phase, resolved, cursor, job): (i16, bool, Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "select phase, resolved, resolve_cursor, launch_job_id from campaigns where id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((phase, resolved, cursor), (2, false, None));
    let jobs = jobs_of(&pool, id, "campaign_launch").await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(job, Some(jobs[0].0));
    assert!(jobs[0].1 <= Utc::now() + Duration::seconds(5), "run now");
    // Repeating the same pick is 200 and queues nothing more; a different one is a 409.
    pick(1).await.assert_status_ok();
    pick(0).await.assert_status(StatusCode::CONFLICT);
    assert_eq!(jobs_of(&pool, id, "campaign_launch").await.len(), 1);
    // A pure split has no winner phase at all.
    let split = ab_draft(&t, 100).await;
    sqlx::query("update campaigns set state = 'sending', resolved = true where id = $1")
        .bind(split)
        .execute(&pool)
        .await
        .unwrap();
    t.admin
        .post(&verb(t.id, split, "winner"))
        .json(&json!({"variant": 0}))
        .await
        .assert_status(StatusCode::CONFLICT);
}

#[sqlx::test]
async fn resuming_rebuilds_the_rendition_only_when_it_no_longer_passes(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = draft(&t, "c").await;
    set_state(&pool, id, "paused").await;
    sqlx::query("update campaigns set pause_reason = 'the relay was down' where id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let frozen_at = || async {
        sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
            "select frozen_at from campaigns where id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    // A paused campaign with no usable frozen rendition gets a fresh one, and a time for it.
    let r = t.admin.post(&verb(t.id, id, "resume")).await;
    r.assert_status_ok();
    let v = r.json::<Value>();
    assert_eq!(v["state"], "sending");
    assert_eq!(v["pause_reason"], Value::Null);
    let first = frozen_at().await;
    assert!(first.is_some());
    let frozen: Value = sqlx::query_scalar("select frozen from campaigns where id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(frozen["from"], "news@mail.example.com");
    // Read back through its guard: the column is what a launch would have written.
    assert!(nm::campaign::Frozen::from_column(&frozen).is_ok());
    // A rendition that still passes is kept as it is, however long it has been paused.
    set_state(&pool, id, "paused").await;
    t.admin
        .post(&verb(t.id, id, "resume"))
        .await
        .assert_status_ok();
    assert_eq!(frozen_at().await, first, "a good rendition is not rebuilt");
    // The template starts naming a custom field, and the campaign is re-frozen against it.
    sqlx::query(
        "insert into custom_fields (id, workspace_id, key, label, kind) \
         values ($1, $2, 'plan', 'Plan', 'text')",
    )
    .bind(Uuid::now_v7())
    .bind(t.id)
    .execute(&pool)
    .await
    .unwrap();
    t.editor
        .patch(&format!(
            "/api/v1/workspaces/{}/templates/{}",
            t.id, t.template
        ))
        .json(&json!({"tree": {"blocks": [
            {"type": "Text", "runs": [{"type": "Tag", "source": "{{ plan }}"}]}]}}))
        .await
        .assert_status_ok();
    set_state(&pool, id, "paused").await;
    sqlx::query("update campaigns set frozen = null where id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    t.admin
        .post(&verb(t.id, id, "resume"))
        .await
        .assert_status_ok();
    // The field changes type: the frozen rendition no longer passes `ready`, a rebuild is refused
    // (the template was saved against the old type), and the campaign stays paused.
    sqlx::query(
        "update custom_fields set kind = 'number' where key = 'plan' and workspace_id = $1",
    )
    .bind(t.id)
    .execute(&pool)
    .await
    .unwrap();
    set_state(&pool, id, "paused").await;
    let r = t.admin.post(&verb(t.id, id, "resume")).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:template-stale");
    assert_eq!(state_of(&pool, id).await, "paused");
}
