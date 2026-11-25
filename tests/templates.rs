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
        ("html", Some(tree.clone()), Some("<p/>")),
        ("mjml", Some(tree.clone()), None),
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

/// A child table's composite foreign key needs a unique key on exactly `(workspace_id, id)`: creating
/// the child fails without it, and the reference then refuses another workspace's template.
#[sqlx::test]
async fn templates_workspace_id_id_key_backs_a_composite_foreign_key(pool: PgPool) {
    sqlx::query(
        "create table child (workspace_id uuid not null, template_id uuid not null,
         foreign key (workspace_id, template_id) references templates (workspace_id, id))",
    )
    .execute(&pool)
    .await
    .expect("composite foreign key needs templates_workspace_id_id_key");

    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let t = Uuid::now_v7();
    sqlx::query("insert into templates (id, workspace_id, name, kind, source) values ($1, $2, 't', 'html', 'x')")
        .bind(t)
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    let insert = |ws: Uuid| {
        sqlx::query("insert into child values ($1, $2)")
            .bind(ws)
            .bind(t)
            .execute(&pool)
    };
    insert(a).await.expect("own workspace");
    let err = insert(b).await.expect_err("other workspace");
    assert_eq!(
        err.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23503")
    );
}

#[sqlx::test]
async fn json_columns_must_be_objects(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    for bad in [json!(null), json!(7), json!([])] {
        let err = template(&pool, ws, "blocks", Some(bad.clone()), None)
            .await
            .expect_err("tree");
        assert!(is_check_violation(&err), "tree {bad}: {err}");
        for sql in [
            "insert into brand_kits (workspace_id, palette) values ($1, $2)",
            "insert into brand_kits (workspace_id, footer) values ($1, $2)",
        ] {
            let err = sqlx::query(sql)
                .bind(ws)
                .bind(bad.clone())
                .execute(&pool)
                .await;
            let err = err.expect_err("brand kit");
            assert!(is_check_violation(&err), "{sql} {bad}: {err}");
        }
    }
}
