// SPDX-License-Identifier: AGPL-3.0-only

//! Bulk actions over a **frozen set** (spec M1b section 8). `POST /contacts/selection` writes the
//! matching ids down; a bulk action narrows `contacts` by that set and re-evaluates nothing.
//!
//! Every test that claims a refusal reads the tables afterwards: a status code alone cannot tell
//! "refused" from "refused after deleting four thousand people".

mod common;

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::{
    contacts::selection::{MAX_EXCLUDED, MAX_IDS, Selection, Token, issue},
    crypto::tokens::hmac_sha256,
    workspaces::roles::Role,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::{NoContext, Timestamp, Uuid};

async fn client(pool: &PgPool, email: &str, ws: Uuid, role: Role) -> TestApp {
    let id = create_user(pool, email, PW, false).await;
    add_member(pool, ws, id, role).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    app
}

/// A workspace and a signed-in editor of it.
async fn workspace(pool: &PgPool, slug: &str) -> (Uuid, TestApp) {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let ws = create_workspace(pool, owner, slug).await;
    let editor = client(
        pool,
        &format!("editor-{slug}@example.com"),
        ws,
        Role::Editor,
    )
    .await;
    (ws, editor)
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
    let r = app
        .post(&format!("{}/contacts", base(ws)))
        .json(&json!({"email": email}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn contacts(app: &TestApp, ws: Uuid, prefix: &str, n: usize) -> Vec<Uuid> {
    let mut ids = vec![];
    for i in 0..n {
        ids.push(contact(app, ws, &format!("{prefix}{i}@ex.test")).await);
    }
    ids
}

async fn tag(app: &TestApp, ws: Uuid, name: &str) -> Uuid {
    let r = app
        .post(&format!("{}/tags", base(ws)))
        .json(&json!({"name": name}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn list(app: &TestApp, ws: Uuid, name: &str, double_optin: bool) -> Uuid {
    let r = app
        .post(&format!("{}/lists", base(ws)))
        .json(&json!({"name": name, "double_optin": double_optin}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

async fn segment(app: &TestApp, ws: Uuid, name: &str, rules: Value) -> Uuid {
    let r = app
        .post(&format!("{}/segments", base(ws)))
        .json(&json!({"name": name, "rules": rules}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

/// What `POST /contacts/selection` answered: the token, the frozen set's id, and the number the
/// person was shown — which is what a bulk action must be told it confirmed.
#[derive(Clone)]
struct Sel {
    token: String,
    snapshot_id: Uuid,
    selected: u64,
}

impl Sel {
    /// The target fields of a bulk body for this selection: the token and the number the
    /// confirmation showed, which with nothing unticked is the whole frozen set.
    fn target(&self) -> Value {
        json!({"selection_token": self.token, "confirmed_count": self.selected})
    }

    /// Unticking rows, as the grid does it: the confirmation's number drops by one per unticked
    /// row, which is right exactly while the rows shown are the rows frozen.
    fn excluding(&self, ids: &[Uuid]) -> Value {
        self.excluding_confirming(ids, self.selected - ids.len() as u64)
    }

    /// Unticking rows while the confirmation showed `confirmed` — the two come apart when the
    /// grid is showing the live filter and a row that is not in the frozen set is unticked.
    fn excluding_confirming(&self, ids: &[Uuid], confirmed: u64) -> Value {
        body(
            self.target(),
            json!({"excluded_ids": ids, "confirmed_count": confirmed}),
        )
    }
}

/// A bulk body: a target's fields plus the action's own.
fn body(target: Value, extra: Value) -> Value {
    let mut out = target;
    for (k, v) in extra.as_object().unwrap() {
        out[k] = v.clone();
    }
    out
}

async fn select(app: &TestApp, ws: Uuid, filter: Value) -> Sel {
    let r = app
        .post(&format!("{}/contacts/selection", base(ws)))
        .json(&filter)
        .await;
    r.assert_status_ok();
    let v: Value = r.json();
    Sel {
        token: v["selection_token"].as_str().unwrap().to_owned(),
        snapshot_id: v["snapshot_id"].as_str().unwrap().parse().unwrap(),
        selected: v["selected"].as_u64().unwrap(),
    }
}

async fn bulk(app: &TestApp, ws: Uuid, action: &str, body: Value) -> axum_test::TestResponse {
    app.post(&format!("{}/contacts/bulk/{action}", base(ws)))
        .json(&body)
        .await
}

async fn affected(r: axum_test::TestResponse) -> u64 {
    r.assert_status_ok();
    r.json::<Value>()["affected"].as_u64().unwrap()
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

async fn tagged(pool: &PgPool, tag: Uuid) -> Vec<Uuid> {
    sqlx::query_scalar("select contact_id from contact_tags where tag_id = $1 order by contact_id")
        .bind(tag)
        .fetch_all(pool)
        .await
        .unwrap()
}

fn sorted(mut ids: Vec<Uuid>) -> Vec<Uuid> {
    ids.sort();
    ids
}

/// Tags every contact in `ids`, as a second editor would.
async fn apply_tag(app: &TestApp, ws: Uuid, tag: Uuid, ids: &[Uuid]) {
    for id in ids {
        app.put(&format!("{}/contacts/{id}/tags/{tag}", base(ws)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
}

async fn remove_tag(app: &TestApp, ws: Uuid, tag: Uuid, id: Uuid) {
    app.delete(&format!("{}/contacts/{id}/tags/{tag}", base(ws)))
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

async fn join_list(app: &TestApp, ws: Uuid, id: Uuid, list_id: Uuid) {
    app.post(&format!("{}/contacts/{id}/lists", base(ws)))
        .json(&json!({"list_id": list_id}))
        .await
        .assert_status_success();
}

async fn status_of(pool: &PgPool, contact: Uuid, list: Uuid) -> Option<String> {
    sqlx::query_scalar("select status from list_memberships where contact_id = $1 and list_id = $2")
        .bind(contact)
        .bind(list)
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn set_status(pool: &PgPool, contact: Uuid, list: Uuid, status: &str) {
    sqlx::query("update list_memberships set status = $3 where contact_id = $1 and list_id = $2")
        .bind(contact)
        .bind(list)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
}

/// The audit row for the one bulk action a test ran.
async fn audit_meta(pool: &PgPool) -> Value {
    sqlx::query_scalar("select metadata from audit_log where action like 'contacts.bulk_%'")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The review-focus case. A contact that lands between the click and the action must not be acted
/// on, whatever the action is: it is not in the frozen set.
#[sqlx::test]
async fn contacts_created_after_the_selection_are_not_acted_on(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let before = contacts(&app, ws, "before", 3).await;
    let sel = select(&app, ws, json!({})).await;
    let after = contacts(&app, ws, "after", 2).await;

    let n = affected(bulk(&app, ws, "tag", body(sel.target(), json!({"tag_id": vip}))).await).await;
    assert_eq!(n, 3);
    assert_eq!(
        tagged(&pool, vip).await,
        sorted(before.clone()),
        "only the contacts that were frozen"
    );

    // Deleting is the case that cannot be undone: the late arrivals must survive it.
    let n = affected(bulk(&app, ws, "delete", sel.target()).await).await;
    assert_eq!(n, 3);
    let left: Vec<Uuid> = sqlx::query_scalar("select id from contacts order by id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(left, after);
}

/// Issue `04`, the whole reason the set is materialised. A colleague tags three more contacts into
/// the filter inside the token's hour. Under a re-evaluated predicate the action hit all four;
/// here it hits the one that was frozen, and the other three are untouched — on `bulk/delete`,
/// alive.
#[sqlx::test]
async fn a_tag_applied_after_the_selection_does_not_bring_its_contact_in(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let churn = tag(&app, ws, "churn").await;
    let ids = contacts(&app, ws, "c", 4).await;
    apply_tag(&app, ws, churn, &ids[..1]).await;

    let sel = select(&app, ws, json!({"tag_id": churn})).await;
    assert_eq!(sel.selected, 1);
    apply_tag(&app, ws, churn, &ids[1..]).await;

    let r = bulk(&app, ws, "delete", sel.target()).await;
    r.assert_status_ok();
    let v: Value = r.json();
    assert_eq!(
        (&v["issued"], &v["selected"], &v["affected"]),
        (&json!(1), &json!(1), &json!(1)),
        "the frozen set is one contact however the filter moved"
    );
    let left: Vec<Uuid> = sqlx::query_scalar("select id from contacts order by id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(left, sorted(ids[1..].to_vec()), "three were never selected");
}

/// The other direction, which a time bound could never give: a contact that was frozen and has
/// since stopped matching is **still acted on**. The person confirmed these contacts, not this
/// query.
#[sqlx::test]
async fn a_tag_removed_after_the_selection_does_not_take_its_contact_out(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (churn, done) = (tag(&app, ws, "churn").await, tag(&app, ws, "done").await);
    let ids = contacts(&app, ws, "c", 3).await;
    apply_tag(&app, ws, churn, &ids[..2]).await;

    let sel = select(&app, ws, json!({"tag_id": churn})).await;
    assert_eq!(sel.selected, 2);
    // A second editor unties one of them from the filter.
    remove_tag(&app, ws, churn, ids[0]).await;

    let n =
        affected(bulk(&app, ws, "tag", body(sel.target(), json!({"tag_id": done}))).await).await;
    assert_eq!(n, 2, "both frozen contacts, including the one that left");
    assert_eq!(tagged(&pool, done).await, sorted(ids[..2].to_vec()));
}

/// Negation is what killed the timestamp design (section 8.1), so it is tested in both directions.
/// `not_has` bounded by a timestamp inverts: delete the tag row and the bounded `NOT EXISTS`
/// becomes true, so a contact that was never shown is acted on. A frozen set cannot invert.
#[sqlx::test]
async fn not_has_is_frozen_in_both_directions(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (churn, done) = (tag(&app, ws, "churn").await, tag(&app, ws, "done").await);
    // `without` has no tag and is selected; `with` has it and is not.
    let without = contact(&app, ws, "without@ex.test").await;
    let with = contact(&app, ws, "with@ex.test").await;
    apply_tag(&app, ws, churn, &[with]).await;
    let seg = segment(
        &app,
        ws,
        "untagged",
        json!({"match": "all", "rules": [{"field": "tag", "op": "not_has", "value": churn}]}),
    )
    .await;

    let sel = select(&app, ws, json!({"segment_id": seg})).await;
    assert_eq!(sel.selected, 1, "only the contact without the tag");

    // The superset violation: the tag row that kept `with` out is deleted. A re-evaluated
    // `not_has` now matches it.
    remove_tag(&app, ws, churn, with).await;
    // And the subset half: `without` is tagged, so a re-evaluated `not_has` drops it.
    apply_tag(&app, ws, churn, &[without]).await;

    let n =
        affected(bulk(&app, ws, "tag", body(sel.target(), json!({"tag_id": done}))).await).await;
    assert_eq!(n, 1);
    assert_eq!(
        tagged(&pool, done).await,
        vec![without],
        "the action followed the predicate rather than the frozen set"
    );
}

/// The same inversion through a list, which section 8.1 calls the re-solicitation predicate: an
/// unsubscribe hides a membership that existed at selection time, so a bounded `not_member_of`
/// pulls somebody who *was* on the list into a "not on the list" action.
#[sqlx::test]
async fn not_member_of_is_frozen_in_both_directions(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let news = list(&app, ws, "news", false).await;
    let done = tag(&app, ws, "done").await;
    let off = contact(&app, ws, "off@ex.test").await;
    let on = contact(&app, ws, "on@ex.test").await;
    join_list(&app, ws, on, news).await;
    let seg = segment(
        &app,
        ws,
        "not-subscribed",
        json!({"match": "all", "rules": [
            {"field": "list", "op": "not_member_of", "value": news, "status": "subscribed"}]}),
    )
    .await;

    let sel = select(&app, ws, json!({"segment_id": seg})).await;
    assert_eq!(sel.selected, 1, "only the contact off the list");

    // `on` unsubscribes, which makes a re-evaluated `not_member_of(subscribed)` true for them.
    app.post(&format!(
        "{}/contacts/{on}/lists/{news}/unsubscribe",
        base(ws)
    ))
    .await
    .assert_status_success();
    // `off` joins, which makes it false for them.
    join_list(&app, ws, off, news).await;

    let n =
        affected(bulk(&app, ws, "tag", body(sel.target(), json!({"tag_id": done}))).await).await;
    assert_eq!(n, 1);
    assert_eq!(
        tagged(&pool, done).await,
        vec![off],
        "an unsubscribe pulled somebody into a 'not on the list' action"
    );
}

/// The action narrows `contacts` rather than reading ids out of the members table. Reading them
/// out and inserting them elsewhere hits a foreign-key violation the moment one selected contact
/// has been deleted since selection — a 500 that kills the whole action. Narrowed, the deleted
/// contact is simply absent and the count is short.
#[sqlx::test]
async fn deleting_a_selected_contact_leaves_the_action_working_and_the_count_short(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let news = list(&app, ws, "news", false).await;
    let ids = contacts(&app, ws, "c", 3).await;
    let sel = select(&app, ws, json!({})).await;
    assert_eq!(sel.selected, 3);

    app.delete(&format!("{}/contacts/{}", base(ws), ids[0]))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // The snapshot still names the contact that is gone.
    let frozen: i64 = sqlx::query_scalar("select count(*) from selection_snapshot_members")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(frozen, 3, "the members row is deliberately left dangling");

    // Writes through a foreign key: `contact_tags` and `list_memberships` both reference
    // `contacts`, so an id taken out of the members table would abort these statements.
    let r = bulk(&app, ws, "tag", body(sel.target(), json!({"tag_id": vip}))).await;
    r.assert_status_ok();
    let v: Value = r.json();
    assert_eq!(
        (&v["issued"], &v["selected"], &v["affected"]),
        (&json!(3), &json!(2), &json!(2)),
        "the action worked and reported the gap"
    );
    assert_eq!(tagged(&pool, vip).await, sorted(ids[1..].to_vec()));

    let r = bulk(
        &app,
        ws,
        "subscribe",
        body(sel.target(), json!({"list_id": news})),
    )
    .await;
    assert_eq!(affected(r).await, 2);
    let n = affected(bulk(&app, ws, "delete", sel.target()).await).await;
    assert_eq!(n, 2);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 0);
}

/// The TTL is enforced from the row as well as from the token, so expiry does not wait for the
/// reaper — which clears one snapshot a minute and can be hours behind (section 8.5).
#[sqlx::test]
async fn an_expired_but_unswept_snapshot_is_refused(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    contacts(&app, ws, "c", 2).await;
    let sel = select(&app, ws, json!({})).await;

    sqlx::query("update selection_snapshots set expires_at = now() - interval '1 minute'")
        .execute(&pool)
        .await
        .unwrap();
    // Unswept: the rows are all still there, so nothing but the `expires_at` check can refuse.
    assert_eq!(
        count(&pool, "select count(*) from selection_snapshot_members").await,
        2
    );

    let r = bulk(&app, ws, "delete", sel.target()).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem_type(&r), "urn:nm:problem:selection-expired");
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);
    assert_eq!(
        count(
            &pool,
            "select count(*) from audit_log where action like 'contacts.bulk_%'"
        )
        .await,
        0
    );
}

/// The `sel` CTE's own half of that check, driven through the engine because the handler refuses
/// an expired snapshot before the statement is ever built. Remove the CTE's join and this deletes
/// two contacts under an expired selection.
#[sqlx::test]
async fn the_statement_itself_refuses_an_expired_snapshot(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    contacts(&app, ws, "c", 2).await;
    let sel = select(&app, ws, json!({})).await;
    let frozen = Selection::Snapshot {
        token: Token {
            workspace_id: ws,
            snapshot_id: sel.snapshot_id,
            issued: sel.selected,
        },
        filter: json!({}),
    };
    let mut conn = pool.acquire().await.unwrap();

    // Live: the statement resolves the set, which is what makes the refusal below meaningful.
    // `unsubscribe` against a list nobody is on changes nothing, so only `selected` is read.
    let live = nm::contacts::selection::unsubscribe(&mut conn, &frozen, &[], Uuid::now_v7())
        .await
        .unwrap();
    assert_eq!(
        live.selected, 2,
        "the live snapshot resolves to its members"
    );

    sqlx::query("update selection_snapshots set expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let expired = nm::contacts::selection::delete(&mut conn, &frozen, &[])
        .await
        .unwrap();
    assert_eq!(
        (expired.selected, expired.affected),
        (0, 0),
        "an expired snapshot resolved to its members anyway"
    );
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);
}

/// A snapshot that is live in the table and a token that is older than the TTL: the refusal has to
/// come from the token's own age, before the database is touched, or a Friday token acts on
/// Monday. The accepted half is the same shapes one minute the other side of the hour, so this
/// cannot pass by refusing everything.
#[sqlx::test]
async fn a_token_older_than_the_ttl_is_refused_although_its_snapshot_is_live(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let ids = contacts(&app, ws, "c", 2).await;
    let hour = std::time::Duration::from_secs(60 * 60);
    let minute = std::time::Duration::from_secs(60);

    let old = aged_snapshot(&pool, &app, ws, hour + minute, &ids[..1]).await;
    let r = bulk(&app, ws, "delete", old).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem_type(&r), "urn:nm:problem:selection-expired");
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);

    let fresh = aged_snapshot(&pool, &app, ws, hour - minute, &ids[1..]).await;
    assert_eq!(affected(bulk(&app, ws, "delete", fresh).await).await, 1);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 1);
}

/// A snapshot whose id was minted `age` ago, holding `members`, with an hour of life left in the
/// table. Returns the bulk target for it: a token this instance signed.
async fn aged_snapshot(
    pool: &PgPool,
    app: &TestApp,
    ws: Uuid,
    age: std::time::Duration,
    members: &[Uuid],
) -> Value {
    let then = std::time::SystemTime::now() - age;
    let secs = then
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let snapshot_id = Uuid::new_v7(Timestamp::from_unix(NoContext, secs, 0));
    sqlx::query(
        "insert into selection_snapshots (id, workspace_id, filter, size, expires_at) \
         values ($1, $2, '{}'::jsonb, $3, now() + interval '1 hour')",
    )
    .bind(snapshot_id)
    .bind(ws)
    .bind(i64::try_from(members.len()).unwrap())
    .execute(pool)
    .await
    .unwrap();
    for id in members {
        sqlx::query(
            "insert into selection_snapshot_members (workspace_id, snapshot_id, contact_id) \
             values ($1, $2, $3)",
        )
        .bind(ws)
        .bind(snapshot_id)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }
    let token = issue(
        &app.state.secrets,
        &Token {
            workspace_id: ws,
            snapshot_id,
            issued: members.len() as u64,
        },
    );
    json!({"selection_token": token, "confirmed_count": members.len()})
}

/// The label bumped to `v3` because the payload's meaning changed. A `v2` token — correctly signed
/// under `nm:selection:v2` by this instance's key — must fail the HMAC rather than deserialise
/// into something this build misreads.
///
/// The v3 half is minted the same way, so a mistake in this test's own HMAC cannot make the v2
/// half pass vacuously.
#[sqlx::test]
async fn a_v2_token_is_refused_after_the_label_bump(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    contacts(&app, ws, "c", 2).await;
    let sel = select(&app, ws, json!({})).await;

    let v2 = json!({
        "workspace_id": ws,
        "filter": {"q": null, "list_id": null, "list_status": null, "tag_id": null},
        "as_of": Uuid::now_v7(),
        "issued": 2,
    });
    let r = bulk(
        &app,
        ws,
        "delete",
        json!({"selection_token": hand_signed("nm:selection:v2", &v2), "confirmed_count": 2}),
    )
    .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem_type(&r), "urn:nm:problem:selection-invalid");
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);

    // The same payload under the new label is refused too: the label is not the only lock, the
    // shape is refused on its own.
    let r = bulk(
        &app,
        ws,
        "delete",
        json!({"selection_token": hand_signed("nm:selection:v3", &v2), "confirmed_count": 2}),
    )
    .await;
    assert_eq!(problem_type(&r), "urn:nm:problem:selection-invalid");

    // And this test's HMAC is the one the server computes: a v3 payload signed here is accepted.
    let v3 = json!({
        "workspace_id": ws,
        "snapshot_id": sel.snapshot_id,
        "issued": sel.selected,
    });
    let r = bulk(
        &app,
        ws,
        "delete",
        json!({"selection_token": hand_signed("nm:selection:v3", &v3), "confirmed_count": 2}),
    )
    .await;
    assert_eq!(affected(r).await, 2);
}

/// `base64url(json).base64url(hmac)` under a key derived from the test instance's secret and
/// `label`, exactly as `crypto::secrets::derive_key` and `selection::mac` do it.
fn hand_signed(label: &str, payload: &Value) -> String {
    let payload = serde_json::to_vec(payload).unwrap();
    let key = hmac_sha256(&[7u8; 32], label.as_bytes());
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(hmac_sha256(&key, &payload))
    )
}

/// `refuse_if_widened` is kept as defence in depth and must never fire (section 8.6), including
/// with exclusions that are not in the snapshot — the case M1a's `selected + excluded > issued`
/// refused, and the most ordinary action the grid produces, because the live view diverges from
/// the frozen set by every contact tagged or created since.
#[sqlx::test]
async fn refuse_if_widened_never_fires(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let churn = tag(&app, ws, "churn").await;
    let news = list(&app, ws, "news", false).await;
    let ids = contacts(&app, ws, "c", 4).await;
    apply_tag(&app, ws, churn, &ids[..2]).await;
    let sel = select(&app, ws, json!({"tag_id": churn})).await;
    assert_eq!(sel.selected, 2);

    // Everything M1a's arithmetic would have refused, at once: a contact tagged into the live
    // view after the selection, and exclusions the frozen set never held. The confirmation named
    // the two that are frozen, which is what a grid showing the frozen set shows.
    apply_tag(&app, ws, churn, &ids[2..3]).await;
    let outside = sel.excluding_confirming(&[ids[2], ids[3], Uuid::now_v7()], 2);
    let r = bulk(
        &app,
        ws,
        "subscribe",
        body(outside, json!({"list_id": news})),
    )
    .await;
    r.assert_status_ok();
    let v: Value = r.json();
    assert_eq!(
        (&v["issued"], &v["selected"], &v["affected"]),
        (&json!(2), &json!(2), &json!(2)),
        "an exclusion outside the snapshot refused a selection that cannot widen"
    );

    // The same on the irreversible one, with one exclusion in the snapshot and one outside: it
    // narrows, it does not refuse.
    let r = bulk(
        &app,
        ws,
        "delete",
        sel.excluding_confirming(&[ids[0], ids[3]], 1),
    )
    .await;
    r.assert_status_ok();
    let v: Value = r.json();
    assert_eq!((&v["issued"], &v["affected"]), (&json!(2), &json!(1)));
    assert_eq!(count(&pool, "select count(*) from contacts").await, 3);

    // Ten thousand ids that are not in the snapshot — `MAX_EXCLUDED` exactly — still do not.
    let sel = select(&app, ws, json!({})).await;
    let many: Vec<Uuid> = (0..MAX_EXCLUDED).map(|_| Uuid::now_v7()).collect();
    let r = bulk(&app, ws, "delete", sel.excluding_confirming(&many, 3)).await;
    assert_eq!(affected(r).await, 3);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 0);
}

/// Issue `03`: the number the person confirmed reaches the server, and an action that would reach
/// **more** contacts than that number is refused with nothing written. "You confirmed 5,000 and we
/// deleted 4,980" was open since M1a because closing it needed this design change.
#[sqlx::test]
async fn an_action_reaching_more_than_the_dialog_said_is_refused(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    contacts(&app, ws, "c", 3).await;
    let sel = select(&app, ws, json!({})).await;
    assert_eq!(sel.selected, 3);

    let mut target = sel.target();
    target["confirmed_count"] = json!(2);
    let r = bulk(&app, ws, "delete", target).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:selection-count-mismatch");
    let detail = r.json::<Value>()["detail"].as_str().unwrap().to_owned();
    assert!(
        detail.contains('2') && detail.contains('3'),
        "the refusal names both numbers: {detail}"
    );

    // Absent is refused too: a count a client may leave out is a count nothing checks.
    let mut target = sel.target();
    target.as_object_mut().unwrap().remove("confirmed_count");
    bulk(&app, ws, "delete", target)
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    assert_eq!(count(&pool, "select count(*) from contacts").await, 3);
    assert_eq!(
        count(
            &pool,
            "select count(*) from audit_log where action like 'contacts.bulk_%'"
        )
        .await,
        0,
        "a refused action wrote an audit row"
    );

    // **Fewer** than confirmed is not refused: a contact somebody else deleted in between is
    // simply absent, and the `issued`/`selected`/`affected` gap is how that is reported
    // (section 8.2). Refusing here would turn their delete into this person's error.
    let mut target = sel.target();
    target["confirmed_count"] = json!(9);
    let r = bulk(&app, ws, "delete", target).await;
    r.assert_status_ok();
    let v: Value = r.json();
    assert_eq!(
        (&v["issued"], &v["selected"], &v["affected"]),
        (&json!(3), &json!(3), &json!(3))
    );
    assert_eq!(count(&pool, "select count(*) from contacts").await, 0);

    // It only applies to a token: with `ids` there is no frozen set to compare against, and a
    // number nothing checks must not be accepted.
    let fresh = contacts(&app, ws, "d", 2).await;
    bulk(
        &app,
        ws,
        "delete",
        json!({"ids": fresh, "confirmed_count": 2}),
    )
    .await
    .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);
}

/// The case the check exists for, and the one the grid produces today: the live view has grown
/// since the selection, the person unticks a row that is **not** in the frozen set, and the
/// confirmation therefore says one less than the action would reach. Under a comparison against
/// `selection_snapshots.size` — the token's own signed `issued` — this passed, and a dialog
/// reading "Delete 1 contact" deleted two.
#[sqlx::test]
async fn unticking_a_row_outside_the_frozen_set_is_refused_not_quietly_widened(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let churn = tag(&app, ws, "churn").await;
    let ids = contacts(&app, ws, "c", 3).await;
    apply_tag(&app, ws, churn, &ids[..2]).await;
    let sel = select(&app, ws, json!({"tag_id": churn})).await;
    assert_eq!(sel.selected, 2, "the frozen set is c0 and c1");

    // A second editor tags c2 into the live view; the grid shows three and the person unticks the
    // newcomer, so the button reads "Delete 1 contact".
    apply_tag(&app, ws, churn, &ids[2..]).await;
    let r = bulk(&app, ws, "delete", sel.excluding(&[ids[2]])).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(problem_type(&r), "urn:nm:problem:selection-count-mismatch");
    let detail = r.json::<Value>()["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains('1') && detail.contains('2'), "{detail}");

    assert_eq!(count(&pool, "select count(*) from contacts").await, 3);
    assert_eq!(
        count(
            &pool,
            "select count(*) from audit_log where action like 'contacts.bulk_%'"
        )
        .await,
        0
    );

    // Unticking a row that *is* frozen is the ordinary case and proceeds: the numbers agree.
    let r = bulk(&app, ws, "delete", sel.excluding(&[ids[0]])).await;
    assert_eq!(affected(r).await, 1);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);
}

/// The audit row copies the predicate out of the snapshot, because the snapshot is deleted within
/// the hour and the log is not: without it the row reads "deleted N contacts, snapshot 0192…"
/// against a row nobody can look up. `q` is redacted in the copy, which the log outliving an
/// erasure requires.
#[sqlx::test]
async fn the_audit_row_carries_the_predicate_and_survives_the_snapshots_deletion(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let news = list(&app, ws, "news", false).await;
    let id = contact(&app, ws, "zebediah.quux@ex.test").await;
    apply_tag(&app, ws, vip, &[id]).await;
    join_list(&app, ws, id, news).await;

    let sel = select(
        &app,
        ws,
        json!({"q": "zebediah", "tag_id": vip, "list_id": news, "list_status": "subscribed"}),
    )
    .await;
    assert_eq!(sel.selected, 1);
    assert_eq!(
        affected(bulk(&app, ws, "delete", sel.target()).await).await,
        1
    );

    // The snapshot goes, as the reaper or the next selection would take it.
    sqlx::query("delete from selection_snapshots")
        .execute(&pool)
        .await
        .unwrap();

    let meta = audit_meta(&pool).await;
    assert_eq!(meta["filter"]["tag_id"], json!(vip));
    assert_eq!(meta["filter"]["list_id"], json!(news));
    assert_eq!(meta["filter"]["list_status"], json!("subscribed"));
    assert_eq!(meta["snapshot_id"], json!(sel.snapshot_id));
    assert_eq!(meta["by"], json!("filter"));
    assert_eq!(
        meta["filter"]["q"], "<redacted>",
        "a reader can still tell a search was used"
    );
    let text = meta.to_string();
    for needle in ["zebediah", "quux", "ex.test"] {
        assert!(!text.contains(needle), "{needle} leaked into {text}");
    }
}

#[sqlx::test]
async fn excluded_ids_are_not_affected(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let ids = contacts(&app, ws, "c", 4).await;
    let sel = select(&app, ws, json!({})).await;
    let target = sel.excluding(&[ids[0], ids[3]]);
    let n = affected(bulk(&app, ws, "tag", body(target, json!({"tag_id": vip}))).await).await;
    assert_eq!(n, 2);
    assert_eq!(tagged(&pool, vip).await, sorted(vec![ids[1], ids[2]]));
}

#[sqlx::test]
async fn token_from_another_workspace_is_404(pool: PgPool) {
    let (ws_a, app_a) = workspace(&pool, "acme").await;
    let (ws_b, app_b) = workspace(&pool, "other").await;
    contacts(&app_a, ws_a, "a", 2).await;
    let victim = contacts(&app_b, ws_b, "b", 2).await;
    let sel_b = select(&app_b, ws_b, json!({})).await;

    // An editor of A presents B's token against A: not found, and B's contacts are untouched.
    let r = bulk(&app_a, ws_a, "delete", sel_b.target()).await;
    r.assert_status_not_found();
    // And against B itself, where A's editor is no member.
    let r = bulk(&app_a, ws_b, "delete", sel_b.target()).await;
    r.assert_status_not_found();
    assert_eq!(count(&pool, "select count(*) from contacts").await, 4);
    assert_eq!(victim.len(), 2);
}

/// A snapshot id is not a capability: it is signed into the token, and a hand-written one for
/// another workspace's snapshot resolves to nothing even when the forger knows the id.
#[sqlx::test]
async fn another_workspaces_snapshot_id_reaches_nothing(pool: PgPool) {
    let (ws_a, app_a) = workspace(&pool, "acme").await;
    let (ws_b, app_b) = workspace(&pool, "other").await;
    contacts(&app_a, ws_a, "a", 1).await;
    contacts(&app_b, ws_b, "b", 2).await;
    let theirs = select(&app_b, ws_b, json!({})).await;

    // A's own token, re-signed to name B's snapshot: the members are scoped by workspace, so it
    // resolves to nothing rather than to B's contacts.
    let forged = hand_signed(
        "nm:selection:v3",
        &json!({"workspace_id": ws_a, "snapshot_id": theirs.snapshot_id, "issued": 2}),
    );
    let r = bulk(
        &app_a,
        ws_a,
        "delete",
        json!({"selection_token": forged, "confirmed_count": 2}),
    )
    .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem_type(&r), "urn:nm:problem:selection-expired");
    assert_eq!(count(&pool, "select count(*) from contacts").await, 3);
}

#[sqlx::test]
async fn a_tampered_token_is_rejected(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    contacts(&app, ws, "c", 2).await;
    let sel = select(&app, ws, json!({})).await;
    let (payload, sig) = sel.token.split_once('.').unwrap();
    let v: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    let keep_sig = |v: &Value| {
        format!(
            "{}.{sig}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
        )
    };

    // Point it at another snapshot, inflate the count it was issued for, and move it to another
    // workspace — each with the signature that belonged to the original.
    let mut other_snapshot = v.clone();
    other_snapshot["snapshot_id"] = json!(Uuid::now_v7());
    let mut inflated = v.clone();
    inflated["issued"] = json!(9);
    let mut moved = v.clone();
    moved["workspace_id"] = json!(Uuid::now_v7());
    // A signature from the right payload cut short.
    let truncated = format!("{payload}.{}", &sig[..sig.len() - 2]);
    for bad in [
        keep_sig(&other_snapshot),
        keep_sig(&inflated),
        keep_sig(&moved),
        truncated,
        "not-a-token".into(),
    ] {
        let r = bulk(
            &app,
            ws,
            "delete",
            json!({"selection_token": bad, "confirmed_count": 2}),
        )
        .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(problem_type(&r), "urn:nm:problem:selection-invalid");
    }
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);
}

#[sqlx::test]
async fn bulk_writes_exactly_one_audit_row(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let news = list(&app, ws, "news", false).await;
    let ids = contacts(&app, ws, "c", 5).await;
    for id in &ids {
        join_list(&app, ws, *id, news).await;
    }
    let sel = select(&app, ws, json!({"list_id": news})).await;
    let audit_before = count(&pool, "select count(*) from audit_log").await;

    let target = sel.excluding(&[ids[0]]);
    let n = affected(
        bulk(
            &app,
            ws,
            "unsubscribe",
            body(target, json!({"list_id": news})),
        )
        .await,
    )
    .await;
    assert_eq!(n, 4);

    // Exactly one row in all, not one per contact.
    assert_eq!(
        count(&pool, "select count(*) from audit_log").await,
        audit_before + 1
    );
    let (action, meta): (String, Value) = sqlx::query_as(
        "select action, metadata from audit_log where action like 'contacts.bulk_%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(action, "contacts.bulk_unsubscribed");
    assert_eq!(meta["affected"], 4);
    assert_eq!(meta["excluded"], 1);
    assert_eq!(meta["issued"], 5);
    assert_eq!(meta["filter"]["list_id"], json!(news));
    assert_eq!(meta["snapshot_id"], json!(sel.snapshot_id));
}

#[sqlx::test]
async fn bulk_is_editor_and_above(pool: PgPool) {
    let (ws, editor) = workspace(&pool, "acme").await;
    let viewer = client(&pool, "viewer@example.com", ws, Role::Viewer).await;
    let vip = tag(&editor, ws, "vip").await;
    let news = list(&editor, ws, "news", false).await;
    contacts(&editor, ws, "c", 2).await;
    let sel = select(&editor, ws, json!({})).await;

    // A viewer cannot even take a selection, and a token they were handed is no use to them.
    viewer
        .post(&format!("{}/contacts/selection", base(ws)))
        .json(&json!({}))
        .await
        .assert_status_forbidden();
    for (action, extra) in [
        ("tag", json!({"tag_id": vip})),
        ("untag", json!({"tag_id": vip})),
        ("subscribe", json!({"list_id": news})),
        ("unsubscribe", json!({"list_id": news})),
        ("delete", json!({})),
    ] {
        bulk(&viewer, ws, action, body(sel.target(), extra))
            .await
            .assert_status_forbidden();
    }
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);

    // An editor can, and delete writes no suppression: it is not erasure.
    let n = affected(bulk(&editor, ws, "delete", sel.target()).await).await;
    assert_eq!(n, 2);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 0);
    assert_eq!(count(&pool, "select count(*) from suppressions").await, 0);
}

#[sqlx::test]
async fn response_reports_the_count_actually_affected(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let double = list(&app, ws, "double", true).await;
    let ids = contacts(&app, ws, "c", 4).await;
    let all = select(&app, ws, json!({})).await;
    let tag_body = || body(all.target(), json!({"tag_id": vip}));

    // One is tagged already: three newly tagged, not four.
    apply_tag(&app, ws, vip, &ids[..1]).await;
    assert_eq!(affected(bulk(&app, ws, "tag", tag_body()).await).await, 3);
    // Doing it again changes nothing.
    assert_eq!(affected(bulk(&app, ws, "tag", tag_body()).await).await, 0);
    assert_eq!(affected(bulk(&app, ws, "untag", tag_body()).await).await, 4);
    assert_eq!(affected(bulk(&app, ws, "untag", tag_body()).await).await, 0);

    // Subscribing to a double-opt-in list writes `pending`, and a second go writes nothing.
    let sub = || body(all.target(), json!({"list_id": double}));
    assert_eq!(affected(bulk(&app, ws, "subscribe", sub()).await).await, 4);
    assert_eq!(affected(bulk(&app, ws, "subscribe", sub()).await).await, 0);
    let pending = count(
        &pool,
        "select count(*) from list_memberships where status = 'pending'",
    )
    .await;
    assert_eq!(pending, 4);

    // Unsubscribing one by hand first leaves three for the bulk action to do.
    app.post(&format!(
        "{}/contacts/{}/lists/{double}/unsubscribe",
        base(ws),
        ids[1]
    ))
    .await
    .assert_status_success();
    assert_eq!(
        affected(bulk(&app, ws, "unsubscribe", sub()).await).await,
        3
    );
    // The row stays, with the status: an unsubscribe is never a deleted row.
    let kept = count(
        &pool,
        "select count(*) from list_memberships where status = 'unsubscribed'",
    )
    .await;
    assert_eq!(kept, 4);
}

/// Every field of the filter narrows what is frozen, and so what the action reaches.
#[sqlx::test]
async fn all_four_filter_fields_apply(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let done = tag(&app, ws, "done").await;
    let news = list(&app, ws, "news", false).await;
    let ids = [
        contact(&app, ws, "match-a@ex.test").await,
        contact(&app, ws, "match-b@ex.test").await,
        contact(&app, ws, "other@ex.test").await,
    ];
    for id in &ids {
        join_list(&app, ws, *id, news).await;
    }
    // `match-b` is unsubscribed, so `list_status` takes it out; `other` fails the search.
    app.post(&format!(
        "{}/contacts/{}/lists/{news}/unsubscribe",
        base(ws),
        ids[1]
    ))
    .await
    .assert_status_success();
    apply_tag(&app, ws, vip, &ids).await;

    let sel = select(
        &app,
        ws,
        json!({"q": "match", "list_id": news, "list_status": "subscribed", "tag_id": vip}),
    )
    .await;
    assert_eq!(sel.selected, 1);
    let n =
        affected(bulk(&app, ws, "tag", body(sel.target(), json!({"tag_id": done}))).await).await;
    assert_eq!(n, 1);
    assert_eq!(tagged(&pool, done).await, vec![ids[0]]);

    // A `list_status` with no list, and a one-letter search, are refused at the door.
    for bad in [json!({"list_status": "subscribed"}), json!({"q": "ab"})] {
        app.post(&format!("{}/contacts/selection", base(ws)))
            .json(&bad)
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[sqlx::test]
async fn excluded_ids_are_bounded(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    contacts(&app, ws, "c", 2).await;
    let vip = tag(&app, ws, "vip").await;
    let sel = select(&app, ws, json!({})).await;
    let ids = |n: usize| (0..n).map(|_| Uuid::now_v7()).collect::<Vec<_>>();

    let over = sel.excluding_confirming(&ids(MAX_EXCLUDED + 1), 2);
    let r = bulk(&app, ws, "delete", over).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);

    // Exactly the cap passes the check and the action runs over the two that are frozen: none of
    // those ten thousand ids is in the snapshot, so none of them narrows it.
    let target = sel.excluding_confirming(&ids(MAX_EXCLUDED), 2);
    let r = bulk(&app, ws, "tag", body(target, json!({"tag_id": vip}))).await;
    assert_eq!(affected(r).await, 2);
}

#[sqlx::test]
async fn a_tag_or_list_of_another_workspace_is_404(pool: PgPool) {
    let (ws_a, app_a) = workspace(&pool, "acme").await;
    let (ws_b, app_b) = workspace(&pool, "other").await;
    let foreign_tag = tag(&app_b, ws_b, "theirs").await;
    let foreign_list = list(&app_b, ws_b, "theirs", false).await;
    contacts(&app_a, ws_a, "c", 2).await;
    let sel = select(&app_a, ws_a, json!({})).await;
    for (action, extra) in [
        ("tag", json!({"tag_id": foreign_tag})),
        ("untag", json!({"tag_id": foreign_tag})),
        ("subscribe", json!({"list_id": foreign_list})),
        ("unsubscribe", json!({"list_id": foreign_list})),
    ] {
        bulk(&app_a, ws_a, action, body(sel.target(), extra))
            .await
            .assert_status_not_found();
    }
    assert_eq!(
        count(
            &pool,
            "select count(*) from audit_log where action like 'contacts.bulk_%'"
        )
        .await,
        0
    );
}

/// Bulk subscribe adds the missing and touches nothing else: no promoting `pending`, and above
/// all no reviving `unsubscribed`. Freezing the set is only safe because of that — somebody who
/// unsubscribes between the selection and the action is still in the frozen set.
#[sqlx::test]
async fn bulk_subscribe_never_changes_an_existing_membership(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let single = list(&app, ws, "single", false).await;
    let double = list(&app, ws, "double", true).await;
    let ids = contacts(&app, ws, "c", 4).await; // [none, pending, subscribed, unsubscribed]

    for l in [single, double] {
        for i in [1, 2, 3] {
            join_list(&app, ws, ids[i], l).await;
        }
        set_status(&pool, ids[1], l, "pending").await;
        set_status(&pool, ids[2], l, "subscribed").await;
        set_status(&pool, ids[3], l, "unsubscribed").await;
    }
    let sel = select(&app, ws, json!({})).await;

    for (l, new_status) in [(single, "subscribed"), (double, "pending")] {
        let target = body(sel.target(), json!({"list_id": l}));
        // Only the contact with no row is added: the count says so.
        assert_eq!(affected(bulk(&app, ws, "subscribe", target).await).await, 1);
        let mut got = vec![];
        for id in &ids {
            got.push(status_of(&pool, *id, l).await.unwrap());
        }
        assert_eq!(got, [new_status, "pending", "subscribed", "unsubscribed"]);
    }
}

/// Bulk subscribe records `bulk`, and leaves the provenance of a membership that already exists
/// alone: `on conflict do nothing` must not become a rewrite of how somebody first came in.
#[sqlx::test]
async fn bulk_subscribe_records_bulk_and_keeps_an_existing_source(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let news = list(&app, ws, "news", false).await;
    let ids = contacts(&app, ws, "c", 2).await;
    join_list(&app, ws, ids[1], news).await; // by the UI route: `ui`
    let sel = select(&app, ws, json!({})).await;
    let target = body(sel.target(), json!({"list_id": news}));
    assert_eq!(affected(bulk(&app, ws, "subscribe", target).await).await, 1);

    let mut got = vec![];
    for id in &ids {
        got.push(
            sqlx::query_scalar::<_, Option<String>>(
                "select source from list_memberships where contact_id = $1 and list_id = $2",
            )
            .bind(id)
            .bind(news)
            .fetch_one(&pool)
            .await
            .unwrap(),
        );
    }
    assert_eq!(got, [Some("bulk".to_owned()), Some("ui".to_owned())]);
}

/// Somebody who unsubscribes between the selection and the action, which is section 8.3's case:
/// they are still frozen, the statement runs against them, and `on conflict do nothing` is the
/// whole reason that is harmless.
#[sqlx::test]
async fn a_contact_who_unsubscribes_after_the_selection_is_not_resubscribed(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let news = list(&app, ws, "news", false).await;
    let ids = contacts(&app, ws, "c", 2).await;
    join_list(&app, ws, ids[0], news).await;
    let sel = select(&app, ws, json!({})).await;

    app.post(&format!(
        "{}/contacts/{}/lists/{news}/unsubscribe",
        base(ws),
        ids[0]
    ))
    .await
    .assert_status_success();
    assert_eq!(
        status_of(&pool, ids[0], news).await.unwrap(),
        "unsubscribed"
    );

    let target = body(sel.target(), json!({"list_id": news}));
    assert_eq!(affected(bulk(&app, ws, "subscribe", target).await).await, 1);
    assert_eq!(
        status_of(&pool, ids[0], news).await.unwrap(),
        "unsubscribed",
        "the opt-out was reversed in bulk"
    );
    assert_eq!(status_of(&pool, ids[1], news).await.unwrap(), "subscribed");
}

/// `affected` alone cannot be read: the caller needs `selected` to say "N of M", and on a
/// double-opt-in list needs `status` to know the people are not yet mailable.
#[sqlx::test]
async fn response_carries_selected_and_the_resulting_status(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let single = list(&app, ws, "single", false).await;
    let double = list(&app, ws, "double", true).await;
    let ids = contacts(&app, ws, "c", 6).await;
    let sel = select(&app, ws, json!({})).await;
    contacts(&app, ws, "late", 3).await; // after the selection: in neither number
    apply_tag(&app, ws, vip, &ids[..4]).await;

    let r = bulk(&app, ws, "tag", body(sel.target(), json!({"tag_id": vip}))).await;
    r.assert_status_ok();
    let v: Value = r.json();
    assert_eq!((&v["selected"], &v["affected"]), (&json!(6), &json!(2)));
    assert!(v.get("status").is_none(), "only subscribe has a status");

    for (l, status) in [(double, "pending"), (single, "subscribed")] {
        let target = body(sel.excluding(&[ids[0]]), json!({"list_id": l}));
        let v: Value = bulk(&app, ws, "subscribe", target.clone()).await.json();
        assert_eq!((&v["selected"], &v["affected"]), (&json!(5), &json!(5)));
        assert_eq!(v["status"], status);
        // Again: all five selected, none of them changed.
        let v: Value = bulk(&app, ws, "subscribe", target).await.json();
        assert_eq!((&v["selected"], &v["affected"]), (&json!(5), &json!(0)));
        assert_eq!(v["status"], status);
    }

    let v: Value = bulk(&app, ws, "delete", sel.target()).await.json();
    assert_eq!((&v["selected"], &v["affected"]), (&json!(6), &json!(6)));
    let meta: Value =
        sqlx::query_scalar("select metadata from audit_log where action = 'contacts.bulk_deleted'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(meta["selected"], 6);
}

/// Ticked rows, no token. The same skip-unsubscribed rule as a frozen selection, and the same two
/// numbers: three of four were added, and the one who unsubscribed was not brought back.
#[sqlx::test]
async fn an_id_list_subscribes_the_new_and_skips_an_unsubscribed_contact(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (_, other) = workspace(&pool, "other").await;
    let single = list(&app, ws, "single", false).await;
    let ids = contacts(&app, ws, "c", 4).await;
    join_list(&app, ws, ids[3], single).await;
    set_status(&pool, ids[3], single, "unsubscribed").await;
    let outsider = Uuid::now_v7(); // not a contact of this workspace at all
    drop(other);

    let mut named = ids.clone();
    named.push(outsider);
    let r = bulk(
        &app,
        ws,
        "subscribe",
        json!({"ids": named, "list_id": single}),
    )
    .await;
    r.assert_status_ok();
    let v: Value = r.json();
    assert_eq!((&v["selected"], &v["affected"]), (&json!(4), &json!(3)));
    assert_eq!(v["status"], "subscribed");
    assert_eq!(
        status_of(&pool, ids[3], single).await.unwrap(),
        "unsubscribed"
    );
    for id in &ids[..3] {
        assert_eq!(status_of(&pool, *id, single).await.unwrap(), "subscribed");
    }
}

/// An id of another workspace's contact selects nothing: the workspace scope is in the statement.
#[sqlx::test]
async fn an_id_list_cannot_reach_another_workspaces_contacts(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (ws2, app2) = workspace(&pool, "other").await;
    let theirs = contacts(&app2, ws2, "t", 2).await;
    let vip = tag(&app, ws, "vip").await;
    let r = bulk(&app, ws, "tag", json!({"ids": theirs, "tag_id": vip})).await;
    let v: Value = r.json();
    assert_eq!((&v["selected"], &v["affected"]), (&json!(0), &json!(0)));
    assert!(tagged(&pool, vip).await.is_empty());
}

#[sqlx::test]
async fn exactly_one_of_ids_and_a_token(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let ids = contacts(&app, ws, "c", 2).await;
    let sel = select(&app, ws, json!({})).await;
    for target in [
        body(sel.target(), json!({"ids": ids})),
        json!({}),
        json!({"ids": ids, "excluded_ids": [ids[0]]}),
    ] {
        let r = bulk(&app, ws, "tag", body(target, json!({"tag_id": vip}))).await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
    assert!(tagged(&pool, vip).await.is_empty());
}

#[sqlx::test]
async fn ids_are_bounded(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    contacts(&app, ws, "c", 2).await;
    let ids = |n: usize| (0..n).map(|_| Uuid::now_v7()).collect::<Vec<_>>();
    let r = bulk(
        &app,
        ws,
        "tag",
        json!({"ids": ids(MAX_IDS + 1), "tag_id": vip}),
    )
    .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    // Exactly the cap is accepted (and matches nothing here).
    let r = bulk(&app, ws, "tag", json!({"ids": ids(MAX_IDS), "tag_id": vip})).await;
    assert_eq!(affected(r).await, 0);
}

/// One row, saying it was an id list and how many, with none of the ids in it and no snapshot it
/// never had.
#[sqlx::test]
async fn an_id_list_writes_one_audit_row_without_the_ids(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let ids = contacts(&app, ws, "c", 3).await;
    let before = count(&pool, "select count(*) from audit_log").await;
    assert_eq!(
        affected(bulk(&app, ws, "tag", json!({"ids": ids, "tag_id": vip})).await).await,
        3
    );
    assert_eq!(
        count(&pool, "select count(*) from audit_log").await,
        before + 1
    );
    let meta = audit_meta(&pool).await;
    let text = meta.to_string();
    for id in &ids {
        assert!(!text.contains(&id.to_string()), "{id} leaked into {text}");
    }
    assert_eq!(
        (&meta["by"], &meta["ids"], &meta["selected"]),
        (&json!("ids"), &json!(3), &json!(3))
    );
    assert_eq!(meta["snapshot_id"], Value::Null);
    assert_eq!(meta["filter"], Value::Null);
}

/// `selected` is the size of the set that was frozen, so a later signup is not in it.
#[sqlx::test]
async fn creating_a_selection_reports_how_many_it_matched(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    contacts(&app, ws, "match", 3).await;
    contacts(&app, ws, "other", 2).await;
    assert_eq!(select(&app, ws, json!({"q": "match"})).await.selected, 3);
    assert_eq!(select(&app, ws, json!({})).await.selected, 5);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 5);
}

/// The irreversible one. Workspace A names workspace B's contacts to `bulk/delete`: they must
/// survive, and neither number may reveal that those ids exist somewhere else. The workspace is
/// scoped in both the `sel` CTE and the delete statement, so a one-layer regression would not
/// redden this; removing both does.
#[sqlx::test]
async fn an_id_list_cannot_delete_another_workspaces_contacts(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (ws2, app2) = workspace(&pool, "other").await;
    let mine = contacts(&app, ws, "m", 1).await;
    let theirs = contacts(&app2, ws2, "t", 3).await;

    let r = bulk(&app, ws, "delete", json!({"ids": theirs})).await;
    r.assert_status_ok();
    let v: Value = r.json();
    assert_eq!((&v["selected"], &v["affected"]), (&json!(0), &json!(0)));
    assert_eq!(count(&pool, "select count(*) from contacts").await, 4);

    // The same request with one of its own is scoped, not rejected.
    let r = bulk(&app, ws, "delete", json!({"ids": [theirs[0], mine[0]]})).await;
    let v: Value = r.json();
    assert_eq!((&v["selected"], &v["affected"]), (&json!(1), &json!(1)));
    assert_eq!(count(&pool, "select count(*) from contacts").await, 3);
    assert_eq!(count_in(&pool, ws2).await, 3);
}

/// An empty list is not a selection: 422, and no audit row for an action that did nothing.
#[sqlx::test]
async fn an_empty_id_list_is_422_and_writes_no_audit_row(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let before = count(&pool, "select count(*) from audit_log").await;
    let r = bulk(&app, ws, "tag", json!({"ids": [], "tag_id": vip})).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count(&pool, "select count(*) from audit_log").await, before);
}

async fn count_in(pool: &PgPool, ws: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from contacts where workspace_id = $1")
        .bind(ws)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// An unknown key in a filter must be a refusal. Ignored, `{"tag_id_": ...}` is a filter with
/// every field empty, which matches every contact in the workspace; handed to `bulk/delete` that
/// deletes them all.
#[sqlx::test]
async fn a_selection_filter_with_an_unknown_field_is_422_not_everything(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    contacts(&app, ws, "c", 3).await;
    for bad in [
        json!({"tag_id_": vip}),
        json!({"tag": vip}),
        json!({"tag_id": vip, "extra": 1}),
    ] {
        let r = app
            .post(&format!("{}/contacts/selection", base(ws)))
            .json(&bad)
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert!(r.json::<Value>().get("selection_token").is_none());
    }
    assert_eq!(count(&pool, "select count(*) from contacts").await, 3);
}

/// The typo that would have run the action on exactly the people the user spared: with
/// `excluded_ids` defaulting to empty, an ignored `excluded_id` deletes the excluded contact.
#[sqlx::test]
async fn a_misspelt_excluded_ids_is_422_and_the_spared_contact_survives(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let ids = contacts(&app, ws, "c", 3).await;
    let sel = select(&app, ws, json!({})).await;

    let target = body(sel.target(), json!({"excluded_id": [ids[0]]}));
    let r = bulk(&app, ws, "delete", target).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 3);
    let spared: i64 = sqlx::query_scalar("select count(*) from contacts where id = $1")
        .bind(ids[0])
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(spared, 1, "the contact the user excluded must still exist");
    assert_eq!(
        count(
            &pool,
            "select count(*) from audit_log where action like 'contacts.bulk_%'"
        )
        .await,
        0
    );
}

/// Each of the three bulk body shapes refuses an unknown field, on every route that takes it.
#[sqlx::test]
async fn every_bulk_body_refuses_an_unknown_field(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let news = list(&app, ws, "news", false).await;
    contacts(&app, ws, "c", 2).await;
    let sel = select(&app, ws, json!({})).await;
    for (action, extra) in [
        ("delete", json!({"bogus": 1})),                       // BulkTarget
        ("tag", json!({"tag_id": vip, "bogus": 1})),           // BulkTag
        ("untag", json!({"tag_id": vip, "bogus": 1})),         // BulkTag
        ("subscribe", json!({"list_id": news, "bogus": 1})),   // BulkList
        ("unsubscribe", json!({"list_id": news, "bogus": 1})), // BulkList
    ] {
        bulk(&app, ws, action, body(sel.target(), extra))
            .await
            .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    }
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);
    assert_eq!(count(&pool, "select count(*) from contact_tags").await, 0);
    assert_eq!(
        count(&pool, "select count(*) from list_memberships").await,
        0
    );
}

/// `excluded_ids` present, empty or absent all work, on a flat body.
#[sqlx::test]
async fn valid_bodies_are_accepted_with_or_without_excluded_ids(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let ids = contacts(&app, ws, "c", 3).await;
    let sel = select(&app, ws, json!({})).await;
    let with = body(sel.excluding(&[ids[0]]), json!({"tag_id": vip}));
    assert_eq!(affected(bulk(&app, ws, "tag", with).await).await, 2);
    let empty = body(sel.excluding(&[]), json!({"tag_id": vip}));
    assert_eq!(affected(bulk(&app, ws, "tag", empty).await).await, 1);
    let absent = body(sel.target(), json!({"tag_id": vip}));
    assert_eq!(affected(bulk(&app, ws, "untag", absent).await).await, 3);
    assert_eq!(
        affected(bulk(&app, ws, "delete", sel.target()).await).await,
        3
    );
}

/// An id list names its contacts, so it has no predicate to re-run and can only shrink. The
/// widening check must never fire on one.
#[sqlx::test]
async fn an_id_list_delete_is_never_refused_as_widened(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let ids = contacts(&app, ws, "c", 3).await;
    let target = json!({"ids": [ids[0], ids[1]]});
    assert_eq!(affected(bulk(&app, ws, "delete", target).await).await, 2);
    assert_eq!(count(&pool, "select count(*) from contacts").await, 1);
}

/// A token for a selection over a **segment** now acts, and acts on exactly the frozen set. Under
/// the v2 payload it could not: a `ContactFilter` cannot express a segment's rules, so a token
/// issued for one described every contact the remaining filter matched — here, the whole
/// workspace.
#[sqlx::test]
async fn a_segment_selection_acts_on_exactly_the_frozen_set(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let vip = tag(&app, ws, "vip").await;
    let ids = contacts(&app, ws, "c", 3).await;
    apply_tag(&app, ws, vip, &ids[..1]).await;
    let seg = segment(
        &app,
        ws,
        "tagged",
        json!({"match": "all", "rules": [{"field": "tag", "op": "has", "value": vip}]}),
    )
    .await;

    let sel = select(&app, ws, json!({"segment_id": seg})).await;
    assert_eq!(sel.selected, 1);
    let r = bulk(&app, ws, "delete", sel.target()).await;
    r.assert_status_ok();
    assert_eq!(count(&pool, "select count(*) from contacts").await, 2);
    let left: Vec<Uuid> = sqlx::query_scalar("select id from contacts order by id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(left, sorted(ids[1..].to_vec()));
}

/// "Here is what you selected" (section 14): the grid reads the frozen set through
/// `?snapshot_id=`, which is what removes the divergence between the live view and the snapshot
/// at source.
#[sqlx::test]
async fn the_grid_shows_the_frozen_set(pool: PgPool) {
    let (ws, app) = workspace(&pool, "acme").await;
    let (other_ws, other) = workspace(&pool, "globex").await;
    let churn = tag(&app, ws, "churn").await;
    let ids = contacts(&app, ws, "c", 4).await;
    apply_tag(&app, ws, churn, &ids[..2]).await;
    let sel = select(&app, ws, json!({"tag_id": churn})).await;
    // The live filter moves on; the frozen set does not.
    apply_tag(&app, ws, churn, &ids[2..3]).await;

    assert_eq!(
        page_ids(&app, ws, &format!("tag_id={churn}")).await,
        sorted(ids[..3].to_vec()),
        "the live filter"
    );
    assert_eq!(
        page_ids(&app, ws, &format!("snapshot_id={}", sel.snapshot_id)).await,
        sorted(ids[..2].to_vec()),
        "the frozen set"
    );
    // An id that names no live snapshot of this workspace is refused, not answered with an empty
    // page: "zero contacts matched" is a different fact from "this selection is gone", and the
    // grid cannot say "select again" if the server will not say which. It is certainly never the
    // whole workspace, which is what ignoring the parameter would show.
    let theirs = select(&other, other_ws, json!({})).await;
    for gone in [Uuid::now_v7(), Uuid::nil(), theirs.snapshot_id] {
        let r = app
            .get(&format!("{}/contacts?snapshot_id={gone}", base(ws)))
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(problem_type(&r), "urn:nm:problem:selection-expired");
        assert!(r.json::<Value>().get("data").is_none(), "{gone}");
    }
    // A snapshot narrows alongside the ordinary filters rather than replacing them.
    assert_eq!(
        page_ids(
            &app,
            ws,
            &format!("snapshot_id={}&q=c1@ex", sel.snapshot_id)
        )
        .await,
        vec![ids[1]]
    );

    // An expired snapshot, still unswept, is refused here too, and by its `expires_at` rather
    // than by the row being gone.
    sqlx::query(
        "update selection_snapshots set expires_at = now() - interval '1 minute' where id = $1",
    )
    .bind(sel.snapshot_id)
    .execute(&pool)
    .await
    .unwrap();
    let r = app
        .get(&format!(
            "{}/contacts?snapshot_id={}",
            base(ws),
            sel.snapshot_id
        ))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem_type(&r), "urn:nm:problem:selection-expired");
}

/// The ids on one page of `GET /contacts`, sorted.
async fn page_ids(app: &TestApp, ws: Uuid, query: &str) -> Vec<Uuid> {
    let r = app.get(&format!("{}/contacts?{query}", base(ws))).await;
    r.assert_status_ok();
    let v: Value = r.json();
    sorted(
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap().parse::<Uuid>().unwrap())
            .collect(),
    )
}
