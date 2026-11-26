// SPDX-License-Identifier: AGPL-3.0-only

//! The launch (`nm::campaign::launch`, spec M2c §4.2, §4.3, §4.9): who is mailed, and the freeze.
//!
//! Every audience test seeds one contact of each kind (suppressed, unsubscribed, unconfirmed, on two
//! lists, outside the segment) and asserts exactly who is queued, never a count alone: a count
//! passes when the wrong people are swapped for the right number of others.

mod common;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use common::{PW, TestApp, add_member, create_user, create_workspace, test_config};
use nm::{
    campaign::{
        self,
        launch::{LAUNCH_BATCH, LAUNCH_KIND, Resolution, resolve_batch},
    },
    crypto::{secrets::Secrets, suppression_hmac},
    relay::transport::Transports,
    worker::{self, Worker, confirmation::Ctx},
    workspaces::roles::Role,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const SLICE: Duration = Duration::from_secs(60);

struct Fx {
    pool: PgPool,
    app: TestApp,
    ws: Uuid,
    domain: Uuid,
    template: Uuid,
    list: Uuid,
    ctx: Ctx,
}

fn id_of(r: &axum_test::TestResponse) -> Uuid {
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

async fn fx(pool: &PgPool) -> Fx {
    let slug = "acme";
    let owner = create_user(pool, "owner@example.com", PW, false).await;
    let ws = create_workspace(pool, owner, slug).await;
    let user = create_user(pool, "admin@example.com", PW, false).await;
    add_member(pool, ws, user, Role::Admin).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login("admin@example.com", PW).await;

    app.put(&format!("/api/v1/workspaces/{ws}/brand-kit"))
        .json(&json!({"logo_url": null, "palette": {}, "font_stack": null,
                      "footer": {"address": "1 Main St, Springfield"}}))
        .await
        .assert_status_ok();
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/sending-domains"))
        .json(&json!({"domain": "mail.example.com", "signing": "relay"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let domain = id_of(&r);
    sqlx::query("update sending_domains set checks = $2 where id = $1")
        .bind(domain)
        .bind(
            json!({"spf": {"status": "pass"}, "dkim": {"status": "pass"},
                     "dmarc": {"status": "pass"}, "return_path": {"status": "pass"}}),
        )
        .execute(pool)
        .await
        .expect("checks");
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/templates"))
        .json(
            &json!({"name": "welcome", "kind": "blocks", "tree": {"blocks": [
            {"type": "Text", "runs": [{"type": "Tag", "source": "{{ first_name }}"}]}]}}),
        )
        .await;
    r.assert_status(StatusCode::CREATED);
    let template = id_of(&r);
    let list = new_list(&app, ws, "news").await;
    let config = test_config("postgres://unused");
    let ctx = Ctx {
        db: pool.clone(),
        secrets: Arc::new(Secrets::new(&config.secret_key)),
        config: Arc::new(config),
        transports: Arc::new(Transports::new()),
    };
    Fx {
        pool: pool.clone(),
        app,
        ws,
        domain,
        template,
        list,
        ctx,
    }
}

async fn new_list(app: &TestApp, ws: Uuid, name: &str) -> Uuid {
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/lists"))
        .json(&json!({"name": name, "double_optin": false}))
        .await;
    r.assert_status(StatusCode::CREATED);
    id_of(&r)
}

impl Fx {
    /// A campaign through the API, on `lists`, with `extra` merged over a complete body.
    async fn campaign(&self, lists: &[Uuid], extra: Value) -> Uuid {
        let mut body = json!({
            "name": "c", "list_ids": lists, "sending_domain_id": self.domain,
            "from_local_part": "news", "from_name": "Acme", "reply_to": "hello@example.com",
            "variants": [{"subject": "Hi {{ first_name }}", "template_id": self.template,
                          "from_name": null}],
        });
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let r = self
            .app
            .post(&format!("/api/v1/workspaces/{}/campaigns", self.ws))
            .json(&body)
            .await;
        r.assert_status(StatusCode::CREATED);
        id_of(&r)
    }

    /// Two variants at `percent`.
    async fn ab(&self, percent: i64) -> Uuid {
        let r = self
            .app
            .post(&format!("/api/v1/workspaces/{}/templates", self.ws))
            .json(
                &json!({"name": "second", "kind": "blocks", "tree": {"blocks": [
                {"type": "Text", "runs": [{"type": "Tag", "source": "{{ last_name }}"}]}]}}),
            )
            .await;
        r.assert_status(StatusCode::CREATED);
        let second = id_of(&r);
        self.campaign(
            &[self.list],
            json!({"variants": [
                {"subject": "One", "template_id": self.template, "from_name": null},
                {"subject": "Two", "template_id": second, "from_name": null}],
                "ab_test_percent": percent}),
        )
        .await
    }

    /// What `schedule` leaves, send now, written directly so a fixed date is usable.
    async fn schedule(&self, id: Uuid) -> Uuid {
        let job = Uuid::now_v7();
        sqlx::query("update campaigns set state = 'scheduled', launch_job_id = $2 where id = $1")
            .bind(id)
            .bind(job)
            .execute(&self.pool)
            .await
            .expect("schedule");
        self.launch_job(id, job).await;
        job
    }

    async fn launch_job(&self, id: Uuid, job: Uuid) {
        sqlx::query(
            "insert into jobs (id, workspace_id, kind, campaign_id, payload, run_after)
             values ($1, $2, 'campaign_launch', $3, '{}', now())",
        )
        .bind(job)
        .bind(self.ws)
        .bind(id)
        .execute(&self.pool)
        .await
        .expect("launch job");
    }

    /// Claims and handles launch jobs until none is due. Returns how many claims ran.
    async fn drain(&self, slice: Duration) -> usize {
        let mut handled = 0;
        while let Some(job) = worker::claim(&self.pool, LAUNCH_KIND, 1, 120)
            .await
            .expect("claim")
            .pop()
        {
            nm::campaign::launch::handle(&self.ctx, &job, slice).await;
            handled += 1;
            assert!(handled < 20, "a launch that never ends");
        }
        handled
    }

    /// `n` subscribed contacts on `list`, named `first`; returns their ids.
    async fn contacts(&self, list: Uuid, n: i32, first: &str) -> Vec<Uuid> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "insert into contacts (id, workspace_id, email, first_name)
             select gen_random_uuid(), $1, $2 || '-' || g || '@example.com', $2
               from generate_series(1, $3) g returning id",
        )
        .bind(self.ws)
        .bind(first)
        .bind(n)
        .fetch_all(&self.pool)
        .await
        .expect("contacts");
        sqlx::query(
            "insert into list_memberships (workspace_id, contact_id, list_id, status)
             select $1, c, $2, 'subscribed' from unnest($3::uuid[]) as c",
        )
        .bind(self.ws)
        .bind(list)
        .bind(&ids)
        .execute(&self.pool)
        .await
        .expect("memberships");
        ids
    }

    async fn contact(&self, email: &str, first: &str) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "insert into contacts (id, workspace_id, email, first_name) values ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(self.ws)
        .bind(email)
        .bind(first)
        .execute(&self.pool)
        .await
        .expect("contact");
        id
    }

    async fn member(&self, contact: Uuid, list: Uuid, status: &str) {
        sqlx::query(
            "insert into list_memberships (workspace_id, contact_id, list_id, status)
             values ($1, $2, $3, $4)",
        )
        .bind(self.ws)
        .bind(contact)
        .bind(list)
        .bind(status)
        .execute(&self.pool)
        .await
        .expect("membership");
    }

    async fn suppress_address(&self, email: &str) {
        sqlx::query(
            "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason)
             values ($1, $2, 'address', $3, $4, 'manual')",
        )
        .bind(Uuid::now_v7())
        .bind(self.ws)
        .bind(email)
        .bind(suppression_hmac(&self.ctx.secrets, email).as_slice())
        .execute(&self.pool)
        .await
        .expect("suppression");
    }

    async fn state(&self, id: Uuid) -> String {
        sqlx::query_scalar("select state from campaigns where id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// The addresses queued for the campaign.
    async fn queued(&self, id: Uuid) -> BTreeSet<String> {
        sqlx::query_scalar(
            "select k.email from messages m join contacts k on k.id = m.contact_id
              where m.campaign_id = $1",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .unwrap()
        .into_iter()
        .collect()
    }

    async fn messages(&self, id: Uuid) -> i64 {
        sqlx::query_scalar("select count(*) from messages where campaign_id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// `(contact, variant)` for every message.
    async fn variants(&self, id: Uuid) -> BTreeMap<Uuid, i16> {
        sqlx::query_as::<_, (Uuid, i16)>(
            "select contact_id, variant from messages where campaign_id = $1",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .unwrap()
        .into_iter()
        .collect()
    }

    async fn total(&self, id: Uuid) -> i64 {
        sqlx::query_scalar("select total from campaigns where id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

fn set<const N: usize>(items: [&str; N]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[sqlx::test]
async fn a_launch_freezes_every_variant_and_moves_to_sending(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 40, "ada").await;
    let id = f.ab(100).await;
    f.schedule(id).await;
    assert_eq!(f.drain(SLICE).await, 1);

    assert_eq!(f.state(id).await, "sending");
    let c = campaign::load(&pool, f.ws, id).await.unwrap();
    assert!(c.frozen_at.is_some() && c.launched_at.is_some() && c.resolved);
    let frozen = campaign::load_frozen(&pool, f.ws, id)
        .await
        .unwrap()
        .expect("frozen is set")
        .expect("and reads back through Frozen::from_column");
    assert_eq!(frozen.variants.len(), 2, "every variant is frozen");
    assert_eq!(frozen.from, "news@mail.example.com");
    // A pure split (100%) queues everyone, spread over both variants.
    assert_eq!(f.messages(id).await, 40);
    let used: BTreeSet<i16> = f.variants(id).await.into_values().collect();
    assert_eq!(used, BTreeSet::from([0, 1]));
    // The job that launched it is finished.
    let open: i64 = sqlx::query_scalar(
        "select count(*) from jobs where kind = 'campaign_launch' and completed_at is null",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(open, 0);
}

#[sqlx::test]
async fn the_audience_is_mailable_members_narrowed_by_the_segment_each_once(pool: PgPool) {
    let f = fx(&pool).await;
    let second = new_list(&f.app, f.ws, "second").await;
    let other = new_list(&f.app, f.ws, "other").await;
    // In the segment ("Ada"), and mailable:
    let a = f.contact("a@x.test", "Ada").await;
    f.member(a, f.list, "subscribed").await;
    let b = f.contact("b@x.test", "Ada").await; // on both campaign lists: still one recipient
    f.member(b, f.list, "subscribed").await;
    f.member(b, second, "subscribed").await;
    let i = f.contact("i@x.test", "Ada").await; // only on the second list
    f.member(i, second, "subscribed").await;
    // Not in the audience:
    let outside = f.contact("outside@x.test", "Bob").await; // mailable, but outside the segment
    f.member(outside, f.list, "subscribed").await;
    let elsewhere = f.contact("elsewhere@x.test", "Ada").await; // in the segment, on another list
    f.member(elsewhere, other, "subscribed").await;
    let nowhere = f.contact("nowhere@x.test", "Ada").await; // on no list at all
    let _ = nowhere;

    let r = f
        .app
        .post(&format!("/api/v1/workspaces/{}/segments", f.ws))
        .json(&json!({"name": "adas", "rules": {"match": "all", "rules": [
            {"field": "first_name", "op": "eq", "value": "Ada"}]}}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let segment = id_of(&r);
    let id = f
        .campaign(&[f.list, second], json!({"segment_id": segment}))
        .await;
    f.schedule(id).await;
    f.drain(SLICE).await;

    assert_eq!(
        f.queued(id).await,
        set(["a@x.test", "b@x.test", "i@x.test"])
    );
    assert_eq!(
        f.messages(id).await,
        3,
        "b is queued once, not once per list"
    );
    assert_eq!(f.total(id).await, 3);

    // Without a segment, the same lists: the segment is what dropped Bob.
    let wide = f.campaign(&[f.list, second], json!({})).await;
    f.schedule(wide).await;
    f.drain(SLICE).await;
    assert_eq!(
        f.queued(wide).await,
        set(["a@x.test", "b@x.test", "i@x.test", "outside@x.test"])
    );
}

#[sqlx::test]
async fn unsubscribed_unconfirmed_and_suppressed_members_are_not_resolved(pool: PgPool) {
    let f = fx(&pool).await;
    let ok = f.contact("ok@x.test", "Ada").await;
    f.member(ok, f.list, "subscribed").await;
    let unsubscribed = f.contact("unsubscribed@x.test", "Ada").await;
    f.member(unsubscribed, f.list, "unsubscribed").await;
    let pending = f.contact("pending@x.test", "Ada").await; // double opt-in, never confirmed
    f.member(pending, f.list, "pending").await;
    let suppressed = f.contact("suppressed@x.test", "Ada").await;
    f.member(suppressed, f.list, "subscribed").await;
    f.suppress_address("suppressed@x.test").await;
    let domain = f.contact("someone@blocked.test", "Ada").await;
    f.member(domain, f.list, "subscribed").await;
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, domain, reason)
         values ($1, $2, 'domain', 'blocked.test', 'manual')",
    )
    .bind(Uuid::now_v7())
    .bind(f.ws)
    .execute(&pool)
    .await
    .unwrap();
    // Suppressed on this list only, by its own unsubscribe (scope `list`).
    let listed = f.contact("listed@x.test", "Ada").await;
    f.member(listed, f.list, "subscribed").await;
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason, list_id, \
         list_name) values ($1, $2, 'list', 'listed@x.test', $3, 'unsubscribed_list', $4, 'news')",
    )
    .bind(Uuid::now_v7())
    .bind(f.ws)
    .bind(suppression_hmac(&f.ctx.secrets, "listed@x.test").as_slice())
    .bind(f.list)
    .execute(&pool)
    .await
    .unwrap();

    let id = f.campaign(&[f.list], json!({})).await;
    f.schedule(id).await;
    f.drain(SLICE).await;
    assert_eq!(f.queued(id).await, set(["ok@x.test"]));
    assert_eq!(f.total(id).await, 1);
}

#[sqlx::test]
async fn a_slice_that_runs_out_of_time_hands_off_and_resumes_without_duplicates(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 25_000, "ada").await;
    let id = f.campaign(&[f.list], json!({})).await;
    let first = f.schedule(id).await;

    // Slice zero: one batch, then a hand-off, every time.
    let claimed = worker::claim(&pool, LAUNCH_KIND, 1, 120)
        .await
        .unwrap()
        .remove(0);
    nm::campaign::launch::handle(&f.ctx, &claimed, Duration::ZERO).await;
    assert_eq!(f.messages(id).await, LAUNCH_BATCH, "one batch");
    let c = campaign::load(&pool, f.ws, id).await.unwrap();
    assert!(!c.resolved);
    let second = c
        .launch_job_id
        .expect("the campaign points at its successor");
    assert_ne!(second, first, "a new job took over");
    let done: Option<DateTime<Utc>> =
        sqlx::query_scalar("select completed_at from jobs where id = $1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(done.is_some(), "the first job completed in the hand-off");

    // The second claim crashes after committing its batch, before it can hand off or complete.
    let claimed = worker::claim(&pool, LAUNCH_KIND, 1, 120)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(claimed.id, second);
    let r = Resolution::load(&pool, &c, second)
        .await
        .expect("resolution");
    let batch = resolve_batch(&pool, &r, c.resolve_cursor).await.unwrap();
    assert_eq!((batch.scanned, batch.inserted), (10_000, 10_000));
    assert_eq!(f.messages(id).await, 20_000);
    sqlx::query("update jobs set run_after = now() where id = $1")
        .bind(second)
        .execute(&pool)
        .await
        .unwrap(); // the lease ran out

    // The re-claim resumes from the cursor: nothing doubled, nothing skipped.
    f.drain(Duration::ZERO).await;
    assert_eq!(f.messages(id).await, 25_000);
    let distinct: i64 = sqlx::query_scalar(
        "select count(distinct contact_id) from messages where campaign_id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(distinct, 25_000);
    let c = campaign::load(&pool, f.ws, id).await.unwrap();
    assert_eq!((c.total, c.resolved), (25_000, true));
    assert_eq!(f.state(id).await, "sending");
}

#[sqlx::test]
async fn rerunning_a_batch_inserts_nothing_twice(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 50, "ada").await;
    let id = f.campaign(&[f.list], json!({})).await;
    let job = f.schedule(id).await;
    f.drain(SLICE).await;
    assert_eq!(f.total(id).await, 50);

    let c = campaign::load(&pool, f.ws, id).await.unwrap();
    let r = Resolution::load(&pool, &c, job).await.unwrap();
    let again = resolve_batch(&pool, &r, None).await.unwrap();
    assert_eq!((again.scanned, again.inserted, again.stale), (50, 0, false));
    assert_eq!((f.messages(id).await, f.total(id).await), (50, 50));

    // A batch fenced on some other job writes nothing, even to a sending campaign.
    let other = Resolution::load(&pool, &c, Uuid::now_v7()).await.unwrap();
    assert!(resolve_batch(&pool, &other, None).await.unwrap().stale);
    // A campaign that is no longer this job's changes nothing, and says so.
    sqlx::query("update campaigns set state = 'cancelled' where id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    f.contacts(f.list, 5, "late").await;
    let stale = resolve_batch(&pool, &r, None).await.unwrap();
    assert!(stale.stale);
    assert_eq!(
        (f.messages(id).await, f.total(id).await),
        (50, 50),
        "rolled back"
    );
}

#[sqlx::test]
async fn a_launch_after_unschedule_does_nothing(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 5, "ada").await;
    let id = f.campaign(&[f.list], json!({})).await;
    let path = |v: &str| format!("/api/v1/workspaces/{}/campaigns/{id}/{v}", f.ws);
    f.app
        .post(&path("schedule"))
        .json(&json!({"mode": "instant", "zone": "America/New_York", "local": null}))
        .await
        .assert_status_ok();
    f.app.post(&path("unschedule")).await.assert_status_ok();

    assert_eq!(
        f.drain(SLICE).await,
        1,
        "the orphaned job is claimed and finished"
    );
    assert_eq!(f.state(id).await, "draft");
    assert_eq!(f.messages(id).await, 0);
    let c = campaign::load(&pool, f.ws, id).await.unwrap();
    assert!(c.frozen_at.is_none() && c.launched_at.is_none());
    let open: i64 = sqlx::query_scalar(
        "select count(*) from jobs where campaign_id = $1 and completed_at is null",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        open, 0,
        "the job completed rather than waiting to be claimed again"
    );

    // A job that is not the campaign's current one is just as inert.
    let id = f.campaign(&[f.list], json!({})).await;
    f.schedule(id).await;
    f.launch_job(id, Uuid::now_v7()).await; // a stale twin
    f.drain(SLICE).await;
    assert_eq!(
        f.messages(id).await,
        5,
        "launched once, by the job it points at"
    );
    assert_eq!(f.state(id).await, "sending");
}

#[sqlx::test]
async fn a_deleted_list_fails_the_launch_and_names_it(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 5, "ada").await;
    let doomed = new_list(&f.app, f.ws, "doomed").await;
    let id = f.campaign(&[f.list, doomed], json!({})).await;
    f.schedule(id).await;
    sqlx::query("delete from lists where id = $1")
        .bind(doomed)
        .execute(&pool)
        .await
        .unwrap();
    f.drain(SLICE).await;

    assert_eq!(f.state(id).await, "failed");
    let c = campaign::load(&pool, f.ws, id).await.unwrap();
    let reason = c.failure_reason.expect("a reason");
    assert!(reason.contains(&doomed.to_string()), "{reason}");
    assert_eq!(
        f.messages(id).await,
        0,
        "nothing was queued from the lists that remain"
    );
    assert!(c.frozen_at.is_none());
}

#[sqlx::test]
async fn a_phase_two_refusal_pauses_instead_of_failing(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 30, "ada").await;
    let id = f.ab(20).await;
    f.schedule(id).await;
    f.drain(SLICE).await;
    f.app
        .post(&format!(
            "/api/v1/workspaces/{}/campaigns/{id}/winner",
            f.ws
        ))
        .json(&json!({"variant": 1}))
        .await
        .assert_status_ok();
    sqlx::query("delete from lists where id = $1")
        .bind(f.list)
        .execute(&pool)
        .await
        .unwrap();
    f.drain(SLICE).await;
    assert_eq!(f.state(id).await, "paused");
    let c = campaign::load(&pool, f.ws, id).await.unwrap();
    assert!(c.pause_reason.unwrap().contains(&f.list.to_string()));
    assert!(c.failure_reason.is_none());
}

#[sqlx::test]
async fn a_brand_kit_edit_between_schedule_and_launch_does_not_refuse_it(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 3, "ada").await;
    let id = f.campaign(&[f.list], json!({})).await;
    // Scheduled through the API, so `prepare` ran and the template was compiled against the old kit.
    f.app
        .post(&format!(
            "/api/v1/workspaces/{}/campaigns/{id}/schedule",
            f.ws
        ))
        .json(&json!({"mode": "instant", "zone": "America/New_York", "local": null}))
        .await
        .assert_status_ok();
    f.app
        .put(&format!("/api/v1/workspaces/{}/brand-kit", f.ws))
        .json(
            &json!({"logo_url": null, "palette": {"primary": "#ff0000"}, "font_stack": null,
                      "footer": {"address": "2 New Rd, Elsewhere"}}),
        )
        .await
        .assert_status_ok();
    f.drain(SLICE).await;
    assert_eq!(
        f.state(id).await,
        "sending",
        "the launch recompiles; it does not refuse"
    );
    assert_eq!(f.messages(id).await, 3);
}

#[sqlx::test]
async fn a_template_edit_after_launch_does_not_reach_the_campaign(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 3, "ada").await;
    let id = f.campaign(&[f.list], json!({})).await;
    f.schedule(id).await;
    f.drain(SLICE).await;
    let before: Value = sqlx::query_scalar("select frozen from campaigns where id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();

    f.app
        .patch(&format!(
            "/api/v1/workspaces/{}/templates/{}",
            f.ws, f.template
        ))
        .json(&json!({"tree": {"blocks": [{"type": "Text", "runs": [
            {"type": "Text", "text": "a different email entirely", "marks": []}]}]}}))
        .await
        .assert_status_ok();
    f.app
        .put(&format!("/api/v1/workspaces/{}/brand-kit", f.ws))
        .json(&json!({"logo_url": null, "palette": {}, "font_stack": null,
                      "footer": {"address": "3 Other St"}}))
        .await
        .assert_status_ok();
    let after: Value = sqlx::query_scalar("select frozen from campaigns where id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "the frozen rendition is the launch's, whatever is edited since"
    );
    assert!(!after.to_string().contains("a different email entirely"));
}

#[sqlx::test]
async fn ab_sampling_is_deterministic_and_near_the_percent(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 10_000, "ada").await;
    let id = f.ab(20).await;
    f.schedule(id).await;
    f.drain(SLICE).await;

    let cohort = f.variants(id).await;
    assert!(
        (1_800..=2_200).contains(&cohort.len()),
        "a 20% test of 10,000 queued {}",
        cohort.len()
    );
    let ones = cohort.values().filter(|v| **v == 1).count();
    assert!(
        (800..=1_200).contains(&ones),
        "the cohort splits evenly, and {ones} of {} got variant 1",
        cohort.len()
    );
    assert!(cohort.values().all(|v| *v == 0 || *v == 1));

    // Start over from nothing: the same campaign picks the same people with the same variants.
    sqlx::query("delete from messages where campaign_id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "update campaigns set state = 'scheduled', resolve_cursor = null, resolved = false, \
         total = 0, frozen = null, frozen_at = null where id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    let job = Uuid::now_v7();
    sqlx::query("update campaigns set launch_job_id = $2 where id = $1")
        .bind(id)
        .bind(job)
        .execute(&pool)
        .await
        .unwrap();
    f.launch_job(id, job).await;
    f.drain(SLICE).await;
    assert_eq!(
        f.variants(id).await,
        cohort,
        "the sample is a function of the contact"
    );
}

#[sqlx::test]
async fn phase_two_sends_the_winner_to_everyone_not_already_resolved(pool: PgPool) {
    let f = fx(&pool).await;
    let people = f.contacts(f.list, 100, "ada").await;
    let id = f.ab(20).await;
    f.schedule(id).await;
    f.drain(SLICE).await;
    let cohort = f.variants(id).await;
    assert!(
        !cohort.is_empty() && cohort.len() < 100,
        "a test group, not everyone"
    );
    assert!(f.state(id).await == "sending");

    // Between the phases: one unsubscribes, one joins.
    let leaver = *people.iter().find(|p| !cohort.contains_key(p)).unwrap();
    sqlx::query("update list_memberships set status = 'unsubscribed' where contact_id = $1")
        .bind(leaver)
        .execute(&pool)
        .await
        .unwrap();
    let newcomer = f.contact("late@x.test", "ada").await;
    f.member(newcomer, f.list, "subscribed").await;

    f.app
        .post(&format!(
            "/api/v1/workspaces/{}/campaigns/{id}/winner",
            f.ws
        ))
        .json(&json!({"variant": 1}))
        .await
        .assert_status_ok();
    f.drain(SLICE).await;

    let all = f.variants(id).await;
    assert_eq!(all.len(), 100, "100 - the leaver + the newcomer");
    assert!(
        !all.contains_key(&leaver),
        "an unsubscribe since phase 1 is respected"
    );
    assert!(
        all.contains_key(&newcomer),
        "a contact who joined since gets the winner"
    );
    for (contact, variant) in &all {
        match cohort.get(contact) {
            Some(was) => assert_eq!(variant, was, "the test group keeps what it was sent"),
            None => assert_eq!(*variant, 1, "everyone else gets the winner"),
        }
    }
    assert_eq!(f.total(id).await, 100);
}

#[sqlx::test]
async fn recipient_local_sets_run_after_from_each_contacts_zone(pool: PgPool) {
    let f = fx(&pool).await;
    let id = f.campaign(&[f.list], json!({})).await;
    // A fixed date is safe: the launch never checks that `send_at` is in the future.
    let job = Uuid::now_v7();
    sqlx::query(
        "update campaigns set state = 'scheduled', launch_job_id = $2, \
         schedule_mode = 'recipient_local', schedule_zone = 'America/New_York', \
         schedule_local = '2027-03-14 09:00' where id = $1",
    )
    .bind(id)
    .bind(job)
    .execute(&pool)
    .await
    .unwrap();
    f.launch_job(id, job).await;
    for (email, zone) in [
        ("tokyo@x.test", Some("Asia/Tokyo")),
        ("newyork@x.test", Some("America/New_York")),
        ("none@x.test", None),
        ("invalid@x.test", Some("Mars/Olympus_Mons")),
    ] {
        let c = f.contact(email, "ada").await;
        sqlx::query("update contacts set timezone = $2 where id = $1")
            .bind(c)
            .bind(zone)
            .execute(&pool)
            .await
            .unwrap();
        f.member(c, f.list, "subscribed").await;
    }
    f.drain(SLICE).await;

    let due: BTreeMap<String, DateTime<Utc>> = sqlx::query_as::<_, (String, DateTime<Utc>)>(
        "select k.email, m.run_after from messages m join contacts k on k.id = m.contact_id
          where m.campaign_id = $1",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap()
    .into_iter()
    .collect();
    let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().to_utc();
    // Tokyo is UTC+9 all year; New York has sprung forward by 09:00 on the 14th (UTC-4).
    assert_eq!(due["tokyo@x.test"], at("2027-03-14T00:00:00Z"));
    assert_eq!(due["newyork@x.test"], at("2027-03-14T13:00:00Z"));
    // No zone, or one nobody knows: the campaign's own zone.
    assert_eq!(due["none@x.test"], at("2027-03-14T13:00:00Z"));
    assert_eq!(due["invalid@x.test"], at("2027-03-14T13:00:00Z"));
}

#[sqlx::test]
async fn an_instant_campaign_queues_everyone_due_now(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 3, "ada").await;
    let id = f.campaign(&[f.list], json!({})).await;
    f.schedule(id).await;
    f.drain(SLICE).await;
    let late: i64 = sqlx::query_scalar(
        "select count(*) from messages where campaign_id = $1 and run_after > now() + interval '5 seconds'",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((f.messages(id).await, late), (3, 0));
}

#[sqlx::test]
async fn an_empty_audience_finishes_as_sent_with_zero(pool: PgPool) {
    let f = fx(&pool).await;
    let id = f.campaign(&[f.list], json!({})).await;
    f.schedule(id).await;
    f.drain(SLICE).await;
    assert_eq!(f.state(id).await, "sent");
    let c = campaign::load(&pool, f.ws, id).await.unwrap();
    assert_eq!((c.total, c.resolved), (0, true));
    assert!(c.finished_at.is_some() && c.frozen_at.is_some());
}

#[sqlx::test]
async fn the_jobs_tick_runs_a_launch(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 3, "ada").await;
    let id = f.campaign(&[f.list], json!({})).await;
    f.schedule(id).await;
    let ctx = Ctx {
        db: pool.clone(),
        secrets: Arc::clone(&f.ctx.secrets),
        config: Arc::clone(&f.ctx.config),
        transports: Arc::new(Transports::new()),
    };
    let claimed = Worker::new(ctx, 1).tick().await.expect("tick");
    assert_eq!(claimed, 1, "the tick claimed the launch job");
    assert_eq!(f.state(id).await, "sending");
    assert_eq!(f.messages(id).await, 3);
}

// ---------------------------------------------------------------------------------------------
// Fix round 1

impl Fx {
    /// Claims the next launch job and runs it for `slice`. Returns whether one was due.
    async fn one_slice(&self, slice: Duration) -> bool {
        let Some(job) = worker::claim(&self.pool, LAUNCH_KIND, 1, 120)
            .await
            .expect("claim")
            .pop()
        else {
            return false;
        };
        nm::campaign::launch::handle(&self.ctx, &job, slice).await;
        true
    }

    fn verb(&self, id: Uuid, verb: &str) -> String {
        format!("/api/v1/workspaces/{}/campaigns/{id}/{verb}", self.ws)
    }

    async fn row(&self, id: Uuid) -> (String, bool, Option<String>, Option<Uuid>) {
        sqlx::query_as(
            "select state, resolved, pause_reason, launch_job_id from campaigns where id = $1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn frozen_of(&self, id: Uuid) -> (Value, DateTime<Utc>, DateTime<Utc>) {
        sqlx::query_as("select frozen, frozen_at, launched_at from campaigns where id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// Edits what a launched campaign must not follow: its template and the brand kit's footer.
    async fn edit_content(&self, n: u32) {
        self.app
            .patch(&format!(
                "/api/v1/workspaces/{}/templates/{}",
                self.ws, self.template
            ))
            .json(&json!({"tree": {"blocks": [{"type": "Text", "runs": [
                {"type": "Text", "text": format!("edited {n}"), "marks": []}]}]}}))
            .await
            .assert_status_ok();
        self.app
            .put(&format!("/api/v1/workspaces/{}/brand-kit", self.ws))
            .json(&json!({"logo_url": null, "palette": {}, "font_stack": null,
                          "footer": {"address": format!("{n} Changed Rd")}}))
            .await
            .assert_status_ok();
    }
}

#[sqlx::test]
async fn a_segment_edited_between_slices_does_not_change_the_phase(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 10_001, "ada").await;
    let r = f
        .app
        .post(&format!("/api/v1/workspaces/{}/segments", f.ws))
        .json(&json!({"name": "adas", "rules": {"match": "all", "rules": [
            {"field": "first_name", "op": "eq", "value": "ada"}]}}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let segment = id_of(&r);
    let id = f.campaign(&[f.list], json!({"segment_id": segment})).await;
    f.schedule(id).await;

    assert!(f.one_slice(Duration::ZERO).await);
    assert_eq!(f.messages(id).await, LAUNCH_BATCH, "one slice, one batch");
    // Between the slices the segment is edited so that the contact still to come no longer
    // matches it. The phase's snapshot of the rules is what the next slice uses.
    f.app
        .patch(&format!("/api/v1/workspaces/{}/segments/{segment}", f.ws))
        .json(&json!({"rules": {"match": "all", "rules": [
            {"field": "first_name", "op": "eq", "value": "nobody"}]}}))
        .await
        .assert_status_ok();
    f.drain(SLICE).await;
    assert_eq!(
        f.messages(id).await,
        10_001,
        "the whole phase used the rules it began with"
    );
    assert!(f.row(id).await.1);
}

#[sqlx::test]
async fn a_template_or_brand_kit_edit_while_resolving_does_not_reach_the_campaign(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 10_001, "ada").await;
    let id = f.ab(20).await;
    f.schedule(id).await;
    assert!(f.one_slice(Duration::ZERO).await);
    let launched = f.frozen_of(id).await;

    // Between two slices of phase 1.
    f.edit_content(1).await;
    f.drain(SLICE).await;
    assert_eq!(
        f.frozen_of(id).await,
        launched,
        "a later slice does not freeze again"
    );

    // Before phase 2.
    f.app
        .post(&f.verb(id, "winner"))
        .json(&json!({"variant": 1}))
        .await
        .assert_status_ok();
    f.edit_content(2).await;
    f.drain(SLICE).await;
    assert_eq!(
        f.frozen_of(id).await,
        launched,
        "phase 2 sends what was frozen at launch"
    );
    assert!(f.messages(id).await > 10_000);
}

#[sqlx::test]
async fn a_slice_resuming_from_a_stored_cursor_makes_every_full_batch_in_one_claim(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 30_001, "ada").await;
    let id = f.campaign(&[f.list], json!({})).await;
    f.schedule(id).await;
    assert!(f.one_slice(Duration::ZERO).await);
    assert_eq!(f.messages(id).await, LAUNCH_BATCH);
    // A fresh claim resumes from the stored cursor and runs three batches (two full, one of one
    // contact) without handing off. A cursor that stopped advancing would rescan the same ten
    // thousand until the slice ran out.
    assert!(f.one_slice(Duration::from_secs(30)).await);
    assert_eq!(f.messages(id).await, 30_001);
    assert!(f.row(id).await.1, "resolved in that one claim");
    assert!(!f.one_slice(SLICE).await, "no successor was queued");
}

#[sqlx::test]
async fn resuming_an_unresolved_campaign_queues_a_job_that_carries_the_snapshot(pool: PgPool) {
    let f = fx(&pool).await;
    let doomed = new_list(&f.app, f.ws, "doomed").await;
    f.contacts(f.list, 10_001, "ada").await;
    let id = f.campaign(&[f.list, doomed], json!({})).await;
    f.schedule(id).await;
    assert!(f.one_slice(Duration::ZERO).await);
    let (_, _, _, second) = f.row(id).await;
    let second = second.expect("the hand-off job");
    let snapshot: Value = sqlx::query_scalar("select payload from jobs where id = $1")
        .bind(second)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(snapshot["lists"][doomed.to_string()], "doomed");

    sqlx::query("delete from lists where id = $1")
        .bind(doomed)
        .execute(&pool)
        .await
        .unwrap();
    f.drain(SLICE).await;
    let (state, resolved, reason, _) = f.row(id).await;
    assert_eq!((state.as_str(), resolved), ("paused", false));
    let reason = reason.expect("a reason");
    assert!(
        reason.contains("doomed") && reason.contains("cancel this campaign"),
        "{reason}"
    );

    // Resuming while the list is still gone refuses again, and says the same thing.
    f.app.post(&f.verb(id, "resume")).await.assert_status_ok();
    f.drain(SLICE).await;
    let (state, _, again, _) = f.row(id).await;
    assert_eq!(state, "paused");
    assert!(again.unwrap().contains("cancel this campaign"));

    // The operator takes the list off the audience (here, directly), and resume finishes the job.
    sqlx::query("update campaigns set list_ids = $2 where id = $1")
        .bind(id)
        .bind(vec![f.list])
        .execute(&pool)
        .await
        .unwrap();
    f.app.post(&f.verb(id, "resume")).await.assert_status_ok();
    let (state, _, _, job) = f.row(id).await;
    let job = job.expect("a new launch job");
    assert_eq!(state, "sending");
    assert_ne!(job, second, "resume repointed the campaign");
    let carried: Value = sqlx::query_scalar("select payload from jobs where id = $1")
        .bind(job)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        carried, snapshot,
        "the new job carries the phase's snapshot"
    );
    f.drain(SLICE).await;
    assert_eq!(f.messages(id).await, 10_001);
    assert!(f.row(id).await.1, "the audience is resolved");
}

#[sqlx::test]
async fn a_refusal_on_a_campaign_the_operator_paused_is_recorded(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 10_001, "ada").await;
    let id = f.campaign(&[f.list], json!({})).await;
    f.schedule(id).await;
    assert!(f.one_slice(Duration::ZERO).await);
    f.app.post(&f.verb(id, "pause")).await.assert_status_ok();
    sqlx::query("delete from lists where id = $1")
        .bind(f.list)
        .execute(&pool)
        .await
        .unwrap();
    f.drain(SLICE).await;
    let (state, resolved, reason, _) = f.row(id).await;
    assert_eq!((state.as_str(), resolved), ("paused", false));
    assert!(
        reason
            .expect("the reason survives the pause")
            .contains("news"),
        "the list is named"
    );

    f.app.post(&f.verb(id, "resume")).await.assert_status_ok();
    f.drain(SLICE).await;
    let (state, _, reason, _) = f.row(id).await;
    assert_eq!(
        state, "paused",
        "the campaign ends paused, not stuck sending"
    );
    assert!(reason.unwrap().contains("cancel this campaign"));
}

#[sqlx::test]
async fn resuming_a_resolved_campaign_queues_no_job(pool: PgPool) {
    let f = fx(&pool).await;
    let id = f.campaign(&[f.list], json!({})).await;
    sqlx::query("update campaigns set state = 'paused', resolved = true where id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    f.app.post(&f.verb(id, "resume")).await.assert_status_ok();
    let jobs: i64 = sqlx::query_scalar("select count(*) from jobs where campaign_id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs, 0);
}

/// A trigger that makes one statement of the launch fail, the way a database error would.
async fn break_updates_of(pool: &PgPool, column: &str) {
    sqlx::query(
        "create or replace function launch_boom() returns trigger language plpgsql as \
         $$ begin raise exception 'boom'; end $$",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("drop trigger if exists boom on campaigns")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "create trigger boom before update of {column} on campaigns \
         for each row execute function launch_boom()"
    )))
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn a_launch_that_runs_out_of_attempts_on_database_errors_ends_the_campaign(pool: PgPool) {
    let f = fx(&pool).await;
    f.contacts(f.list, 10, "ada").await;
    break_updates_of(&pool, "frozen").await;

    // Not the last attempt: the job waits for a retry and the campaign is left alone.
    let id = f.campaign(&[f.list], json!({})).await;
    let job = f.schedule(id).await;
    assert!(f.one_slice(SLICE).await);
    assert_eq!(f.state(id).await, "scheduled");
    let (failed, err): (Option<DateTime<Utc>>, Option<String>) =
        sqlx::query_as("select failed_at, last_error from jobs where id = $1")
            .bind(job)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(failed.is_none() && err.is_some(), "deferred, not failed");

    // The last attempt, before anything was sent: the campaign fails, with the reason.
    let id = f.campaign(&[f.list], json!({})).await;
    let job = f.schedule(id).await;
    sqlx::query("update jobs set max_attempts = 1 where id = $1")
        .bind(job)
        .execute(&pool)
        .await
        .unwrap();
    assert!(f.one_slice(SLICE).await);
    assert_eq!(f.state(id).await, "failed");
    let c = campaign::load(&pool, f.ws, id).await.unwrap();
    assert!(
        c.failure_reason
            .unwrap()
            .contains("gave up after 1 attempts")
    );
    let failed: Option<DateTime<Utc>> =
        sqlx::query_scalar("select failed_at from jobs where id = $1")
            .bind(job)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(failed.is_some(), "the job failed in the same step");

    // The last attempt after the freeze: something may have been sent, so it is a system pause.
    break_updates_of(&pool, "total").await;
    let id = f.campaign(&[f.list], json!({})).await;
    let job = f.schedule(id).await;
    sqlx::query("update jobs set max_attempts = 1 where id = $1")
        .bind(job)
        .execute(&pool)
        .await
        .unwrap();
    assert!(f.one_slice(SLICE).await);
    let (state, _, reason, _) = f.row(id).await;
    assert_eq!(state, "paused");
    assert!(reason.unwrap().contains("gave up after 1 attempts"));
}
