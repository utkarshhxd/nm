// SPDX-License-Identifier: AGPL-3.0-only

//! The schema M1c's importer writes to: `imports`, `import_errors`, and the `import` provenance.

mod common;

use common::{PW, create_user, create_workspace};
use nm::lists::MembershipSource;
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(pool: &PgPool, slug: &str) -> Uuid {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    create_workspace(pool, owner, slug).await
}

async fn insert_import(pool: &PgPool, ws: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into imports (id, workspace_id, status, filename) values ($1, $2, 'pending', 'a.csv')")
        .bind(id)
        .bind(ws)
        .execute(pool)
        .await
        .expect("insert import");
    id
}

async fn insert_error(pool: &PgPool, ws: Uuid, import: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "insert into import_errors (import_id, workspace_id, row_number, reason) values ($1, $2, 1, 'bad')",
    )
    .bind(import)
    .bind(ws)
    .execute(pool)
    .await
    .map(|_| ())
}

#[sqlx::test]
async fn an_import_error_cannot_name_another_workspaces_import(pool: PgPool) {
    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let import_of_a = insert_import(&pool, a).await;

    // Control: the owning workspace may write it, so the refusal below is the foreign key's.
    insert_error(&pool, a, import_of_a)
        .await
        .expect("own workspace");
    let err = insert_error(&pool, b, import_of_a)
        .await
        .expect_err("cross-tenant");
    assert!(
        err.as_database_error()
            .is_some_and(|e| e.is_foreign_key_violation()),
        "expected a foreign key violation, got {err}"
    );
    let n: i64 = sqlx::query_scalar("select count(*) from import_errors")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test]
async fn import_source_round_trips(pool: PgPool) {
    let ws = workspace(&pool, "a").await;
    let (contact, list) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("insert into contacts (id, workspace_id, email) values ($1, $2, 'x@example.com')")
        .bind(contact)
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("insert into lists (id, workspace_id, name) values ($1, $2, 'news')")
        .bind(list)
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "insert into list_memberships (workspace_id, contact_id, list_id, status, source) \
         values ($1, $2, $3, 'subscribed', $4)",
    )
    .bind(ws)
    .bind(contact)
    .bind(list)
    .bind(MembershipSource::Import)
    .execute(&pool)
    .await
    .unwrap();

    let got: MembershipSource = sqlx::query_scalar(
        "select source from list_memberships where contact_id = $1 and list_id = $2",
    )
    .bind(contact)
    .bind(list)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(got, MembershipSource::Import);
    let raw: String =
        sqlx::query_scalar("select source from list_memberships where contact_id = $1")
            .bind(contact)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(raw, "import");
}

#[sqlx::test]
async fn an_import_cannot_name_another_workspaces_list(pool: PgPool) {
    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let list_of_a = Uuid::now_v7();
    sqlx::query("insert into lists (id, workspace_id, name) values ($1, $2, 'news')")
        .bind(list_of_a)
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    let insert = |ws: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "insert into imports (id, workspace_id, status, filename, list_id) \
                 values ($1, $2, 'pending', 'a.csv', $3)",
            )
            .bind(Uuid::now_v7())
            .bind(ws)
            .bind(list_of_a)
            .execute(&pool)
            .await
        }
    };
    insert(a).await.expect("own workspace");
    let err = insert(b).await.expect_err("cross-tenant");
    assert!(
        err.as_database_error()
            .is_some_and(|e| e.is_foreign_key_violation()),
        "expected a foreign key violation, got {err}"
    );

    // Deleting the list keeps the import and nulls only list_id.
    sqlx::query("delete from lists where id = $1")
        .bind(list_of_a)
        .execute(&pool)
        .await
        .unwrap();
    let (n, nulls): (i64, i64) =
        sqlx::query_as("select count(*), count(*) filter (where list_id is null) from imports where workspace_id = $1")
            .bind(a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((n, nulls), (1, 1));
}
