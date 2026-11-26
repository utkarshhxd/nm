// SPDX-License-Identifier: AGPL-3.0-only

//! The tracking routes (spec M2d §4.1–4.3): `GET /o/{token}`, the open pixel, and
//! `GET /l/{token}/{link}`, the click redirect. Campaigns and messages are written in raw SQL, as
//! `tests/campaign_report.rs` writes them, so each test names exactly the link table it reads.

mod common;

use axum::http::{Method, StatusCode};
use axum_test::TestResponse;
use chrono::{DateTime, Utc};
use common::{PW, TestApp, create_user, create_workspace};
use nm::{
    config::SecretKey,
    crypto::secrets::Secrets,
    http::rate_limit::TRACK_PER_MINUTE,
    public::{
        token::{self, Purpose, Token},
        track,
    },
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

struct Fx {
    app: TestApp,
    pool: PgPool,
    ws: Uuid,
}

/// A workspace, and an app signed in as its owner.
async fn fx_with(pool: &PgPool, trust_proxy_headers: bool) -> Fx {
    let owner = create_user(pool, "owner@example.com", PW, false).await;
    let ws = create_workspace(pool, owner, "acme").await;
    let mut app = TestApp::with_config(pool.clone(), |c| {
        c.trust_proxy_headers = trust_proxy_headers
    })
    .await;
    app.login("owner@example.com", PW).await;
    Fx {
        app,
        pool: pool.clone(),
        ws,
    }
}

async fn fx(pool: &PgPool) -> Fx {
    fx_with(pool, false).await
}

impl Fx {
    /// A campaign frozen with one variant per entry of `variants`, each carrying that link table.
    async fn campaign(&self, variants: &[&[&str]]) -> Uuid {
        let id = Uuid::now_v7();
        let variants: Vec<Value> = variants
            .iter()
            .map(|links| {
                json!({"subject": [], "from_name": null, "rendition": {
                    "marker_format_version": 3, "fingerprint": "x", "nodes": [], "html": "",
                    "text": null, "fields": {}, "unsubscribe": null, "links": links}})
            })
            .collect();
        let frozen = json!({"from": "a@mail.example.com", "reply_to": null,
                            "sending_domain_id": Uuid::now_v7(), "variants": variants});
        sqlx::query(
            "insert into campaigns (id, workspace_id, name, frozen) values ($1, $2, 'c', $3)",
        )
        .bind(id)
        .bind(self.ws)
        .bind(frozen)
        .execute(&self.pool)
        .await
        .expect("campaign");
        id
    }

    /// A sent message of `campaign`'s `variant` to `contact`, and its tracking token.
    async fn message_to(&self, campaign: Uuid, variant: i16, contact: Uuid) -> (Uuid, String) {
        let id = Uuid::now_v7();
        sqlx::query(
            "insert into messages (id, workspace_id, campaign_id, contact_id, variant, status)
             values ($1, $2, $3, $4, $5, 'sent')",
        )
        .bind(id)
        .bind(self.ws)
        .bind(campaign)
        .bind(contact)
        .bind(variant)
        .execute(&self.pool)
        .await
        .expect("message");
        (id, track::issue(&self.app.state.secrets, id))
    }

    async fn message(&self, campaign: Uuid, variant: i16) -> (Uuid, String) {
        self.message_to(campaign, variant, Uuid::now_v7()).await
    }

    async fn marks(&self, m: Uuid) -> Marks {
        let (opens, clicks, opened_at, clicked_at): (i32, i32, _, _) = sqlx::query_as(
            "select opens, clicks, opened_at, clicked_at from messages where id = $1",
        )
        .bind(m)
        .fetch_one(&self.pool)
        .await
        .expect("the message");
        Marks {
            opens,
            clicks,
            opened_at,
            clicked_at,
        }
    }

    /// `(link, clicks)` of the message's `message_clicks` rows, by link.
    async fn link_clicks(&self, m: Uuid) -> Vec<(i32, i32)> {
        sqlx::query_as(
            "select link, clicks from message_clicks where message_id = $1 order by link",
        )
        .bind(m)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn get(&self, path: &str) -> TestResponse {
        self.app.server.get(path).await
    }
}

#[derive(Debug, PartialEq)]
struct Marks {
    opens: i32,
    clicks: i32,
    opened_at: Option<DateTime<Utc>>,
    clicked_at: Option<DateTime<Utc>>,
}

/// A token naming `m` with a MAC from another instance's key: well-formed, and forged.
fn forged(m: Uuid) -> String {
    track::issue(&Secrets::new(&SecretKey([9u8; 32])), m)
}

/// What the pixel answers: everything a client could compare.
fn pixel_answer(r: &TestResponse) -> (StatusCode, String, String, Vec<u8>) {
    let h = |name: &str| {
        r.maybe_header(name)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    };
    (
        r.status_code(),
        h("content-type"),
        h("cache-control"),
        r.as_bytes().to_vec(),
    )
}

#[track_caller]
fn assert_not_valid(r: &TestResponse, what: &str) {
    assert_eq!(r.status_code(), StatusCode::NOT_FOUND, "{what}");
    assert!(r.maybe_header("location").is_none(), "{what} redirected");
    assert!(
        r.text().contains("This link is not valid"),
        "{what}: {}",
        r.text()
    );
}

#[sqlx::test]
async fn the_pixel_records_the_first_open_once_and_counts_every_open(pool: PgPool) {
    let f = fx(&pool).await;
    let c = f.campaign(&[&["https://a.test/"]]).await;
    let (m, t) = f.message(c, 0).await;

    f.get(&format!("/o/{t}")).await.assert_status_ok();
    let first = f.marks(m).await;
    assert_eq!(first.opens, 1);
    let opened_at = first.opened_at.expect("the first open sets opened_at");

    f.get(&format!("/o/{t}")).await.assert_status_ok();
    assert_eq!(
        f.marks(m).await,
        Marks {
            opens: 2,
            clicks: 0,
            opened_at: Some(opened_at),
            clicked_at: None,
        },
        "every open counts, and the first is kept"
    );
}

#[sqlx::test]
async fn the_pixel_answers_the_same_for_a_forged_an_unknown_and_a_real_token(pool: PgPool) {
    let f = fx(&pool).await;
    let c = f.campaign(&[&["https://a.test/"]]).await;
    let (m, t) = f.message(c, 0).await;
    let unknown = track::issue(&f.app.state.secrets, Uuid::now_v7());

    let mut answers = Vec::new();
    for path in [
        "/o/garbage".to_owned(),
        "/o/%FF".to_owned(),
        format!("/o/{}", forged(m)),
        format!("/o/{unknown}"),
    ] {
        let r = f.get(&path).await;
        for name in ["ratelimit-limit", "ratelimit-remaining", "ratelimit-reset"] {
            assert!(r.maybe_header(name).is_none(), "{path} carries {name}");
        }
        answers.push((path, pixel_answer(&r)));
    }
    assert_eq!(f.marks(m).await.opens, 0, "a forged token moved the row");
    let real = pixel_answer(&f.get(&format!("/o/{t}")).await);
    assert_eq!(f.marks(m).await.opens, 1, "the real one moves its row");

    assert_eq!(
        real,
        (
            StatusCode::OK,
            "image/gif".to_owned(),
            "no-store".to_owned(),
            track::PIXEL.to_vec()
        )
    );
    for (path, answer) in answers {
        assert_eq!(answer, real, "{path} answers differently from a real token");
    }
}

#[sqlx::test]
async fn a_head_records_nothing(pool: PgPool) {
    let f = fx(&pool).await;
    let c = f.campaign(&[&["https://a.test/"]]).await;
    let (m, t) = f.message(c, 0).await;

    let r = f.app.server.method(Method::HEAD, &format!("/o/{t}")).await;
    r.assert_status_ok();
    assert_eq!(r.header("content-type"), "image/gif");
    let r = f
        .app
        .server
        .method(Method::HEAD, &format!("/l/{t}/0"))
        .await;
    assert_eq!(
        r.status_code(),
        StatusCode::FOUND,
        "a HEAD is answered as a GET"
    );
    assert_eq!(r.header("location"), "https://a.test/");

    assert_eq!(
        f.marks(m).await,
        Marks {
            opens: 0,
            clicks: 0,
            opened_at: None,
            clicked_at: None,
        }
    );
    assert!(f.link_clicks(m).await.is_empty());
}

#[sqlx::test]
async fn a_click_redirects_to_the_frozen_link_and_records_it_once_per_message_and_link(
    pool: PgPool,
) {
    let f = fx(&pool).await;
    let c = f
        .campaign(&[&["https://a.test/x?y=1&z=%20", "http://b.test/"]])
        .await;
    let (m, t) = f.message(c, 0).await;

    for (link, want) in [
        (0, "https://a.test/x?y=1&z=%20"),
        (0, "https://a.test/x?y=1&z=%20"),
        (1, "http://b.test/"),
    ] {
        let r = f.get(&format!("/l/{t}/{link}")).await;
        assert_eq!(r.status_code(), StatusCode::FOUND, "link {link}");
        assert_eq!(r.header("location"), want, "link {link}");
        assert!(r.maybe_header("ratelimit-remaining").is_some());
    }

    let marks = f.marks(m).await;
    assert_eq!(
        (marks.clicks, marks.opens),
        (3, 0),
        "a click is not an open"
    );
    assert!(marks.clicked_at.is_some(), "clicked_at");
    assert!(marks.opened_at.is_some(), "a click implies an open");
    assert_eq!(f.link_clicks(m).await, [(0, 2), (1, 1)]);
}

#[sqlx::test]
async fn a_forged_token_an_unknown_message_and_an_index_past_the_table_are_the_same_404(
    pool: PgPool,
) {
    let f = fx(&pool).await;
    let c = f.campaign(&[&["https://a.test/"]]).await;
    let (m, t) = f.message(c, 0).await;
    let unknown = track::issue(&f.app.state.secrets, Uuid::now_v7());

    let mut bodies = Vec::new();
    for path in [
        format!("/l/{}/0", forged(m)),
        format!("/l/{unknown}/0"),
        "/l/garbage/0".to_owned(),
        format!("/l/{t}/1"),
        format!("/l/{t}/999"),
        format!("/l/{t}/abc"),
        format!("/l/{t}/-1"),
        format!("/l/{t}/4294967296"),
    ] {
        let r = f.get(&path).await;
        assert_not_valid(&r, &path);
        bodies.push(r.text());
    }
    bodies.dedup();
    assert_eq!(bodies.len(), 1, "two refusals differ: {bodies:#?}");
    assert_eq!(
        f.marks(m).await,
        Marks {
            opens: 0,
            clicks: 0,
            opened_at: None,
            clicked_at: None,
        },
        "a refused click recorded something"
    );
    assert!(f.link_clicks(m).await.is_empty());
}

#[sqlx::test]
async fn the_redirect_target_comes_only_from_the_messages_own_campaign(pool: PgPool) {
    let f = fx(&pool).await;
    let a = f.campaign(&[&["https://a.test/"]]).await;
    let b = f
        .campaign(&[&["https://b0.test/", "https://b1.test/", "https://b2.test/"]])
        .await;
    let (_, ta) = f.message(a, 0).await;
    let (_, tb) = f.message(b, 0).await;

    let r = f.get(&format!("/l/{ta}/0?url=https://evil.test")).await;
    assert_eq!(r.status_code(), StatusCode::FOUND);
    assert_eq!(
        r.header("location"),
        "https://a.test/",
        "the query string named the target"
    );
    assert_not_valid(&f.get(&format!("/l/{ta}/2")).await, "A's token, B's index");
    assert_eq!(
        f.get(&format!("/l/{tb}/2")).await.header("location"),
        "https://b2.test/"
    );

    // And from the message's own variant.
    let v = f
        .campaign(&[&["https://v0.test/"], &["https://v1.test/"]])
        .await;
    let (_, tv) = f.message(v, 1).await;
    assert_eq!(
        f.get(&format!("/l/{tv}/0")).await.header("location"),
        "https://v1.test/"
    );
}

#[sqlx::test]
async fn a_frozen_link_that_is_not_http_is_never_followed(pool: PgPool) {
    let f = fx(&pool).await;
    let c = f.campaign(&[&["https://a.test/", "https://b.test/"]]).await;
    let (m, t) = f.message(c, 0).await;
    sqlx::query(
        r#"update campaigns
              set frozen = jsonb_set(frozen, '{variants,0,rendition,links}',
                                     '["javascript:alert(1)", "//evil.test"]')
            where id = $1"#,
    )
    .bind(c)
    .execute(&f.pool)
    .await
    .unwrap();

    for link in [0, 1] {
        assert_not_valid(
            &f.get(&format!("/l/{t}/{link}")).await,
            &format!("link {link}"),
        );
    }
    assert_eq!(f.marks(m).await.clicks, 0);
    assert!(f.link_clicks(m).await.is_empty());
}

#[sqlx::test]
async fn the_tracking_routes_spend_their_own_per_ip_budget(pool: PgPool) {
    let f = fx_with(&pool, true).await;
    let c = f.campaign(&[&["https://a.test/"]]).await;
    let contact = Uuid::now_v7();
    sqlx::query("insert into contacts (id, workspace_id, email) values ($1, $2, 'p@ex.test')")
        .bind(contact)
        .bind(f.ws)
        .execute(&f.pool)
        .await
        .unwrap();
    let (m, t) = f.message_to(c, 0, contact).await;
    let from =
        |ip: &'static str, path: String| f.app.server.get(&path).add_header("x-forwarded-for", ip);
    let ip = "198.51.100.20";

    // Forged, so none of these does any database work.
    for n in 0..TRACK_PER_MINUTE {
        let r = from(ip, format!("/o/{}", forged(m))).await;
        assert_eq!(r.status_code(), StatusCode::OK, "pixel {n}");
    }

    let r = from(ip, format!("/l/{t}/0")).await;
    assert_eq!(r.status_code(), StatusCode::TOO_MANY_REQUESTS);
    assert!(r.maybe_header("retry-after").is_some());
    assert!(r.maybe_header("location").is_none());
    assert_eq!(r.header("content-type"), "text/html; charset=utf-8");

    let r = from(ip, format!("/o/{t}")).await;
    assert_eq!(pixel_answer(&r).0, StatusCode::OK);
    assert_eq!(r.as_bytes().as_ref(), track::PIXEL);
    assert_eq!(
        f.marks(m).await.opens,
        0,
        "an open over budget was recorded"
    );

    // The public pages' budget is untouched, and so is another address's.
    let unsubscribe = token::issue(
        &f.app.state.secrets,
        &Token {
            workspace_id: f.ws,
            contact_id: contact,
            purpose: Purpose::Unsubscribe,
            list_id: None,
            issued_at: Utc::now().timestamp(),
            nonce: "n".into(),
            kid: None,
            ref_id: None,
        },
    );
    from(ip, format!("/u/{unsubscribe}"))
        .await
        .assert_status_ok();
    let r = from("198.51.100.21", format!("/l/{t}/0")).await;
    assert_eq!(r.status_code(), StatusCode::FOUND);
}

#[sqlx::test]
async fn an_erased_contacts_message_still_redirects_and_keeps_only_ids(pool: PgPool) {
    let f = fx(&pool).await;
    let r = f
        .app
        .post(&format!("/api/v1/workspaces/{}/contacts", f.ws))
        .json(&json!({"email": "alice+x@example.test"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let contact: Uuid = r.json::<Value>()["id"].as_str().unwrap().parse().unwrap();
    let c = f.campaign(&[&["https://a.test/"]]).await;
    let (m, t) = f.message_to(c, 0, contact).await;

    f.app
        .post(&format!(
            "/api/v1/workspaces/{}/contacts/{contact}/erase",
            f.ws
        ))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let r = f.get(&format!("/l/{t}/0")).await;
    assert_eq!(r.status_code(), StatusCode::FOUND);
    assert_eq!(r.header("location"), "https://a.test/");
    f.get(&format!("/o/{t}")).await.assert_status_ok();
    assert!(!t.contains('@'));

    let rows: Vec<String> = sqlx::query_scalar(
        "select row_to_json(m)::text from messages m where id = $1
         union all
         select row_to_json(k)::text from message_clicks k where message_id = $1",
    )
    .bind(m)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in rows {
        assert!(!row.contains('@'), "a stored row carries an address: {row}");
    }
}
