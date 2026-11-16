// SPDX-License-Identifier: AGPL-3.0-only

//! Selection snapshots (spec M1b section 8): `POST /contacts/selection` evaluates the predicate
//! once and writes the matching contact ids down, so that the set a bulk action runs against is
//! the set the person was shown. Issue `04` closes on this: a predicate re-evaluated when the
//! action runs resolves to whatever the table says then.
//!
//! Every test that claims a refusal reads the tables afterwards. A status code alone cannot tell
//! "refused" from "refused after writing a million rows", and the cap's whole job is the former.

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::{
    audit::Actor,
    contacts::{
        ContactFilter,
        selection::{MAX_SELECTION, materialise, reap_one_expired},
    },
    error::ApiError,
    http::rate_limit::{PREVIEW_PER_MINUTE, SELECTION_PER_MINUTE},
    workspaces::roles::Role,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

async fn client(pool: &PgPool, email: &str, ws: Uuid, role: Role) -> TestApp {
    let id = create_user(pool, email, PW, false).await;
    add_member(pool, ws, id, role).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    app
}

/// A workspace and a signed-in admin of it: an admin may do everything an editor may, and may
/// also define and archive custom fields, which the segment tests need.
async fn workspace(pool: &PgPool, slug: &str) -> (Uuid, TestApp) {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let ws = create_workspace(pool, owner, slug).await;
    let admin = client(pool, &format!("admin-{slug}@example.com"), ws, Role::Admin).await;
    (ws, admin)
}

fn base(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}")
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

fn problem_type(r: &axum_test::TestResponse) -> String {
    r.json::<Value>()["type"].as_str().unwrap().to_owned()
}

async fn contact(app: &TestApp, ws: Uuid, email: &str) -> Uuid {
    contact_with(app, ws, email, json!({})).await
}

async fn contact_with(app: &TestApp, ws: Uuid, email: &str, attributes: Value) -> Uuid {
    let r = app
        .post(&format!("{}/contacts", base(ws)))
        .json(&json!({"email": email, "attributes": attributes}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn tag(app: &TestApp, ws: Uuid, name: &str) -> Uuid {
    let r = app
        .post(&format!("{}/tags", base(ws)))
        .json(&json!({"name": name}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn apply_tag(app: &TestApp, ws: Uuid, contact: Uuid, tag_id: Uuid) {
    app.put(&format!("{}/contacts/{contact}/tags/{tag_id}", base(ws)))
        .await
        .assert_status_success();
}

async fn select(app: &TestApp, ws: Uuid, body: Value) -> axum_test::TestResponse {
    app.post(&format!("{}/contacts/selection", base(ws)))
        .json(&body)
        .await
}

/// `POST /contacts/selection`, asserted 200, as `(snapshot_id, selected)`.
async fn selection(app: &TestApp, ws: Uuid, body: Value) -> (Uuid, u64) {
    let r = select(app, ws, body).await;
    r.assert_status_ok();
    let body: Value = r.json();
    (
        body["snapshot_id"].as_str().unwrap().parse().unwrap(),
        body["selected"].as_u64().unwrap(),
    )
}

/// An editor API key for the workspace, as `(id, key)`.
async fn make_key(owner: &TestApp, ws: Uuid) -> (Uuid, String) {
    let r = owner
        .post(&format!("{}/api-keys", base(ws)))
        .json(&json!({"name": "ci", "role": "editor"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let body = r.json::<Value>();
    (
        body["id"].as_str().unwrap().parse().unwrap(),
        body["key"].as_str().unwrap().to_owned(),
    )
}

/// A selection made by an API key (Bearer, no cookie), as its snapshot id.
async fn key_selection(anon: &TestApp, ws: Uuid, key: &str) -> Uuid {
    let r = anon
        .post(&format!("{}/contacts/selection", base(ws)))
        .add_header("authorization", format!("Bearer {key}"))
        .json(&json!({}))
        .await;
    r.assert_status_ok();
    r.json::<Value>()["snapshot_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

/// The frozen set, straight from the table, in id order.
async fn members(pool: &PgPool, snapshot: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar(
        "select contact_id from selection_snapshot_members \
         where snapshot_id = $1 order by contact_id",
    )
    .bind(snapshot)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The `size` column, or `None` once the snapshot is gone.
async fn stored_size(pool: &PgPool, snapshot: Uuid) -> Option<i64> {
    sqlx::query_scalar("select size from selection_snapshots where id = $1")
        .bind(snapshot)
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

fn sorted(mut ids: Vec<Uuid>) -> Vec<Uuid> {
    ids.sort();
    ids
}

/// `size` is the materialising insert's own `rows_affected`, so the number the person is shown,
/// the number stored, and the rows actually written cannot disagree.
#[sqlx::test]
async fn size_is_the_rows_written_and_the_members_are_exactly_the_match(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let mut wanted = vec![];
    for i in 0..5 {
        let id = contact(&app, ws, &format!("c{i}@ex.test")).await;
        if i < 3 {
            apply_tag(&app, ws, id, vip).await;
            wanted.push(id);
        }
    }

    let (snapshot, selected) = selection(&app, ws, json!({"tag_id": vip})).await;
    assert_eq!(selected, 3, "three contacts carry the tag");
    assert_eq!(stored_size(&pool, snapshot).await, Some(3));
    assert_eq!(members(&pool, snapshot).await, sorted(wanted));
    assert_eq!(
        count(&pool, "select count(*) from selection_snapshot_members").await,
        3,
        "the two untagged contacts were not frozen"
    );
    // UUIDv7, so the issue time comes back out of the id and the TTL can be checked before the
    // database is touched. A v4 id would leave the TTL enforceable only by the row.
    assert_eq!(snapshot.get_version_num(), 7);
}

/// The compiler and the filter builder both emit fragments that assume the caller restricted
/// `workspace_id`; the materialising insert is the caller.
#[sqlx::test]
async fn a_contact_in_another_workspace_is_never_materialised(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (other, their_app) = workspace(&pool, "globex").await;
    let mine = vec![
        contact(&app, ws, "a@ex.test").await,
        contact(&app, ws, "b@ex.test").await,
    ];
    let theirs = vec![
        contact(&their_app, other, "c@ex.test").await,
        contact(&their_app, other, "d@ex.test").await,
        contact(&their_app, other, "e@ex.test").await,
    ];

    // No filter at all: "select everything", the case with nothing but the workspace clause.
    let (snapshot, selected) = selection(&app, ws, json!({})).await;
    assert_eq!(selected, 2);
    let frozen = members(&pool, snapshot).await;
    assert_eq!(frozen, sorted(mine));
    for id in theirs {
        assert!(
            !frozen.contains(&id),
            "a contact of another workspace was frozen: {id}"
        );
    }
    assert_eq!(
        count(&pool, "select count(*) from selection_snapshot_members").await,
        2,
        "the other workspace contributed rows"
    );
}

/// The cap takes one row past the limit as its only evidence and refuses **without naming a
/// count**: naming one needs a counting pass that doubles the work and races with the insert.
///
/// Driven through `materialise` with a cap of 2 rather than through the endpoint, because a
/// million contacts cannot be made in a test. Every line the refusal runs is the one the endpoint
/// runs; only the number differs, and the assertion below is that the number comes from the cap.
#[sqlx::test]
async fn the_cap_refuses_without_claiming_an_exact_count_and_writes_nothing(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    for i in 0..5 {
        contact(&app, ws, &format!("c{i}@ex.test")).await;
    }

    let err = materialise(
        pool.clone(),
        ws,
        Actor::System,
        json!({}),
        ContactFilter::default(),
        None,
        2,
    )
    .await
    .expect_err("five contacts against a cap of two");
    let ApiError::UnprocessableDetail { code, detail } = err else {
        panic!("not the cap's refusal");
    };
    assert_eq!(code, "selection-too-large");
    assert!(detail.contains("more than 2"), "{detail}");
    let digits: String = detail.chars().filter(char::is_ascii_digit).collect();
    assert_eq!(
        digits, "2",
        "the refusal names a count it cannot know: {detail}"
    );

    // Refused, not refused-after-writing: the parent row and every member row rolled back.
    assert_eq!(
        count(&pool, "select count(*) from selection_snapshots").await,
        0
    );
    assert_eq!(
        count(&pool, "select count(*) from selection_snapshot_members").await,
        0
    );

    // And the cap the endpoint applies is the spec's million, not the test's two.
    assert_eq!(MAX_SELECTION, 1_000_000);

    // A refusal past the quota check still carries the `RateLimit` headers the endpoint's
    // contract promises, and the quota it spent. A 404 for an unresolvable segment carries them
    // too: both are answers to a request that was counted.
    let r = select(&app, ws, json!({"q": "ab"})).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        r.header("ratelimit-limit").to_str().unwrap(),
        SELECTION_PER_MINUTE.to_string(),
        "a 422 dropped the headers the contract promises"
    );
    let r = select(&app, ws, json!({"segment_id": Uuid::now_v7()})).await;
    r.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(
        r.header("ratelimit-limit").to_str().unwrap(),
        SELECTION_PER_MINUTE.to_string(),
        "a 404 dropped the headers the contract promises"
    );
}

/// One selection per person per workspace: the UI has one active selection at a time, so the old
/// snapshot is dead weight, and dropping it bounds one person's footprint to one snapshot of up
/// to 158 MB. It must reach no further than that — not a colleague's, not another workspace's.
#[sqlx::test]
async fn a_new_selection_supersedes_only_the_actors_previous_one_in_that_workspace(pool: PgPool) {
    let (ws, mine) = workspace(&pool, "acme").await;
    let (other, _) = workspace(&pool, "globex").await;
    let colleague = client(&pool, "colleague@ex.test", ws, Role::Editor).await;
    // The same person, one workspace along.
    let elsewhere = {
        let id: Uuid =
            sqlx::query_scalar("select id from users where email = 'admin-acme@example.com'")
                .fetch_one(&pool)
                .await
                .unwrap();
        add_member(&pool, other, id, Role::Editor).await;
        let mut app = TestApp::new(pool.clone()).await;
        app.login("admin-acme@example.com", PW).await;
        app
    };
    contact(&mine, ws, "a@ex.test").await;
    contact(&elsewhere, other, "b@ex.test").await;

    let (first, _) = selection(&mine, ws, json!({})).await;
    let (theirs, _) = selection(&colleague, ws, json!({})).await;
    let (across, _) = selection(&elsewhere, other, json!({})).await;
    assert_eq!(
        members(&pool, first).await.len(),
        1,
        "a colleague's selection, or the same person's in another workspace,          superseded this one: the delete is not scoped to the actor and the workspace"
    );

    let (second, _) = selection(&mine, ws, json!({})).await;

    assert_eq!(stored_size(&pool, first).await, None, "not superseded");
    assert!(
        members(&pool, first).await.is_empty(),
        "the parent went but its members stayed: the cascade is not working"
    );
    assert!(stored_size(&pool, second).await.is_some(), "the new one");
    assert!(
        stored_size(&pool, theirs).await.is_some(),
        "a colleague's selection in the same workspace was deleted"
    );
    assert!(
        stored_size(&pool, across).await.is_some(),
        "the same person's selection in another workspace was deleted"
    );
}

/// An API key is an actor too, and its selections supersede each other. They cannot ride
/// `created_by`: that column takes a user, and NULL never equals NULL, so every key in the
/// workspace would share one pile that is never superseded — 600 live snapshots an hour at the
/// rate limit, against a reaper that clears 60.
#[sqlx::test]
async fn an_api_key_supersedes_only_its_own_previous_selection(pool: PgPool) {
    let (ws, owner) = workspace(&pool, "acme").await;
    contact(&owner, ws, "a@ex.test").await;
    let (_, first_key) = make_key(&owner, ws).await;
    let (_, second_key) = make_key(&owner, ws).await;
    let anon = TestApp::new(pool.clone()).await;

    let mine = key_selection(&anon, ws, &first_key).await;
    let theirs = key_selection(&anon, ws, &second_key).await;
    let (users, _) = selection(&owner, ws, json!({})).await;
    assert!(
        stored_size(&pool, mine).await.is_some(),
        "another key's selection superseded this one"
    );

    let second = key_selection(&anon, ws, &first_key).await;

    assert_eq!(
        stored_size(&pool, mine).await,
        None,
        "a key's own previous selection was not superseded"
    );
    assert!(
        members(&pool, mine).await.is_empty(),
        "the parent went but its members stayed"
    );
    assert!(stored_size(&pool, second).await.is_some(), "the new one");
    assert!(
        stored_size(&pool, theirs).await.is_some(),
        "another key's selection in the same workspace was deleted"
    );
    assert!(
        stored_size(&pool, users).await.is_some(),
        "a user's selection was deleted by a key's"
    );
    // The identity is recorded in the column that can hold it, not in the user one.
    let (by_user, by_key): (Option<Uuid>, Option<Uuid>) =
        sqlx::query_as("select created_by, created_by_key from selection_snapshots where id = $1")
            .bind(second)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(by_user, None, "a key is not a user");
    assert!(by_key.is_some(), "the key was not recorded");
}

/// **Both** snapshot tables are `unlogged` (section 7): ~158 MB per million rows would otherwise
/// go through WAL and into every base backup. The consequence is that a crash truncates them,
/// which is the same "select again" path the TTL already produces — so the decision is only safe
/// while the TTL path exists, and only real while this holds.
///
/// The parent is in this assertion because it is the half that is read. Logged, it would survive
/// a crash carrying its `size` with no members behind it, `resolve` and `require_live` would both
/// pass, and the action would answer `200 {selected: 0}` instead of `422 selection-expired` — an
/// empty page where the one thing this milestone insists on is that "nothing matched" and "your
/// selection is gone" are different answers.
#[sqlx::test]
async fn both_snapshot_tables_are_unlogged_so_a_crash_expires_the_selection(pool: PgPool) {
    let both: Vec<(String, String)> = sqlx::query_as(
        // `contacts` is the control: with both snapshot tables answering "u", an assertion over
        // those two alone would also pass on a server where every table came back unlogged.
        "select relname::text, relpersistence::text from pg_class \
         where relname in ('contacts', 'selection_snapshots', 'selection_snapshot_members') \
         order by relname",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        both,
        vec![
            ("contacts".to_owned(), "p".to_owned()),
            ("selection_snapshot_members".to_owned(), "u".to_owned()),
            ("selection_snapshots".to_owned(), "u".to_owned()),
        ]
    );
}

/// An expired snapshot is never extended. The one path in this change that could — a new
/// selection by the same actor — deletes it instead, and the new snapshot is a new id.
#[sqlx::test]
async fn an_expired_snapshot_is_dropped_rather_than_extended(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    contact(&app, ws, "a@ex.test").await;
    let (first, _) = selection(&app, ws, json!({})).await;
    sqlx::query("update selection_snapshots set expires_at = now() - interval '1 hour'")
        .execute(&pool)
        .await
        .unwrap();

    let (second, _) = selection(&app, ws, json!({})).await;
    assert_ne!(second, first, "a snapshot id is never reused");
    assert_eq!(stored_size(&pool, first).await, None);
    let live: i64 = count(
        &pool,
        "select count(*) from selection_snapshots where expires_at > now()",
    )
    .await;
    assert_eq!(live, 1, "only the new snapshot is live");
}

/// The reaper deletes **one** expired snapshot per pass. One, because a snapshot holds up to
/// 158 MB and its cascade is measured at 850 ms: a sweep of the whole backlog is unbounded work.
#[sqlx::test]
async fn the_sweep_deletes_one_expired_snapshot_and_leaves_the_rest(pool: PgPool) {
    let (ws, _app) = workspace(&pool, "acme").await;
    let mut expired = vec![];
    for i in 0..3 {
        expired.push(snapshot_row(&pool, ws, -(i64::from(i) + 1)).await);
    }
    let live = snapshot_row(&pool, ws, 1).await;
    assert_eq!(
        count(&pool, "select count(*) from selection_snapshots").await,
        4
    );
    assert_eq!(
        count(&pool, "select count(*) from selection_snapshot_members").await,
        8,
        "two members each"
    );

    assert!(reap_one_expired(&pool).await.unwrap(), "one to reap");
    assert_eq!(
        count(&pool, "select count(*) from selection_snapshots").await,
        3,
        "a pass deletes one, not every expired one"
    );
    assert_eq!(
        count(&pool, "select count(*) from selection_snapshot_members").await,
        6,
        "its members went with it"
    );

    assert!(reap_one_expired(&pool).await.unwrap());
    assert!(reap_one_expired(&pool).await.unwrap());
    assert!(
        !reap_one_expired(&pool).await.unwrap(),
        "nothing expired is left, so the pass reports no work"
    );
    let left: Vec<Uuid> = sqlx::query_scalar("select id from selection_snapshots")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(left, vec![live], "the live snapshot survived every pass");
    for id in expired {
        assert!(members(&pool, id).await.is_empty());
    }
}

/// A snapshot written straight to the table, expiring `hours` from now (negative for expired),
/// with two members. Members need no contact row: the table deliberately has no foreign key to
/// `contacts`.
async fn snapshot_row(pool: &PgPool, ws: Uuid, hours: i64) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into selection_snapshots (id, workspace_id, filter, size, expires_at) \
         values ($1, $2, '{}'::jsonb, 2, now() + make_interval(hours => $3::int))",
    )
    .bind(id)
    .bind(ws)
    .bind(i32::try_from(hours).unwrap())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into selection_snapshot_members (workspace_id, snapshot_id, contact_id) \
         values ($1, $2, $3), ($1, $2, $4)",
    )
    .bind(ws)
    .bind(id)
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .unwrap();
    id
}

/// A selection over a saved segment resolves its rules with archived definitions **included**.
/// Archiving a custom field keeps every contact's value, so it must not break a saved segment
/// that names the field — the opposite of a save or a preview, which are write-shaped and must
/// refuse a *new* rule on an archived field.
#[sqlx::test]
async fn a_selection_over_a_segment_still_resolves_once_its_field_is_archived(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let r = app
        .post(&format!("{}/custom-fields", base(ws)))
        .json(&json!({"key": "plan", "label": "plan", "kind": "text"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let field = id_of(&r);
    let pro = contact_with(&app, ws, "pro@ex.test", json!({"plan": "pro"})).await;
    contact_with(&app, ws, "free@ex.test", json!({"plan": "free"})).await;
    let r = app
        .post(&format!("{}/segments", base(ws)))
        .json(&json!({
            "name": "pro",
            "rules": {"match": "all", "rules": [
                {"field": "attr:plan", "op": "eq", "value": "pro"}]},
        }))
        .await;
    r.assert_status(StatusCode::CREATED);
    let segment = id_of(&r);

    app.delete(&format!("{}/custom-fields/{field}", base(ws)))
        .await
        .assert_status_ok();

    let r = select(&app, ws, json!({"segment_id": segment})).await;
    r.assert_status_ok();
    let body: Value = r.json();
    assert_eq!(body["selected"], 1, "{body}");
    let snapshot: Uuid = body["snapshot_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(members(&pool, snapshot).await, vec![pro]);
    // The token names the snapshot and carries no predicate, so a segment selection yields one
    // like any other. `tests/bulk.rs` acts on it and pins that it reaches exactly the frozen set.
    assert!(
        body["selection_token"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "a selection over a segment must still issue a token: {body}"
    );
}

/// A `segment_id` that resolves to nothing is refused, never read as "no rules" — which, with no
/// other filter, is every contact in the workspace, at `bulk/delete`.
#[sqlx::test]
async fn a_segment_of_another_workspace_is_a_404_and_freezes_nothing(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (other, their_app) = workspace(&pool, "globex").await;
    contact(&app, ws, "a@ex.test").await;
    let r = their_app
        .post(&format!("{}/segments", base(other)))
        .json(&json!({
            "name": "theirs",
            "rules": {"match": "all", "rules": [
                {"field": "first_name", "op": "eq", "value": "Ann"}]},
        }))
        .await;
    r.assert_status(StatusCode::CREATED);
    let theirs = id_of(&r);

    for id in [theirs, Uuid::now_v7()] {
        select(&app, ws, json!({"segment_id": id}))
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    assert_eq!(
        count(&pool, "select count(*) from selection_snapshots").await,
        0,
        "a refused selection froze a set anyway"
    );
}

/// At least as hard as preview, because this is the more expensive call of the two: measured at
/// 4,887 ms and 158 MB for a million rows, against preview's 1.5 s of one connection.
#[sqlx::test]
async fn selection_is_rate_limited_per_workspace_and_reports_its_headers(pool: PgPool) {
    const {
        assert!(
            SELECTION_PER_MINUTE <= PREVIEW_PER_MINUTE,
            "selection must be limited at least as hard as preview"
        );
    }
    let (ws, app) = workspace(&pool, "acme").await;
    let (other, _) = workspace(&pool, "globex").await;
    sqlx::query(
        "insert into memberships (workspace_id, user_id, role) \
         select $1, user_id, 'admin' from memberships where workspace_id = $2",
    )
    .bind(other)
    .bind(ws)
    .execute(&pool)
    .await
    .unwrap();

    for i in 1..=SELECTION_PER_MINUTE {
        let r = select(&app, ws, json!({})).await;
        r.assert_status_ok();
        assert_eq!(
            r.header("ratelimit-remaining").to_str().unwrap(),
            (SELECTION_PER_MINUTE - i).to_string(),
            "request {i} of {SELECTION_PER_MINUTE}"
        );
    }
    let r = select(&app, ws, json!({})).await;
    r.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(problem_type(&r), "urn:nm:problem:rate-limited");
    assert_eq!(
        r.header("ratelimit-limit").to_str().unwrap(),
        SELECTION_PER_MINUTE.to_string()
    );
    let retry: u64 = r.header("retry-after").to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry), "retry-after = {retry}");

    // The window is per workspace: the same person, one workspace along, is not refused.
    select(&app, other, json!({})).await.assert_status_ok();
}

/// The limit is checked **after** `authorize`, so someone who is not in the workspace cannot
/// spend its quota and deny selections to the people who are.
#[sqlx::test]
async fn a_non_member_is_refused_and_spends_none_of_the_workspaces_quota(pool: PgPool) {
    let (ws, mut app) = workspace(&pool, "acme").await;
    create_user(&pool, "outsider@ex.test", PW, false).await;
    app.login("outsider@ex.test", PW).await;

    for i in 0..=SELECTION_PER_MINUTE {
        let r = select(&app, ws, json!({})).await;
        r.assert_status(StatusCode::NOT_FOUND);
        assert!(
            r.maybe_header("ratelimit-limit").is_none(),
            "request {i} told a non-member about the workspace's quota"
        );
    }

    app.login("admin-acme@example.com", PW).await;
    let r = select(&app, ws, json!({})).await;
    r.assert_status_ok();
    assert_eq!(
        r.header("ratelimit-remaining").to_str().unwrap(),
        (SELECTION_PER_MINUTE - 1).to_string(),
        "the member's first selection must be the window's first"
    );
}
