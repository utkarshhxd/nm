// SPDX-License-Identifier: AGPL-3.0-only

mod common;

use std::collections::HashSet;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::workspaces::roles::Role;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// `(email local part, first name, last name)` in INSERTION order. Chosen so that three orders
/// differ: by email, by creation, and by reverse creation. `fixture` asserts it, because an
/// ordering test over a fixture where two of them coincide would pass against a broken query.
/// `mia` and `kai` share a full name; only `zoe` contains "oe", only `ben` has the last name Zulu.
const SEED: [(&str, &str, &str); 10] = [
    ("mia", "Pat", "Quinn"),
    ("zoe", "Ann", "Lee"),
    ("ben", "Bo", "Zulu"),
    ("uma", "Cy", "Park"),
    ("ada", "Di", "Park"),
    ("kai", "Pat", "Quinn"),
    ("rex", "Ed", "Ng"),
    ("cy", "Fi", "Ng"),
    ("ned", "Gus", "Ong"),
    ("fay", "Hal", "Pike"),
];

fn contacts_path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/contacts")
}

fn email(local: &str) -> String {
    format!("{local}@ex.test")
}

async fn editor(pool: &PgPool, ws: Uuid, who: &str, role: Role) -> TestApp {
    let id = create_user(pool, &format!("{who}@example.com"), PW, false).await;
    add_member(pool, ws, id, role).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(&format!("{who}@example.com"), PW).await;
    app
}

