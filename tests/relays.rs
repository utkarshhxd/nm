// SPDX-License-Identifier: AGPL-3.0-only

//! Migration 0028: the three tables the sending core reads. Raw SQL on purpose: the guarantees
//! under test belong to the database, and a handler that happens to check first proves nothing.

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

async fn relay(pool: &PgPool, ws: Uuid, name: &str) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into relays (id, workspace_id, name, host, port, security, rate_limit_per_minute)
         values ($1, $2, $3, 'smtp.example.com', 587, 'starttls', 60)",
    )
    .bind(id)
    .bind(ws)
    .bind(name)
    .execute(pool)
    .await
    .map(|_| id)
}

fn sqlstate(err: &sqlx::Error) -> String {
    err.as_database_error()
        .and_then(|e| e.code().map(|c| c.into_owned()))
        .unwrap_or_else(|| format!("not a database error: {err}"))
}

#[sqlx::test]
async fn relay_requires_a_workspace(pool: PgPool) {
    let err = relay(&pool, Uuid::now_v7(), "primary").await.unwrap_err();
    assert_eq!(sqlstate(&err), "23503", "{err}");
}

#[sqlx::test]
async fn deleting_a_workspace_takes_its_relays_and_buckets(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let relay_id = relay(&pool, ws, "primary").await.unwrap();
    sqlx::query("insert into relay_buckets (relay_id, workspace_id, tokens) values ($1, $2, 60)")
        .bind(relay_id)
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query("delete from workspaces where id = $1")
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();

    for (table, sql) in [
        ("relays", "select count(*) from relays"),
        ("relay_buckets", "select count(*) from relay_buckets"),
    ] {
        let n: i64 = sqlx::query_scalar(sql).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "{table} survived its workspace");
    }
}

#[sqlx::test]
async fn an_app_signed_domain_without_a_key_is_refused(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let insert = |domain: &'static str, signing: &'static str, key: Option<&'static [u8]>| {
        sqlx::query(
            "insert into sending_domains (id, workspace_id, domain, signing, dkim_selector,
                                          dkim_private_key_encrypted)
             values ($1, $2, $3, $4, 'nm1', $5)",
        )
        .bind(Uuid::now_v7())
        .bind(ws)
        .bind(domain)
        .bind(signing)
        .bind(key)
        .execute(&pool)
    };

    let err = insert("a.example.com", "app", None).await.unwrap_err();
    assert_eq!(sqlstate(&err), "23514", "{err}");
    // The check refuses only the keyless app-signed row: the other three shapes are legal.
    insert("b.example.com", "app", Some(b"blob")).await.unwrap();
    insert("c.example.com", "relay", None).await.unwrap();
}

#[sqlx::test]
async fn two_relays_cannot_share_a_name_in_one_workspace(pool: PgPool) {
    let (a, b) = (
        workspace(&pool, "acme").await,
        workspace(&pool, "globex").await,
    );
    relay(&pool, a, "primary").await.unwrap();

    let err = relay(&pool, a, "primary").await.unwrap_err();
    assert_eq!(sqlstate(&err), "23505", "{err}");
    relay(&pool, b, "primary").await.unwrap();
}

#[sqlx::test]
async fn a_bucket_cannot_point_at_another_workspaces_relay(pool: PgPool) {
    let (a, b) = (
        workspace(&pool, "acme").await,
        workspace(&pool, "globex").await,
    );
    let relay_of_a = relay(&pool, a, "primary").await.unwrap();

    let err = sqlx::query(
        "insert into relay_buckets (relay_id, workspace_id, tokens) values ($1, $2, 60)",
    )
    .bind(relay_of_a)
    .bind(b)
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err), "23503", "{err}");
}

#[sqlx::test]
async fn a_bucket_cannot_go_negative(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let relay_id = relay(&pool, ws, "primary").await.unwrap();
    sqlx::query("insert into relay_buckets (relay_id, workspace_id, tokens) values ($1, $2, 1)")
        .bind(relay_id)
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();

    let err = sqlx::query("update relay_buckets set tokens = tokens - 2")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err), "23514", "{err}");
}
