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