async fn add_contact(app: &TestApp, ws: Uuid, email: &str, first: &str, last: &str) -> Uuid {
    let r = app
        .post(&contacts_path(ws))
        .json(&json!({"email": email, "first_name": first, "last_name": last}))
        .await;
    r.assert_status(StatusCode::CREATED);
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

struct Fixture {
    ws: Uuid,
    app: TestApp,
    /// `(email, id)` in insertion order.
    seeded: Vec<(String, Uuid)>,
}

impl Fixture {
    fn emails_by_creation(&self) -> Vec<String> {
        self.seeded.iter().map(|(e, _)| e.clone()).collect()
    }

    fn emails_by_reverse_creation(&self) -> Vec<String> {
        let mut v = self.emails_by_creation();
        v.reverse();
        v
    }

    fn emails_alphabetical(&self) -> Vec<String> {
        let mut v = self.emails_by_creation();
        v.sort();
        v
    }

    /// Every page of `?limit=&query` followed to the end.
    async fn pages(&self, limit: u32, query: &str) -> Vec<Value> {
        let mut pages: Vec<Value> = vec![];
        let mut cursor = String::new();
        loop {
            let url = format!("{}?limit={limit}&{query}{cursor}", contacts_path(self.ws));
            let r = self.app.get(&url).await;
            r.assert_status_ok();
            let page: Value = r.json();
            let next = page["next_cursor"].as_str().map(str::to_owned);
            pages.push(page);
            match next {
                Some(c) => cursor = format!("&cursor={c}"),
                None => return pages,
            }
            assert!(pages.len() <= SEED.len() + 1, "pagination does not end");
        }
    }

    async fn emails(&self, limit: u32, query: &str) -> Vec<String> {
        self.pages(limit, query)
            .await
            .iter()
            .flat_map(|p| p["data"].as_array().unwrap())
            .map(|c| c["email"].as_str().unwrap().to_owned())
            .collect()
    }
}

async fn fixture(pool: &PgPool) -> Fixture {
    let owner = create_user(pool, "owner@example.com", PW, false).await;
    let ws = create_workspace(pool, owner, "acme").await;
    let app = editor(pool, ws, "editor", Role::Editor).await;
    let mut seeded = vec![];
    for (local, first, last) in SEED {
        let e = email(local);
        let id = add_contact(&app, ws, &e, first, last).await;
        seeded.push((e, id));
    }
    let f = Fixture { ws, app, seeded };
    let (by_email, by_creation, by_reverse) = (
        f.emails_alphabetical(),
        f.emails_by_creation(),
        f.emails_by_reverse_creation(),
    );
    assert_ne!(
        by_email, by_creation,
        "fixture: email order is creation order"
    );
    assert_ne!(
        by_email, by_reverse,
        "fixture: email order is reverse creation"
    );
    assert_ne!(by_creation, by_reverse);
    f
}

#[sqlx::test]
async fn sorts_by_email_ascending_across_pages(pool: PgPool) {
    let f = fixture(&pool).await;
    let pages = f.pages(3, "sort=email").await;
    // 10 rows at 3 a page: three page boundaries, and the last page is short.
    let sizes: Vec<usize> = pages
        .iter()
        .map(|p| p["data"].as_array().unwrap().len())
        .collect();
    assert_eq!(sizes, [3, 3, 3, 1]);
    assert_eq!(f.emails(3, "sort=email").await, f.emails_alphabetical());
    // Descending is the exact reverse.
    let mut desc = f.emails_alphabetical();
    desc.reverse();
    assert_eq!(f.emails(3, "sort=email&dir=desc").await, desc);
}

#[sqlx::test]
async fn sorts_by_created_descending_by_default(pool: PgPool) {
    let f = fixture(&pool).await;
    assert_eq!(f.emails(3, "").await, f.emails_by_reverse_creation());
    assert_eq!(
        f.emails(3, "sort=created").await,
        f.emails_by_reverse_creation()
    );
    assert_eq!(
        f.emails(4, "sort=created&dir=asc").await,
        f.emails_by_creation()
    );
}

#[sqlx::test]
async fn search_matches_email_substring(pool: PgPool) {
    let f = fixture(&pool).await;
    // Not a prefix, and not the case it was stored in.
    assert_eq!(f.emails(50, "q=oe@").await, [email("zoe")]);
    assert_eq!(f.emails(50, "q=ZOE").await, [email("zoe")]);
}

#[sqlx::test]
async fn search_matches_name(pool: PgPool) {
    let f = fixture(&pool).await;
    assert_eq!(f.emails(50, "q=zul").await, [email("ben")]);
    // Two contacts with the same full name, paged one at a time.
    assert_eq!(
        f.emails(1, "q=quinn&sort=email").await,
        [email("kai"), email("mia")]
    );
}

#[sqlx::test]
async fn search_below_three_characters_is_422(pool: PgPool) {
    let f = fixture(&pool).await;
    for q in ["o", "oe"] {
        let r = f.app.get(&format!("{}?q={q}", contacts_path(f.ws))).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(r.json::<Value>()["errors"][0]["field"], "q");
    }
    // A blank box is no search, not an error.
    assert_eq!(f.emails(50, "q=").await.len(), SEED.len());
}

#[sqlx::test]
async fn search_treats_wildcards_literally(pool: PgPool) {
    let f = fixture(&pool).await;
    for q in ["___", "%25%25%25", "%5C%5C%5C"] {
        assert!(f.emails(50, &format!("q={q}")).await.is_empty(), "{q}");
    }
}

#[sqlx::test]
async fn paging_never_repeats_or_skips_a_row(pool: PgPool) {
    let f = fixture(&pool).await;
    // Another workspace's contact must never show up.
    let other_owner = create_user(&pool, "other@example.com", PW, false).await;
    let other_ws = create_workspace(&pool, other_owner, "other").await;
    let other = editor(&pool, other_ws, "other-editor", Role::Editor).await;
    add_contact(&other, other_ws, "aaa@ex.test", "", "").await;

    let seeded: HashSet<Uuid> = f.seeded.iter().map(|(_, id)| *id).collect();
    for query in [
        "",
        "sort=created&dir=asc",
        "sort=email",
        "sort=email&dir=desc",
        // The filter and the cursor together; every address contains this.
        "sort=email&q=ex.test",
        "sort=created&q=ex.test",
    ] {
        for limit in [1, 3, 4, 5, 10, 11] {
            let pages = f.pages(limit, query).await;
            let ids: Vec<Uuid> = pages
                .iter()
                .flat_map(|p| p["data"].as_array().unwrap())
                .map(|c| c["id"].as_str().unwrap().parse().unwrap())
                .collect();
            let unique: HashSet<Uuid> = ids.iter().copied().collect();
            let ctx = format!("limit={limit} {query}");
            assert_eq!(ids.len(), unique.len(), "a row repeated: {ctx}");
            assert_eq!(unique, seeded, "a row skipped or leaked: {ctx}");
            // A page that is exactly full and last carries no cursor, so no empty page follows.
            let expected_pages = SEED.len().div_ceil(limit as usize);
            assert_eq!(pages.len(), expected_pages, "{ctx}");
        }
    }
}

#[sqlx::test]
async fn bad_cursors_and_sorts_are_422_and_non_members_404(pool: PgPool) {
    let f = fixture(&pool).await;
    let base = contacts_path(f.ws);
    let created_cursor = f.pages(3, "").await[0]["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    for q in [
        "cursor=%25%25%25".to_owned(),
        "sort=name".to_owned(),
        // A `created` cursor under the `email` sort must not restart the list.
        format!("sort=email&cursor={created_cursor}"),
    ] {
        let r = f.app.get(&format!("{base}?{q}")).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }

    create_user(&pool, "stranger@example.com", PW, false).await;
    let mut stranger = TestApp::new(pool.clone()).await;
    stranger.login("stranger@example.com", PW).await;
    stranger.get(&base).await.assert_status_not_found();
}

#[sqlx::test]
async fn filters_by_list_id(pool: PgPool) {
    let f = fixture(&pool).await;
    let lists = format!("/api/v1/workspaces/{}/lists", f.ws);
    let mut list_ids = vec![];
    for name in ["mine", "theirs"] {
        // Single opt-in, so no status is `pending` and the test says nothing about that.
        let r = f
            .app
            .post(&lists)
            .json(&json!({"name": name, "double_optin": false}))
            .await;
        r.assert_status(StatusCode::CREATED);
        list_ids.push(r.json::<Value>()["id"].as_str().unwrap().to_owned());
    }
    let (mine, theirs) = (&list_ids[0], &list_ids[1]);
    let members = |list: &str| format!("{lists}/{list}/members");
    // Five of the ten, picked so that email order and creation order differ among them.
    let member_idx = [0, 2, 3, 6, 9];
    for i in member_idx {
        let (_, id) = f.seeded[i];
        let r = f
            .app
            .post(&members(mine))
            .json(&json!({"contact_id": id}))
            .await;
        r.assert_status_ok();
    }
    // Another list's members must not leak in.
    let r = f
        .app
        .post(&members(theirs))
        .json(&json!({"contact_id": f.seeded[1].1}))
        .await;
    r.assert_status_ok();
    // An unsubscribed member keeps its row, and is still listed: the filter asks for the list's
    // membership rows, whatever their status.
    f.app
        .delete(&format!("{}/{}", members(mine), f.seeded[3].1))
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let creation: Vec<String> = member_idx.iter().map(|&i| f.seeded[i].0.clone()).collect();
    let mut alphabetical = creation.clone();
    alphabetical.sort();
    assert_ne!(
        creation, alphabetical,
        "fixture: the members' two orders coincide"
    );
    let mut newest_first = creation.clone();
    newest_first.reverse();

    // Page sizes that put a boundary mid-list (2) and exactly at its end (5).
    for limit in [2, 5] {
        assert_eq!(
            f.emails(limit, &format!("list_id={mine}&sort=email")).await,
            alphabetical,
            "limit={limit}"
        );
        assert_eq!(
            f.emails(limit, &format!("list_id={mine}")).await,
            newest_first,
            "limit={limit}"
        );
    }
    let sizes: Vec<usize> = f
        .pages(2, &format!("list_id={mine}&sort=email"))
        .await
        .iter()
        .map(|p| p["data"].as_array().unwrap().len())
        .collect();
    assert_eq!(sizes, [2, 2, 1]);
    assert_eq!(
        f.emails(50, &format!("list_id={theirs}")).await,
        [f.seeded[1].0.clone()]
    );
    // Combined with a search: `quinn` is mia (a member) and kai (not).
    assert_eq!(
        f.emails(50, &format!("list_id={mine}&q=quinn")).await,
        [email("mia")]
    );

    // Another workspace's list matches nothing, and neither does one that does not exist.
    let other_owner = create_user(&pool, "other@example.com", PW, false).await;
    let other_ws = create_workspace(&pool, other_owner, "other").await;
    let other = editor(&pool, other_ws, "other-editor", Role::Editor).await;
    let r = other
        .post(&format!("/api/v1/workspaces/{other_ws}/lists"))
        .json(&json!({"name": "mine"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let foreign = r.json::<Value>()["id"].as_str().unwrap().to_owned();
    for list in [foreign, Uuid::now_v7().to_string()] {
        assert!(f.emails(50, &format!("list_id={list}")).await.is_empty());
    }
    let r = f
        .app
        .get(&format!("{}?list_id=nope", contacts_path(f.ws)))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test]
async fn list_status_narrows_list_id(pool: PgPool) {
    let f = fixture(&pool).await;
    let lists = format!("/api/v1/workspaces/{}/lists", f.ws);
    // Double opt-in, so adding writes `pending`.
    let r = f.app.post(&lists).json(&json!({"name": "doi"})).await;
    r.assert_status(StatusCode::CREATED);
    let list = r.json::<Value>()["id"].as_str().unwrap().to_owned();
    let members = format!("{lists}/{list}/members");
    // Contacts 0..6 are added (pending); then 1 and 2 are made subscribed and 3, 4, 5 unsubscribed,
    // leaving 0 pending. Contacts 6..10 are on no list.
    let ids: Vec<Uuid> = f.seeded.iter().map(|(_, id)| *id).collect();
    for id in &ids[..6] {
        let r = f.app.post(&members).json(&json!({"contact_id": id})).await;
        r.assert_status_ok();
    }
    // There is no confirmation flow yet, so make the subscribed ones directly.
    for id in &ids[1..3] {
        sqlx::query("update list_memberships set status = 'subscribed' where contact_id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    for id in &ids[3..6] {
        f.app
            .delete(&format!("{members}/{id}"))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    let by_creation = |range: std::ops::Range<usize>| -> Vec<String> {
        f.seeded[range]
            .iter()
            .rev()
            .map(|(e, _)| e.clone())
            .collect()
    };
    // No status: every membership row, as before.
    assert_eq!(
        f.emails(2, &format!("list_id={list}")).await,
        by_creation(0..6)
    );
    for (status, expected) in [
        ("pending", by_creation(0..1)),
        ("subscribed", by_creation(1..3)),
        ("unsubscribed", by_creation(3..6)),
    ] {
        // Page size 1 puts a boundary between every two rows of the narrowed result.
        for limit in [1, 2, 50] {
            assert_eq!(
                f.emails(limit, &format!("list_id={list}&list_status={status}"))
                    .await,
                expected,
                "{status} limit={limit}"
            );
        }
    }
    // Contacts that are not on the list are not matched by any status.
    assert!(
        !f.emails(50, &format!("list_id={list}&list_status=pending"))
            .await
            .contains(&f.seeded[9].0)
    );

    // A status says nothing without a list, and an unknown one is not a status.
    for q in [
        "list_status=pending".to_owned(),
        format!("list_id={list}&list_status=bogus"),
    ] {
        let r = f.app.get(&format!("{}?{q}", contacts_path(f.ws))).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
}

/// Puts the tag on the i-th seeded contact.
async fn put(f: &Fixture, tag: &str, i: usize) {
    let url = format!(
        "/api/v1/workspaces/{}/contacts/{}/tags/{tag}",
        f.ws, f.seeded[i].1
    );
    f.app.put(&url).await.assert_status(StatusCode::NO_CONTENT);
}

#[sqlx::test]
async fn filters_by_tag_id(pool: PgPool) {
    let f = fixture(&pool).await;
    let tags = format!("/api/v1/workspaces/{}/tags", f.ws);
    let mut tag_ids = vec![];
    for name in ["mine", "theirs"] {
        let r = f.app.post(&tags).json(&json!({"name": name})).await;
        r.assert_status(StatusCode::CREATED);
        tag_ids.push(r.json::<Value>()["id"].as_str().unwrap().to_owned());
    }
    let (mine, theirs) = (&tag_ids[0], &tag_ids[1]);
    let tagged = [0, 2, 3, 6, 9];
    for i in tagged {
        put(&f, mine, i).await;
    }
    // Another tag's contacts must not leak in.
    put(&f, theirs, 1).await;
    put(&f, theirs, 2).await;

    let creation: Vec<String> = tagged.iter().map(|&i| f.seeded[i].0.clone()).collect();
    let mut alphabetical = creation.clone();
    alphabetical.sort();
    assert_ne!(
        creation, alphabetical,
        "fixture: the tagged contacts' two orders coincide"
    );
    let mut newest_first = creation.clone();
    newest_first.reverse();

    // Page sizes that put a boundary mid-list (2) and exactly at its end (5).
    for limit in [2, 5] {
        assert_eq!(
            f.emails(limit, &format!("tag_id={mine}&sort=email")).await,
            alphabetical,
            "limit={limit}"
        );
        assert_eq!(
            f.emails(limit, &format!("tag_id={mine}")).await,
            newest_first,
            "limit={limit}"
        );
    }
    let sizes: Vec<usize> = f
        .pages(2, &format!("tag_id={mine}&sort=email"))
        .await
        .iter()
        .map(|p| p["data"].as_array().unwrap().len())
        .collect();
    assert_eq!(sizes, [2, 2, 1]);
    // `zoe` and `ben`, the two the other tag was put on.
    let mut other = f.emails(50, &format!("tag_id={theirs}&sort=email")).await;
    other.sort();
    assert_eq!(other, [email("ben"), email("zoe")]);
    // Combined with a search: `quinn` is mia (tagged) and kai (not).
    assert_eq!(
        f.emails(50, &format!("tag_id={mine}&q=quinn")).await,
        [email("mia")]
    );

    // Another workspace's tag matches nothing, and neither does one that does not exist.
    let other_owner = create_user(&pool, "other@example.com", PW, false).await;
    let other_ws = create_workspace(&pool, other_owner, "other").await;
    let other = editor(&pool, other_ws, "other-editor", Role::Editor).await;
    let r = other
        .post(&format!("/api/v1/workspaces/{other_ws}/tags"))
        .json(&json!({"name": "mine"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let foreign = r.json::<Value>()["id"].as_str().unwrap().to_owned();
    for tag in [foreign, Uuid::now_v7().to_string()] {
        assert!(f.emails(50, &format!("tag_id={tag}")).await.is_empty());
    }
    let r = f
        .app
        .get(&format!("{}?tag_id=nope", contacts_path(f.ws)))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}
