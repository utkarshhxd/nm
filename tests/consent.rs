// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use common::{PW, create_user, create_workspace};
use nm::consent::{Action, Context, Source, record};
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(pool: &PgPool, slug: &str) -> Uuid {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    create_workspace(pool, owner, slug).await
}

async fn contact(pool: &PgPool, ws: Uuid, email: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into contacts (id, workspace_id, email) values ($1, $2, $3)")
        .bind(id)
        .bind(ws)
        .bind(email)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn list(pool: &PgPool, ws: Uuid, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into lists (id, workspace_id, name) values ($1, $2, $3)")
        .bind(id)
        .bind(ws)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    id
}

type Row = (
    Uuid,
    Uuid,
    Option<Uuid>,
    String,
    String,
    Option<String>,
    Option<IpAddr>,
    Option<String>,
);

#[sqlx::test]
async fn a_recorded_consent_row_carries_every_column_it_was_given(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let c = contact(&pool, ws, "a@ex.test").await;
    let l = list(&pool, ws, "news").await;
    let cx = Context {
        ip: Some("203.0.113.9".parse().unwrap()),
        user_agent: Some("Mozilla/5.0 (test)".into()),
        source_ref: Some("form-42".into()),
    };
    let mut conn = pool.acquire().await.unwrap();
    record(
        &mut conn,
        ws,
        c,
        Some(l),
        Action::Confirmed,
        Source::OneClick,
        &cx,
    )
    .await
    .unwrap();

    let row: Row = sqlx::query_as(
        "select workspace_id, contact_id, list_id, action, source, source_ref, ip, user_agent \
         from consent_log",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            ws,
            c,
            Some(l),
            "confirmed".into(),
            "one_click".into(),
            Some("form-42".into()),
            Some("203.0.113.9".parse().unwrap()),
            Some("Mozilla/5.0 (test)".into()),
        )
    );

    // A workspace-wide suppression has no list and no evidence; both stay null.
    record(
        &mut conn,
        ws,
        c,
        None,
        Action::Suppressed,
        Source::Bounce,
        &Context::default(),
    )
    .await
    .unwrap();
    let bare: (Option<Uuid>, Option<IpAddr>, Option<String>, Option<String>) = sqlx::query_as(
        "select list_id, ip, user_agent, source_ref from consent_log where action = 'suppressed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bare, (None, None, None, None));
}

/// The database must refuse, not Rust: `record` checks nothing, so the only thing standing between
/// a cross-workspace row and the table is the composite foreign keys. The constraint name proves
/// which one fired.
#[sqlx::test]
async fn a_consent_row_for_another_workspace_cannot_be_written(pool: PgPool) {
    let a = workspace(&pool, "alpha").await;
    let b = workspace(&pool, "beta").await;
    let contact_a = contact(&pool, a, "a@ex.test").await;
    let contact_b = contact(&pool, b, "b@ex.test").await;
    let list_a = list(&pool, a, "news").await;
    let list_b = list(&pool, b, "news").await;
    let mut conn = pool.acquire().await.unwrap();
    let cx = Context::default();

    // Postgres checks the foreign keys in declaration order: contact first, then list.
    for (ws, c, l, fk) in [
        (
            a,
            contact_a,
            list_b,
            "consent_log_workspace_id_list_id_fkey",
        ),
        (
            b,
            contact_a,
            list_b,
            "consent_log_workspace_id_contact_id_fkey",
        ),
        (
            a,
            contact_b,
            list_a,
            "consent_log_workspace_id_contact_id_fkey",
        ),
    ] {
        let err = record(
            &mut conn,
            ws,
            c,
            Some(l),
            Action::Subscribed,
            Source::Api,
            &cx,
        )
        .await
        .expect_err("the database must refuse a cross-workspace row");
        // `record` scrubs its own error, so a caller sees the message and nothing else. This is
        // also what proves the scrub is applied inside `record` and not at each call site.
        let shown = format!("{err:?}");
        assert!(shown.contains("23503"), "foreign_key_violation: {shown}");
        assert!(shown.contains(fk), "{shown}");
    }
    let n: i64 = sqlx::query_scalar("select count(*) from consent_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test]
async fn consent_rows_for_a_contact_come_back_newest_first(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let c = contact(&pool, ws, "a@ex.test").await;
    let l = list(&pool, ws, "news").await;
    let mut conn = pool.acquire().await.unwrap();
    let cx = Context::default();
    // Autocommit statements, so `now()` differs between them.
    for action in [Action::Subscribed, Action::Confirmed, Action::Unsubscribed] {
        record(&mut conn, ws, c, Some(l), action, Source::Form, &cx)
            .await
            .unwrap();
    }
    let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
        "select action, created_at from consent_log \
         where workspace_id = $1 and contact_id = $2 order by created_at desc",
    )
    .bind(ws)
    .bind(c)
    .fetch_all(&pool)
    .await
    .unwrap();
    let actions: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(actions, ["unsubscribed", "confirmed", "subscribed"]);
}

async fn one_row(pool: &PgPool, ws: Uuid, c: Uuid, l: Uuid) {
    let cx = Context {
        ip: Some("203.0.113.9".parse().unwrap()),
        user_agent: Some("Mozilla/5.0 (test)".into()),
        source_ref: Some("form-42".into()),
    };
    let mut conn = pool.acquire().await.unwrap();
    record(
        &mut conn,
        ws,
        c,
        Some(l),
        Action::Confirmed,
        Source::Form,
        &cx,
    )
    .await
    .unwrap();
}

#[sqlx::test]
async fn deleting_a_list_keeps_the_consent_rows_and_only_clears_the_pointer(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let c = contact(&pool, ws, "a@ex.test").await;
    let l = list(&pool, ws, "news").await;
    one_row(&pool, ws, c, l).await;

    sqlx::query("delete from lists where id = $1")
        .bind(l)
        .execute(&pool)
        .await
        .unwrap();

    let row: Row = sqlx::query_as(
        "select workspace_id, contact_id, list_id, action, source, source_ref, ip, user_agent \
         from consent_log",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            ws,
            c,
            None,
            "confirmed".into(),
            "form".into(),
            Some("form-42".into()),
            Some("203.0.113.9".parse().unwrap()),
            Some("Mozilla/5.0 (test)".into()),
        )
    );
}

#[sqlx::test]
async fn erasing_a_contact_removes_its_consent_rows_and_no_one_elses(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let gone = contact(&pool, ws, "gone@ex.test").await;
    let kept = contact(&pool, ws, "kept@ex.test").await;
    let l = list(&pool, ws, "news").await;
    one_row(&pool, ws, gone, l).await;
    one_row(&pool, ws, kept, l).await;

    sqlx::query("delete from contacts where id = $1")
        .bind(gone)
        .execute(&pool)
        .await
        .expect("the erasure cascade must succeed despite the append-only trigger");

    let left: Vec<Uuid> = sqlx::query_scalar("select contact_id from consent_log")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(left, [kept]);
}

#[sqlx::test]
async fn a_consent_row_cannot_be_updated(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let c = contact(&pool, ws, "a@ex.test").await;
    let c2 = contact(&pool, ws, "b@ex.test").await;
    let l = list(&pool, ws, "news").await;
    // Two rows, so the unqualified statements below are bulk updates.
    one_row(&pool, ws, c, l).await;
    one_row(&pool, ws, c2, l).await;

    for sql in [
        "update consent_log set action = 'subscribed'",
        "update consent_log set ip = null",
        "update consent_log set created_at = now() + interval '1 day'",
        "update consent_log set contact_id = contact_id", // even a no-op change
        // `list_id` is evidence, and null already means "workspace-wide": only the list's deletion
        // may clear it, never a statement, alone or with another column.
        "update consent_log set list_id = null, ip = null",
        "update consent_log set list_id = null",
    ] {
        let err = sqlx::query(sql).execute(&pool).await.expect_err(sql);
        assert!(
            err.as_database_error()
                .unwrap()
                .message()
                .contains("append-only"),
            "{sql}: {err}"
        );
    }
    let rows: Vec<(String, Option<Uuid>, Option<IpAddr>)> =
        sqlx::query_as("select action, list_id, ip from consent_log")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    for (action, list_id, ip) in rows {
        assert_eq!(action, "confirmed");
        assert_eq!(list_id, Some(l));
        assert!(ip.is_some());
    }
}

/// The trigger lets Postgres's own `set null (list_id)` through, and only if it changes nothing
/// else. Postgres never changes anything else, so reaching that comparison takes a trigger of the
/// test's own: a `before delete on lists` one runs an UPDATE at depth 2, like the referential action
/// does. `sqlx::test` gives this test a private database, so the trigger and column die with it.
#[sqlx::test]
async fn a_depth_two_update_that_changes_more_than_list_id_is_refused(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let c = contact(&pool, ws, "a@ex.test").await;
    let l = list(&pool, ws, "news").await;
    one_row(&pool, ws, c, l).await;
    // A column the trigger function has never heard of: the comparison must cover it anyway.
    sqlx::query("alter table consent_log add column note text")
        .execute(&pool)
        .await
        .unwrap();

    for (changed, install) in [
        (
            "ip",
            "create or replace function nosy() returns trigger language plpgsql as $$ begin                update consent_log set list_id = null, ip = null where list_id = old.id;                return old; end $$;              create or replace trigger nosy before delete on lists                for each row execute function nosy();",
        ),
        (
            "note",
            "create or replace function nosy() returns trigger language plpgsql as $$ begin                update consent_log set list_id = null, note = 'edited' where list_id = old.id;                return old; end $$;",
        ),
    ] {
        sqlx::raw_sql(install).execute(&pool).await.unwrap();
        let err = sqlx::query("delete from lists where id = $1")
            .bind(l)
            .execute(&pool)
            .await
            .expect_err(changed);
        assert!(
            err.as_database_error()
                .unwrap()
                .message()
                .contains("append-only"),
            "{changed}: {err}"
        );
    }
    // The refused deletes rolled back whole: the list and the untouched row are still there.
    let (list_id, ip): (Option<Uuid>, Option<IpAddr>) =
        sqlx::query_as("select list_id, ip from consent_log")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(list_id, Some(l));
    assert!(ip.is_some());
}

#[sqlx::test]
async fn a_workspace_wide_row_cannot_be_given_a_list(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let c = contact(&pool, ws, "a@ex.test").await;
    let l = list(&pool, ws, "news").await;
    let mut conn = pool.acquire().await.unwrap();
    record(
        &mut conn,
        ws,
        c,
        None,
        Action::Suppressed,
        Source::Bounce,
        &Context::default(),
    )
    .await
    .unwrap();

    let err = sqlx::query("update consent_log set list_id = $1")
        .bind(l)
        .execute(&pool)
        .await
        .expect_err("null to non-null must be refused");
    assert!(
        err.as_database_error()
            .unwrap()
            .message()
            .contains("append-only")
    );
}

/// The migration says a null-`list_id` row is still held to its workspace by the contact key.
#[sqlx::test]
async fn a_workspace_wide_row_for_another_workspaces_contact_cannot_be_written(pool: PgPool) {
    let a = workspace(&pool, "alpha").await;
    let b = workspace(&pool, "beta").await;
    let contact_a = contact(&pool, a, "a@ex.test").await;
    let mut conn = pool.acquire().await.unwrap();

    let err = record(
        &mut conn,
        b,
        contact_a,
        None,
        Action::Suppressed,
        Source::Operator,
        &Context::default(),
    )
    .await
    .expect_err("the database must refuse it");
    let shown = format!("{err:?}");
    assert!(shown.contains("23503"), "{shown}");
    assert!(
        shown.contains("consent_log_workspace_id_contact_id_fkey"),
        "{shown}"
    );
}

/// Every variant, so a Rust enum and the database's `check` list cannot drift apart unseen. The
/// matches are exhaustive: a new variant fails to compile here until it is listed.
#[sqlx::test]
async fn every_action_and_source_round_trips_through_the_database(pool: PgPool) {
    fn action_text(a: Action) -> &'static str {
        match a {
            Action::Subscribed => "subscribed",
            Action::Confirmed => "confirmed",
            Action::Unsubscribed => "unsubscribed",
            Action::Resubscribed => "resubscribed",
            Action::Suppressed => "suppressed",
        }
    }
    fn source_text(s: Source) -> &'static str {
        match s {
            Source::Form => "form",
            Source::Import => "import",
            Source::Api => "api",
            Source::Operator => "operator",
            Source::OneClick => "one_click",
            Source::PreferenceCenter => "preference_center",
            Source::Bounce => "bounce",
            Source::Complaint => "complaint",
        }
    }
    let actions = [
        Action::Subscribed,
        Action::Confirmed,
        Action::Unsubscribed,
        Action::Resubscribed,
        Action::Suppressed,
    ];
    let sources = [
        Source::Form,
        Source::Import,
        Source::Api,
        Source::Operator,
        Source::OneClick,
        Source::PreferenceCenter,
        Source::Bounce,
        Source::Complaint,
    ];
    let ws = workspace(&pool, "acme").await;
    let c = contact(&pool, ws, "a@ex.test").await;
    let mut conn = pool.acquire().await.unwrap();
    for a in actions {
        for s in sources {
            record(&mut conn, ws, c, None, a, s, &Context::default())
                .await
                .unwrap_or_else(|e| panic!("{a:?} / {s:?}: {e:?}"));
        }
    }
    let total: i64 = sqlx::query_scalar("select count(*) from consent_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total as usize, actions.len() * sources.len());
    for a in actions {
        for s in sources {
            let want = (action_text(a).to_owned(), source_text(s).to_owned());
            let n: i64 = sqlx::query_scalar(
                "select count(*) from consent_log where action = $1 and source = $2",
            )
            .bind(&want.0)
            .bind(&want.1)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(n, 1, "{want:?}");
        }
    }
}

