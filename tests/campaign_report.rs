// SPDX-License-Identifier: AGPL-3.0-only

//! `GET …/campaigns/{id}/report` (spec M2d §4.6, §5.2). Rows are written in raw SQL so the report
//! is read against exactly what the open, click and send paths leave behind.

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::workspaces::roles::Role;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

struct Tenant {
    ws: Uuid,
    viewer: TestApp,
}

async fn tenant(pool: &PgPool, slug: &str) -> Tenant {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let ws = create_workspace(pool, owner, slug).await;
    let email = format!("viewer-{slug}@example.com");
    let user = create_user(pool, &email, PW, false).await;
    add_member(pool, ws, user, Role::Viewer).await;
    let mut viewer = TestApp::new(pool.clone()).await;
    viewer.login(&email, PW).await;
    Tenant { ws, viewer }
}

/// A campaign frozen with one variant per entry of `variants`, each carrying that link table.
async fn campaign(pool: &PgPool, ws: Uuid, variants: Option<&[&[&str]]>) -> Uuid {
    let id = Uuid::now_v7();
    let frozen = variants.map(|vs| {
        let variants: Vec<Value> = vs
            .iter()
            .map(|links| {
                json!({"subject": [], "from_name": null, "rendition": {
                    "marker_format_version": 3, "fingerprint": "x", "nodes": [], "html": "",
                    "text": null, "fields": {}, "unsubscribe": null, "links": links}})
            })
            .collect();
        json!({"from": "a@mail.example.com", "reply_to": null,
               "sending_domain_id": Uuid::now_v7(), "variants": variants})
    });
    sqlx::query("insert into campaigns (id, workspace_id, name, frozen) values ($1, $2, 'c', $3)")
        .bind(id)
        .bind(ws)
        .bind(frozen)
        .execute(pool)
        .await
        .expect("campaign");
    id
}

/// A message; `opens` and `clicks` set `opened_at` and `clicked_at` when above zero.
async fn message(pool: &PgPool, ws: Uuid, c: Uuid, opens: i32, clicks: i32) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into messages (id, workspace_id, campaign_id, contact_id, opens, clicks,
                               opened_at, clicked_at)
         values ($1, $2, $3, $4, $5, $6,
                 case when $5 > 0 then now() end, case when $6 > 0 then now() end)",
    )
    .bind(id)
    .bind(ws)
    .bind(c)
    .bind(Uuid::now_v7())
    .bind(opens)
    .bind(clicks)
    .execute(pool)
    .await
    .expect("message");
    id
}

