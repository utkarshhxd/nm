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
    let bucket = |relay_id: Uuid, ws: Uuid| {
        sqlx::query(
            "insert into relay_buckets (relay_id, workspace_id, tokens) values ($1, $2, 60)",
        )
        .bind(relay_id)
        .bind(ws)
        .execute(&pool)
    };
    // A second workspace, untouched by the delete. Without it the `count(*) == 0` assertions below
    // would hold just as well on a cascade that emptied the whole table.
    let (doomed, bystander) = (
        workspace(&pool, "acme").await,
        workspace(&pool, "globex").await,
    );
    let doomed_relay = relay(&pool, doomed, "primary").await.unwrap();
    let surviving_relay = relay(&pool, bystander, "primary").await.unwrap();
    bucket(doomed_relay, doomed).await.unwrap();
    bucket(surviving_relay, bystander).await.unwrap();

    // The rows exist BEFORE the delete. Asserting only that they are gone afterwards cannot tell
    // a working cascade from an insert that never happened.
    for (table, n) in counts(&pool).await {
        assert_eq!(
            n, 2,
            "{table} should hold both workspaces' rows before the delete"
        );
    }

    sqlx::query("delete from workspaces where id = $1")
        .bind(doomed)
        .execute(&pool)
        .await
        .expect("the cascade must reach relays and relay_buckets");

    for (table, n) in counts(&pool).await {
        assert_eq!(n, 1, "{table}: the delete took the wrong number of rows");
    }
    let left: Uuid = sqlx::query_scalar("select workspace_id from relays")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        left, bystander,
        "the surviving relay belongs to the wrong workspace"
    );
}

/// Literal queries rather than a table name interpolated into one: sqlx 0.9 takes only
/// `&'static str`, and reaching for `AssertSqlSafe` to dodge that in a test would be teaching the
/// escape hatch for no gain.
async fn counts(pool: &PgPool) -> [(&'static str, i64); 2] {
    let one = |sql| sqlx::query_scalar::<_, i64>(sql).fetch_one(pool);
    [
        ("relays", one("select count(*) from relays").await.unwrap()),
        (
            "relay_buckets",
            one("select count(*) from relay_buckets").await.unwrap(),
        ),
    ]
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
async fn the_dns_checks_column_must_hold_an_object(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let insert = |domain: &'static str, checks: &'static str| {
        sqlx::query(
            "insert into sending_domains (id, workspace_id, domain, signing, dkim_selector, checks)
             values ($1, $2, $3, 'relay', 'nm1', $4::jsonb)",
        )
        .bind(Uuid::now_v7())
        .bind(ws)
        .bind(domain)
        .bind(checks)
        .execute(&pool)
    };

    // Every reader indexes into this by key, and jsonb takes a bare scalar or a null as happily
    // as an object.
    for bad in ["7", "\"ok\"", "null", "[]"] {
        let err = insert("a.example.com", bad).await.unwrap_err();
        assert_eq!(sqlstate(&err), "23514", "{bad} was accepted: {err}");
    }
    insert("b.example.com", "{}").await.unwrap();
    insert("c.example.com", r#"{"spf": "pass"}"#).await.unwrap();
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

    // Spending the LAST token must be legal: the bucket's whole job is to hand out exactly its
    // limit, and Task 8 debits it down to nothing on the batch that uses the last of it. Without
    // this half the test passes on `check (tokens > 0)`, which would refuse that batch.
    sqlx::query("update relay_buckets set tokens = tokens - 1")
        .execute(&pool)
        .await
        .expect("spending the last token is legal");

    let err = sqlx::query("update relay_buckets set tokens = tokens - 1")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err), "23514", "{err}");
}
