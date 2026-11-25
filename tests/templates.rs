// SPDX-License-Identifier: AGPL-3.0-only

//! Migration 0032: `templates` and `brand_kits`. Raw SQL on purpose: "exactly one of tree and
//! source" is the database's guarantee, so a handler that happens to check first proves nothing.

use serde_json::json;
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

async fn template(
    pool: &PgPool,
    ws: Uuid,
    kind: &str,
    tree: Option<serde_json::Value>,
    source: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "insert into templates (id, workspace_id, name, kind, tree, source)
         values ($1, $2, 't', $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(kind)
    .bind(tree)
    .bind(source)
    .execute(pool)
    .await
    .map(|_| ())
}

fn is_check_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|e| e.code().map(|c| c.into_owned()))
        .as_deref()
        == Some("23514")
}

#[sqlx::test]
async fn tables_exist_and_reject_a_tree_on_a_raw_template(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let tree = json!({"blocks": []});

    // A raw template carrying a tree, and a block template carrying none.
    for (kind, t, s) in [
        ("mjml", Some(tree.clone()), Some("<mjml/>")),
        ("html", Some(tree.clone()), None),
        ("blocks", None, None),
        ("blocks", None, Some("<mjml/>")),
        ("blocks", Some(tree.clone()), Some("<mjml/>")),
        ("mjml", None, None),
    ] {
        let err = template(&pool, ws, kind, t, s)
            .await
            .expect_err("must be refused");
        assert!(is_check_violation(&err), "{kind}: {err}");
    }

    // The three well-formed shapes are accepted.
    template(&pool, ws, "blocks", Some(tree), None)
        .await
        .expect("blocks");
    template(&pool, ws, "mjml", None, Some("<mjml/>"))
        .await
        .expect("mjml");
    template(&pool, ws, "html", None, Some("<p>hi</p>"))
        .await
        .expect("html");

    // An unknown kind is refused too.
    let err = template(&pool, ws, "docx", None, Some("x"))
        .await
        .expect_err("kind");
    assert!(is_check_violation(&err), "{err}");
}

#[sqlx::test]
async fn a_workspace_has_at_most_one_brand_kit(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let insert = || {
        sqlx::query("insert into brand_kits (workspace_id) values ($1)")
            .bind(ws)
            .execute(&pool)
    };
    insert().await.expect("first kit");
    let err = insert().await.expect_err("second kit");
    assert_eq!(
        err.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23505")
    );
}