async fn click(pool: &PgPool, ws: Uuid, c: Uuid, m: Uuid, variant: i16, link: i32, clicks: i32) {
    sqlx::query(
        "insert into message_clicks (message_id, link, workspace_id, campaign_id, variant, clicks)
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(m)
    .bind(link)
    .bind(ws)
    .bind(c)
    .bind(variant)
    .bind(clicks)
    .execute(pool)
    .await
    .expect("click");
}

async fn report(t: &Tenant, c: Uuid) -> Value {
    let r = t
        .viewer
        .get(&format!("/api/v1/workspaces/{}/campaigns/{c}/report", t.ws))
        .await;
    r.assert_status_ok();
    r.json()
}

#[sqlx::test]
async fn another_workspaces_campaign_report_is_404(pool: PgPool) {
    let a = tenant(&pool, "a").await;
    let b = tenant(&pool, "b").await;
    let theirs = campaign(&pool, b.ws, Some(&[&["https://b.test/"]])).await;
    let m = message(&pool, b.ws, theirs, 1, 1).await;
    click(&pool, b.ws, theirs, m, 0, 0, 1).await;

    // Their id under our workspace, and under theirs without being a member.
    for ws in [a.ws, b.ws] {
        let r = a
            .viewer
            .get(&format!(
                "/api/v1/workspaces/{ws}/campaigns/{theirs}/report"
            ))
            .await;
        r.assert_status(StatusCode::NOT_FOUND);
        assert!(!r.text().contains("unique_opens"), "{}", r.text());
    }
    // Control: their own viewer reads it.
    assert_eq!(report(&b, theirs).await["unique_opens"], 1);
}

#[sqlx::test]
async fn a_viewer_reads_the_report(pool: PgPool) {
    let t = tenant(&pool, "a").await;
    let c = campaign(&pool, t.ws, Some(&[&["https://a.test/"]])).await;
    sqlx::query("update campaigns set sent = 3 where id = $1")
        .bind(c)
        .execute(&pool)
        .await
        .unwrap();
    let r = report(&t, c).await;
    assert_eq!(r["sent"], 3);
    assert_eq!(r["links"][0]["url"], "https://a.test/");
}

#[sqlx::test]
async fn a_draft_reports_zeros_and_no_links(pool: PgPool) {
    let t = tenant(&pool, "a").await;
    let c = campaign(&pool, t.ws, None).await;
    let zeros = json!({"sent": 0, "failed": 0, "bounced": 0, "unique_opens": 0,
                       "unique_clicks": 0, "unsubscribed": 0, "links": []});
    assert_eq!(report(&t, c).await, zeros);
    // A frozen column that no longer parses is an empty list, not a 500.
    sqlx::query("update campaigns set frozen = '{\"nope\": 1}' where id = $1")
        .bind(c)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(report(&t, c).await, zeros);
}

#[sqlx::test]
async fn the_report_counts_each_message_once(pool: PgPool) {
    let t = tenant(&pool, "a").await;
    let c = campaign(
        &pool,
        t.ws,
        Some(&[&["https://a.test/", "https://b.test/"]]),
    )
    .await;
    let m1 = message(&pool, t.ws, c, 3, 3).await;
    message(&pool, t.ws, c, 1, 0).await;
    message(&pool, t.ws, c, 0, 0).await;
    click(&pool, t.ws, c, m1, 0, 0, 2).await;
    click(&pool, t.ws, c, m1, 0, 1, 1).await;

    let r = report(&t, c).await;
    assert_eq!(r["unique_opens"], 2, "a repeat open is one");
    assert_eq!(r["unique_clicks"], 1, "a repeat click is one");
    assert_eq!(
        r["links"],
        json!([
            {"variant": 0, "link": 0, "url": "https://a.test/", "clicks": 2, "unique_clicks": 1},
            {"variant": 0, "link": 1, "url": "https://b.test/", "clicks": 1, "unique_clicks": 1},
        ])
    );
}

#[sqlx::test]
async fn a_click_alone_is_not_an_open(pool: PgPool) {
    // The report reads `opened_at` only. The click path sets it too, so this row is hand-built.
    let t = tenant(&pool, "a").await;
    let c = campaign(&pool, t.ws, Some(&[&["https://a.test/"]])).await;
    let m = message(&pool, t.ws, c, 0, 1).await;
    click(&pool, t.ws, c, m, 0, 0, 1).await;
    let r = report(&t, c).await;
    assert_eq!(r["unique_opens"], 0);
    assert_eq!(r["unique_clicks"], 1);
}

#[sqlx::test]
async fn every_frozen_link_is_listed_with_zeros_when_unclicked(pool: PgPool) {
    let t = tenant(&pool, "a").await;
    let variants: &[&[&str]] = &[
        &["https://a.test/"],
        &["https://x.test/", "https://y.test/"],
    ];
    let c = campaign(&pool, t.ws, Some(variants)).await;
    let m = message(&pool, t.ws, c, 0, 1).await;
    click(&pool, t.ws, c, m, 1, 1, 4).await;

    assert_eq!(
        report(&t, c).await["links"],
        json!([
            {"variant": 0, "link": 0, "url": "https://a.test/", "clicks": 0, "unique_clicks": 0},
            {"variant": 1, "link": 0, "url": "https://x.test/", "clicks": 0, "unique_clicks": 0},
            {"variant": 1, "link": 1, "url": "https://y.test/", "clicks": 4, "unique_clicks": 1},
        ])
    );
}

#[sqlx::test]
async fn bounced_is_its_own_count(pool: PgPool) {
    let t = tenant(&pool, "a").await;
    let c = campaign(&pool, t.ws, None).await;
    sqlx::query("update campaigns set sent = 5, failed = 1, bounced = 2 where id = $1")
        .bind(c)
        .execute(&pool)
        .await
        .unwrap();
    let r = report(&t, c).await;
    assert_eq!(
        (&r["sent"], &r["failed"], &r["bounced"]),
        (&json!(5), &json!(1), &json!(2))
    );
}

async fn contact(pool: &PgPool, ws: Uuid, email: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into contacts (id, workspace_id, email) values ($1, $2, $3)")
        .bind(id)
        .bind(ws)
        .bind(email)
        .execute(pool)
        .await
        .expect("contact");
    id
}

async fn unsubscribe(pool: &PgPool, ws: Uuid, contact: Uuid, list: Option<Uuid>, c: Uuid) {
    sqlx::query(
        "insert into consent_log (id, workspace_id, contact_id, list_id, action, source, source_ref)
         values ($1, $2, $3, $4, 'unsubscribed', 'one_click', $5)",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(contact)
    .bind(list)
    .bind(c.to_string())
    .execute(pool)
    .await
    .expect("consent row");
}

#[sqlx::test]
async fn an_unsubscribe_from_two_lists_counts_once(pool: PgPool) {
    let t = tenant(&pool, "a").await;
    let c = campaign(&pool, t.ws, None).await;
    let other = campaign(&pool, t.ws, None).await;
    let who = contact(&pool, t.ws, "x@example.com").await;
    let mut lists = vec![];
    for name in ["one", "two"] {
        let id = Uuid::now_v7();
        sqlx::query("insert into lists (id, workspace_id, name) values ($1, $2, $3)")
            .bind(id)
            .bind(t.ws)
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
        lists.push(Some(id));
    }
    unsubscribe(&pool, t.ws, who, lists[0], c).await;
    unsubscribe(&pool, t.ws, who, lists[1], c).await;
    unsubscribe(&pool, t.ws, who, lists[0], other).await;

    assert_eq!(report(&t, c).await["unsubscribed"], 1);
    assert_eq!(report(&t, other).await["unsubscribed"], 1);
}

#[sqlx::test]
async fn rows_carrying_another_workspace_are_not_counted(pool: PgPool) {
    let t = tenant(&pool, "a").await;
    let other = tenant(&pool, "b").await;
    let c = campaign(&pool, t.ws, Some(&[&["https://a.test/"]])).await;
    // These rows name this campaign but another workspace; no foreign key stops them (a message
    // would be refused by `messages_campaign_fk`), so the report's own predicate has to.
    click(&pool, other.ws, c, Uuid::now_v7(), 0, 0, 7).await;
    let who = contact(&pool, other.ws, "y@example.com").await;
    unsubscribe(&pool, other.ws, who, None, c).await;

    let r = report(&t, c).await;
    assert_eq!(r["unsubscribed"], 0);
    assert_eq!(r["links"][0]["clicks"], 0);
    assert_eq!(r["links"][0]["unique_clicks"], 0);
}