// ---- Task 3: every membership writer records consent -------------------------------------------

use axum::http::StatusCode;
use common::{TestApp, add_member};
use nm::contacts::import::{
    ColumnTarget, ContactField, Dedup, ImportJob,
    staging::{copy_into_staging, upsert_from_staging},
    upload_path,
};
use nm::{config::SecretKey, crypto::secrets::Secrets, workspaces::roles::Role};
use serde_json::{Value, json};

/// A workspace, a signed-in editor of it, and that editor's user id.
async fn editor(pool: &PgPool, slug: &str) -> (Uuid, Uuid, TestApp) {
    let ws = workspace(pool, slug).await;
    let email = format!("editor-{slug}@example.com");
    let id = create_user(pool, &email, PW, false).await;
    add_member(pool, ws, id, Role::Editor).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(&email, PW).await;
    (ws, id, app)
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

async fn api_list(app: &TestApp, ws: Uuid, name: &str, double_optin: bool) -> Uuid {
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/lists"))
        .json(&json!({"name": name, "double_optin": double_optin}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn api_contact(app: &TestApp, ws: Uuid, email: &str) -> Uuid {
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/contacts"))
        .json(&json!({"email": email}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

/// `(action, source, source_ref)` of every consent row, sorted: rows written in one transaction
/// share `created_at`, so nothing here may depend on the order they were written in.
async fn consent_rows(pool: &PgPool) -> Vec<(String, String, Option<String>)> {
    let mut rows: Vec<_> = sqlx::query_as("select action, source, source_ref from consent_log")
        .fetch_all(pool)
        .await
        .unwrap();
    rows.sort();
    rows
}

/// Consent rows carrying an IP or a user agent. An operator's request is not the subscriber's
/// evidence, so an operator-driven path must leave both null rather than invent them.
async fn with_evidence(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "select count(*) from consent_log where ip is not null or user_agent is not null",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    let sql = sqlx::AssertSqlSafe(format!("select count(*) from {table}"));
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[sqlx::test]
async fn subscribing_through_the_lists_api_writes_a_consent_row(pool: PgPool) {
    let (ws, operator, app) = editor(&pool, "acme").await;
    let list = api_list(&app, ws, "news", false).await;
    let c = api_contact(&app, ws, "a@ex.test").await;
    let members = format!("/api/v1/workspaces/{ws}/lists/{list}/members");

    app.post(&members)
        .json(&json!({"contact_id": c}))
        .await
        .assert_status_ok();
    type LoggedRow = (
        Uuid,
        Option<Uuid>,
        String,
        String,
        Option<String>,
        Option<IpAddr>,
        Option<String>,
    );
    // `fetch_one` takes the first row of however many there are; the count below is what makes
    // "one" true.
    let (who, l, action, source, source_ref, ip, agent): LoggedRow = sqlx::query_as(
        "select contact_id, list_id, action, source, source_ref, ip, user_agent from consent_log",
    )
    .fetch_one(&pool)
    .await
    .expect("the add must have written a consent row");
    assert_eq!((who, l), (c, Some(list)));
    assert_eq!(
        (action.as_str(), source.as_str()),
        ("subscribed", "operator")
    );
    // The operator is named and nothing of the operator's request is kept: `RequestMeta` is in
    // scope at this route, so an IP landing here is exactly the regression nothing else catches.
    assert_eq!(source_ref, Some(operator.to_string()));
    assert_eq!((ip, agent), (None, None));

    // Adding someone already on the list changes nothing, so records nothing.
    app.post(&members)
        .json(&json!({"contact_id": c}))
        .await
        .assert_status_ok();
    assert_eq!(count(&pool, "consent_log").await, 1);

    app.post(&format!("{members}/{c}/unsubscribe"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // A repeat unsubscribe changes nothing either.
    app.post(&format!("{members}/{c}/unsubscribe"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    app.post(&members)
        .json(&json!({"contact_id": c}))
        .await
        .assert_status_ok();
    let actions: Vec<_> = consent_rows(&pool).await.into_iter().map(|r| r.0).collect();
    assert_eq!(actions, ["resubscribed", "subscribed", "unsubscribed"]);
}

#[sqlx::test]
async fn subscribing_through_the_contacts_api_writes_a_consent_row(pool: PgPool) {
    let (ws, operator, app) = editor(&pool, "acme").await;
    let list = api_list(&app, ws, "news", false).await;
    let mut ids = vec![];
    for i in 0..3 {
        ids.push(api_contact(&app, ws, &format!("c{i}@ex.test")).await);
    }
    let body = json!({"ids": ids, "list_id": list});
    let bulk = |action: &'static str| {
        let (app, body) = (&app, &body);
        async move {
            app.post(&format!("/api/v1/workspaces/{ws}/contacts/bulk/{action}"))
                .json(body)
                .await
                .assert_status_ok();
        }
    };

    bulk("subscribe").await;
    let rows = consent_rows(&pool).await;
    assert_eq!(rows.len(), 3, "one row per contact: {rows:?}");
    assert!(rows.iter().all(|r| r.0 == "subscribed" && r.1 == "api"));
    let who: i64 = sqlx::query_scalar(
        "select count(distinct contact_id) from consent_log \
         where list_id = $1 and contact_id = any($2)",
    )
    .bind(list)
    .bind(&ids)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(who, 3);
    // The consent rows come from a CTE inside the bulk statement: it names the operator and binds
    // no evidence of its own, for the same reason the single-row writers invent none.
    assert!(
        rows.iter().all(|r| r.2 == Some(operator.to_string())),
        "{rows:?}"
    );
    assert_eq!(with_evidence(&pool).await, 0);

    // Everyone is on the list already: the statement writes nothing, so records nothing.
    bulk("subscribe").await;
    assert_eq!(count(&pool, "consent_log").await, 3);

    bulk("unsubscribe").await;
    let rows = consent_rows(&pool).await;
    let gone = rows
        .iter()
        .filter(|r| r.0 == "unsubscribed" && r.1 == "api");
    assert_eq!(gone.count(), 3);
    bulk("unsubscribe").await;
    assert_eq!(count(&pool, "consent_log").await, 6);
}

/// The whole importer pipeline over an in-memory file, in one transaction as the real one runs it.
async fn import_into(pool: &PgPool, ws: Uuid, list: Uuid, csv: &str) -> Uuid {
    let id = Uuid::now_v7();
    let job = ImportJob {
        id,
        workspace_id: ws,
        path: upload_path(id),
        delimiter: b',',
        has_header: true,
        mapping: vec![ColumnTarget::Field(ContactField::Email)],
        dedup: Dedup::default(),
        list_id: Some(list),
        defs: vec![],
    };
    let secrets = Secrets::new(&SecretKey([7u8; 32]));
    let mut tx = pool.begin().await.unwrap();
    let staged = copy_into_staging(&mut tx, &secrets, &job, csv.as_bytes())
        .await
        .unwrap();
    upsert_from_staging(&mut tx, &job, staged.failed)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    id
}

#[sqlx::test]
async fn an_import_that_subscribes_writes_one_consent_row_per_contact(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let list = list(&pool, ws, "news").await;
    let id = import_into(&pool, ws, list, "email\na@ex.test\nb@ex.test\nc@ex.test\n").await;

    let rows = consent_rows(&pool).await;
    let want = (
        "subscribed".to_owned(),
        "import".to_owned(),
        Some(id.to_string()),
    );
    assert_eq!(rows, [want.clone(), want.clone(), want]);
    // No IP and no user agent: an importing operator is not the subscriber, and none is invented.
    assert_eq!(with_evidence(&pool).await, 0);

    // The same file again writes no membership, so no consent row.
    import_into(&pool, ws, list, "email\na@ex.test\nb@ex.test\nc@ex.test\n").await;
    assert_eq!(count(&pool, "consent_log").await, 3);
}

/// The gate. One fixture drives all five writers; memberships and consent rows must match.
///
/// The source scan below is a **tripwire, not a proof**: it fails when a file that never touched
/// `list_memberships` starts to, which is where the next writer is most likely to appear. It
/// cannot see a second writer added to a file already on the list, and the count above only
/// covers the writers this fixture reaches. Invariant 8 is held by review; this makes the
/// cheapest way to break it loud.
#[sqlx::test]
async fn every_membership_status_write_in_the_tree_has_a_consent_row(pool: PgPool) {
    let (ws, _, app) = editor(&pool, "acme").await;
    let list = api_list(&app, ws, "news", false).await;
    let double = api_list(&app, ws, "digest", true).await;
    let a = api_contact(&app, ws, "a@ex.test").await;
    let b = api_contact(&app, ws, "b@ex.test").await;

    app.post(&format!("/api/v1/workspaces/{ws}/lists/{list}/members"))
        .json(&json!({"contact_id": a}))
        .await
        .assert_status_ok();
    app.post(&format!("/api/v1/workspaces/{ws}/contacts/bulk/subscribe"))
        .json(&json!({"ids": [a, b], "list_id": double}))
        .await
        .assert_status_ok();
    import_into(&pool, ws, list, "email\nb@ex.test\nc@ex.test\nd@ex.test\n").await;
    import_into(&pool, ws, double, "email\nd@ex.test\ne@ex.test\n").await;

    let memberships = count(&pool, "list_memberships").await;
    assert_eq!(memberships, 8, "the fixture must reach every writer");
    assert_eq!(count(&pool, "consent_log").await, memberships);
    let unlogged: i64 = sqlx::query_scalar(
        "select count(*) from list_memberships m where not exists (select 1 from consent_log c \
         where c.contact_id = m.contact_id and c.list_id = m.list_id)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unlogged, 0);

    // Every file that so much as names `list_memberships` — readers included. Matching the table
    // rather than a write verb is the same size and strictly stronger: `insert into` split over
    // two lines, `public.list_memberships`, a quoted name, `merge into` and a table name built
    // with `format!` all evade a verb list, and the first two are ordinary formatting.
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs")
                && std::fs::read_to_string(&p)
                    .unwrap()
                    .to_lowercase()
                    .contains("list_memberships")
            {
                out.push(p.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut touching = vec![];
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    walk(&src, &mut touching);
    touching.sort();
    let touching: Vec<_> = touching
        .iter()
        .map(|p| p.rsplit_once("/src/").unwrap().1)
        .collect();
    assert_eq!(
        touching,
        [
            "contacts/export.rs",
            "contacts/import/staging.rs",
            "contacts/mod.rs",
            "contacts/selection.rs",
            "lists/mod.rs",
            "lists/routes.rs",
            "public/confirm.rs",
            "segments/compile.rs",
        ],
        "this file now touches list_memberships: a writer of status needs a consent row in the \
         same transaction and a place in this fixture"
    );
}

/// Postgres puts the failing row, with its IP and user agent, into a check violation's DETAIL.
#[sqlx::test]
async fn a_consent_write_failure_carries_the_sqlstate_and_not_the_row(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let c = contact(&pool, ws, "a@ex.test").await;
    let err = sqlx::query(
        "insert into consent_log (id, workspace_id, contact_id, action, source, user_agent, ip) \
         values ($1, $2, $3, 'bogus', 'api', 'SECRET-AGENT', '203.0.113.77')",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(c)
    .execute(&pool)
    .await
    .expect_err("the check refuses it");
    assert!(
        format!("{err}{err:?}").contains("SECRET-AGENT"),
        "the premise: Postgres echoes the row"
    );

    let shown = format!("{:?}", nm::consent::scrub(err));
    assert!(shown.contains("23514"), "{shown}");
    assert!(
        !shown.contains("SECRET-AGENT") && !shown.contains("203.0.113.77"),
        "{shown}"
    );
}
