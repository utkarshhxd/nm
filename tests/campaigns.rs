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
