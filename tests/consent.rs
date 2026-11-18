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
        let db = err
            .as_database_error()
            .expect("a database error, not a Rust one");
        assert_eq!(db.code().as_deref(), Some("23503"), "foreign_key_violation");
        assert_eq!(db.constraint(), Some(fk));
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
    let db = err.as_database_error().expect("a database error");
    assert_eq!(db.code().as_deref(), Some("23503"));
    assert_eq!(
        db.constraint(),
        Some("consent_log_workspace_id_contact_id_fkey")
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
                .unwrap_or_else(|e| panic!("{a:?} / {s:?}: {e}"));
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
