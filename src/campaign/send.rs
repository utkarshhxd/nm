// SPDX-License-Identifier: AGPL-3.0-only

//! The send loop (spec M2c §4.7): claimed messages, rendered from the campaign's frozen content,
//! submitted to a relay, recorded. The worst outcome is a message sent twice, the second worst a
//! message to someone who must not get it; each step below is ordered against those two.
//!
//! One tick:
//!
//! 1. [`queue::sweep`], then [`queue::claim`] for the campaigns [`States`] says are sending.
//! 2. [`queue::eligible`] once for the batch; whoever is missing is `Skipped`, for good.
//! 3. Per campaign: the frozen content, the field definitions, the sending domain and its key,
//!    the domain gate and [`frozen_usable`] — the predicate preview and resume use, so what is
//!    previewed is what is sent. A failure is [`system_pause`] and a release of that campaign's
//!    messages: each is fixable by an operator, so the campaign waits rather than fails.
//! 4. Per workspace: the relays and their passwords, read once ([`Creds`]), and one debit of the
//!    bucket; what it does not pay for is released, spending no attempt.
//! 5. Up to `worker_campaign_concurrency` sends at once, each: the tick's deadline, then
//!    [`States::is_sending`] (a pause stops here, within [`STATE_REFRESH`]), then the write-ahead
//!    [`queue::mark_submitting`] — `false` means another claim owns the row and it is dropped
//!    without a word — then render, build, sign, submit.
//! 6. Outcomes go to a flusher that records them every [`FLUSH_EVERY`] or [`FLUSH_AT`].
//!
//! `mrml` never runs here: [`Ready::render`](crate::template::Ready::render) substitutes into the
//! frozen document, which carries the system footer and was classified at launch.
//!
//! Lock order (ADR 0005): every statement here is one of `queue`'s, `bucket`'s, or
//! [`system_pause`], which writes the `campaigns` row alone. No transaction is held across a send.

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    sync::{Arc, Mutex, PoisonError, RwLock},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use lettre::address::Envelope;
use serde_json::Value;
use sqlx::PgPool;
use tokio::{sync::mpsc, task::JoinSet};
use uuid::Uuid;

use super::{
    Frozen, frozen_usable,
    queue::{self, CAMPAIGN_BATCH, Claimed, Recipient, Verdict},
};
use crate::{
    crypto::secrets::Secrets,
    custom_fields::{Definition, definitions_for},
    mail::{self, Outgoing, dkim},
    public::token::{self, Purpose, Token},
    relay::{self, Relay, transport::Outcome},
    sending_domains::{self, SendingDomain},
    template::{render::Contact, text::render_subject},
    worker::{
        BUDGET_SPENT, JOB_LEASE, RATE_LIMITED, RATE_LIMITED_WAIT,
        confirmation::Ctx,
        debit,
        deliver::{Creds, signed, signing_key, submit},
        poll_interval,
    },
};

/// How often the campaign-state map is re-read (spec §4.7.7).
pub const STATE_REFRESH: Duration = Duration::from_secs(1);

/// A map older than this is not trusted: every campaign reads as not sending until it is
/// refreshed. A refresher that errors or stalls must not stretch a pause past invariant 5's 5 s.
const STALE_AFTER: Duration = Duration::from_secs(4);

// Invariant 5's arithmetic: a send checks the map right before its write-ahead, the map is read
// every `STATE_REFRESH`, and one older than `STALE_AFTER` stops every send. So a pause is seen
// within `STALE_AFTER` at worst, which must stay under the 5 s the spec promises.
// `pause_stops_new_submissions_within_five_seconds` holds the ordering half.
const _: () = assert!(
    STATE_REFRESH.as_secs() < STALE_AFTER.as_secs() && STALE_AFTER.as_secs() < 5,
    "a pause must be seen within 5 s: STATE_REFRESH < STALE_AFTER < 5 s"
);

/// The flusher records what it holds at least this often (spec §4.7.6)...
pub const FLUSH_EVERY: Duration = Duration::from_millis(250);
/// ...or as soon as it holds this many outcomes.
pub const FLUSH_AT: usize = 100;

const NOT_MAILABLE: &str = "not mailable since launch";
const NOT_SENDING: &str = "the campaign stopped sending before this message was started";
const NO_RELAY: &str = "the workspace has no enabled relay";

/// Sending campaign -> its `frozen_at`.
type Sending = HashMap<Uuid, Option<DateTime<Utc>>>;

/// Campaign -> its frozen content, and the `frozen_at` it was read at.
type FrozenCache = HashMap<Uuid, (Option<DateTime<Utc>>, Arc<Frozen>)>;

/// (workspace, sending domain) -> the domain and its DKIM key, read once per tick.
type Domains = HashMap<(Uuid, Uuid), (Arc<SendingDomain>, Option<Arc<dkim::SigningKey>>)>;

/// The campaigns that are `sending`, and when each was frozen, as of the last refresh (when).
#[derive(Default)]
pub struct States(RwLock<Option<(Instant, Sending)>>);

impl States {
    /// Re-reads every sending campaign. On an error the old map is kept, and goes stale.
    pub async fn refresh(&self, db: &PgPool) -> sqlx::Result<()> {
        let rows: Vec<(Uuid, Option<DateTime<Utc>>)> =
            sqlx::query_as("select id, frozen_at from campaigns where state = 'sending'")
                .fetch_all(db)
                .await?;
        *self.0.write().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now(), rows.into_iter().collect()));
        Ok(())
    }

    /// `f` over the map, or over nothing when it was never read or has gone stale.
    fn read<T>(&self, f: impl FnOnce(&Sending) -> T) -> T {
        let guard = self.0.read().unwrap_or_else(PoisonError::into_inner);
        match &*guard {
            Some((at, map)) if at.elapsed() <= STALE_AFTER => f(map),
            _ => f(&HashMap::new()),
        }
    }

    pub fn sending(&self) -> Vec<Uuid> {
        self.read(|m| m.keys().copied().collect())
    }

    pub fn is_sending(&self, id: Uuid) -> bool {
        self.read(|m| m.contains_key(&id))
    }

    pub fn frozen_at(&self, id: Uuid) -> Option<DateTime<Utc>> {
        self.read(|m| m.get(&id).copied().flatten())
    }
}

/// Refreshes `states` every [`STATE_REFRESH`] until `shutdown` resolves.
pub async fn refresh_until(states: &States, db: &PgPool, shutdown: impl Future<Output = ()>) {
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        if let Err(e) = states.refresh(db).await {
            tracing::error!(error = %e, "the campaign states could not be refreshed");
        }
        tokio::select! {
            biased;
            () = &mut shutdown => return,
            () = tokio::time::sleep(STATE_REFRESH) => {}
        }
    }
}

/// A system pause (spec §4.7.10): the `campaigns` row alone, with the reason the screen shows. A
/// campaign an operator paused meanwhile keeps the reason too, since resuming would meet it.
pub async fn system_pause(db: &PgPool, campaign_id: Uuid, reason: &str) -> sqlx::Result<()> {
    sqlx::query(
        "update campaigns set state = 'paused', pause_reason = $2, updated_at = now()
          where id = $1 and state in ('sending', 'paused')",
    )
    .bind(campaign_id)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(())
}

/// What one campaign's messages are built from this tick.
struct Content {
    frozen: Arc<Frozen>,
    defs: Arc<Vec<Definition>>,
    domain: Arc<SendingDomain>,
    key: Option<Arc<dkim::SigningKey>>,
}

/// What one workspace's messages are sent through this tick.
struct Route {
    relays: Vec<Relay>,
    creds: Creds,
    paid_for: Option<Uuid>,
}

/// Why a campaign's messages are not sent this tick.
enum Hold {
    /// The operator's to fix: system pause, with this reason.
    Pause(String),
    /// Ours (a database error): release and try again shortly.
    Wait(String),
}

/// The messages loop.
pub struct CampaignWorker {
    ctx: Arc<Ctx>,
    states: Arc<States>,
    concurrency: usize,
    budget: Duration,
    /// Frozen content by campaign, with the `frozen_at` it was read at. Each tick asks the database
    /// for the current `frozen_at`: a resume that re-freezes moves it, and the new document is read.
    frozen: Mutex<FrozenCache>,
}

impl CampaignWorker {
    /// `concurrency` zero reads as one.
    pub fn new(ctx: Arc<Ctx>, states: Arc<States>, concurrency: usize) -> Self {
        CampaignWorker {
            ctx,
            states,
            concurrency: concurrency.max(1),
            budget: JOB_LEASE / 2,
            frozen: Mutex::default(),
        }
    }

    /// A budget other than half the lease, for a test that has to watch one run out.
    #[doc(hidden)]
    pub fn with_budget(self, b: Duration) -> Self {
        CampaignWorker { budget: b, ..self }
    }

    /// Ticks until `shutdown` resolves: straight on while there is work, a poll apart when not.
    pub async fn run_until(&self, shutdown: impl Future<Output = ()>) {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            let pause = match self.tick().await {
                Ok(0) => poll_interval(),
                Ok(_) => Duration::ZERO,
                Err(e) => {
                    tracing::error!(error = %e, "a campaign tick could not claim");
                    poll_interval()
                }
            };
            tokio::select! {
                biased;
                () = &mut shutdown => return,
                () = tokio::time::sleep(pause) => {}
            }
        }
    }

    /// One tick (see the module docs). Returns how many messages were claimed.
    pub async fn tick(&self) -> sqlx::Result<usize> {
        let started = Instant::now();
        let db = &self.ctx.db;
        queue::sweep(db).await?;
        let sending = self.states.sending();
        self.frozen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|id, _| sending.contains(id));
        let claimed = queue::claim(db, &sending, CAMPAIGN_BATCH, JOB_LEASE).await?;
        let total = claimed.len();
        if total == 0 {
            return Ok(0);
        }

        let ids: Vec<Uuid> = claimed.iter().map(|m| m.id).collect();
        let mut mailable: HashMap<Uuid, Recipient> = match queue::eligible(db, &ids).await {
            Ok(r) => r.into_iter().map(|r| (r.message_id, r)).collect(),
            Err(e) => {
                // Nothing was submitted: hand every one back unattempted.
                release(db, &claimed, Duration::ZERO, "the eligibility check failed").await;
                return Err(e);
            }
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let flusher = tokio::spawn(flush(db.clone(), Arc::clone(&self.ctx.secrets), rx));

        let mut by_campaign: BTreeMap<Uuid, Vec<(Claimed, Recipient)>> = BTreeMap::new();
        for m in claimed {
            match mailable.remove(&m.id) {
                Some(r) => by_campaign.entry(m.campaign_id).or_default().push((m, r)),
                None => {
                    let _ = tx.send((m, Verdict::Skipped(NOT_MAILABLE.to_owned())));
                }
            }
        }

        let mut defs: HashMap<Uuid, Arc<Vec<Definition>>> = HashMap::new();
        let mut domains = Domains::new();
        let mut by_workspace: BTreeMap<Uuid, Vec<(Claimed, Recipient, Arc<Content>)>> =
            BTreeMap::new();
        for (campaign_id, messages) in by_campaign {
            let ws = messages[0].0.workspace_id;
            match self.content(campaign_id, ws, &mut defs, &mut domains).await {
                Ok(content) => {
                    let content = Arc::new(content);
                    by_workspace.entry(ws).or_default().extend(
                        messages
                            .into_iter()
                            .map(|(m, r)| (m, r, Arc::clone(&content))),
                    );
                }
                Err(hold) => {
                    let held: Vec<Claimed> = messages.into_iter().map(|(m, _)| m).collect();
                    match hold {
                        Hold::Pause(reason) => {
                            tracing::warn!(campaign = %campaign_id, %reason, "system pause");
                            // Paused, nothing claims these again until a resume. Not paused, they
                            // wait: released at once, the next tick would claim them straight back.
                            let wait = match system_pause(db, campaign_id, &reason).await {
                                Ok(()) => Duration::ZERO,
                                Err(e) => {
                                    tracing::error!(campaign = %campaign_id, error = %e, "a system pause could not be written");
                                    RATE_LIMITED_WAIT
                                }
                            };
                            release(db, &held, wait, &reason).await;
                        }
                        Hold::Wait(reason) => release(db, &held, RATE_LIMITED_WAIT, &reason).await,
                    }
                }
            }
        }

        let deadline = started + self.budget;
        let mut running: JoinSet<()> = JoinSet::new();
        for (ws, mut items) in by_workspace {
            let relays = match relay::enabled_relays(db, ws).await {
                Ok(relays) => relays,
                Err(e) => {
                    tracing::warn!(error = ?e, "the relays could not be read");
                    let held: Vec<Claimed> = items.into_iter().map(|(m, ..)| m).collect();
                    release(db, &held, RATE_LIMITED_WAIT, "the relays could not be read").await;
                    continue;
                }
            };
            if relays.is_empty() {
                let held: Vec<Claimed> = items.into_iter().map(|(m, ..)| m).collect();
                release(db, &held, RATE_LIMITED_WAIT, NO_RELAY).await;
                continue;
            }
            let n = i32::try_from(items.len()).unwrap_or(i32::MAX);
            let (paid_for, allowed) = match debit(db, ws, &relays, n).await {
                Ok(Some((relay, granted))) => (
                    Some(relay),
                    usize::try_from(granted).unwrap_or(0).min(items.len()),
                ),
                Ok(None) => (None, 0),
                Err(e) => {
                    tracing::warn!(error = %e, "the relay bucket could not be debited");
                    (None, 0)
                }
            };
            // Unpaid is not unsent for good: released, spending no attempt.
            let held: Vec<Claimed> = items
                .split_off(allowed)
                .into_iter()
                .map(|(m, ..)| m)
                .collect();
            release(db, &held, RATE_LIMITED_WAIT, RATE_LIMITED).await;
            if items.is_empty() {
                continue;
            }
            let route = Arc::new(Route {
                creds: Creds::load(&self.ctx, ws, &relays).await,
                relays,
                paid_for,
            });
            for (m, r, content) in items {
                while running.len() >= self.concurrency {
                    let _ = running.join_next().await;
                }
                let (ctx, states, route, tx) = (
                    Arc::clone(&self.ctx),
                    Arc::clone(&self.states),
                    Arc::clone(&route),
                    tx.clone(),
                );
                running.spawn(async move {
                    if Instant::now() >= deadline {
                        release(&ctx.db, &[m], Duration::ZERO, BUDGET_SPENT).await;
                        return;
                    }
                    // A pause stops here: nothing past this line is started for a campaign that
                    // is not sending.
                    if !states.is_sending(m.campaign_id) {
                        release(&ctx.db, &[m], Duration::ZERO, NOT_SENDING).await;
                        return;
                    }
                    match queue::mark_submitting(&ctx.db, &m).await {
                        Ok(true) => {}
                        // Another claim owns the row now.
                        Ok(false) => return,
                        Err(e) => {
                            // Not known to be `submitting`: the release touches only a `queued`
                            // row, and one that did commit is the sweep's.
                            tracing::warn!(message = %m.id, error = %e, "the write-ahead failed");
                            release(&ctx.db, &[m], Duration::ZERO, "the write-ahead failed").await;
                            return;
                        }
                    }
                    let verdict = deliver(&ctx, &m, &r, &content, &route).await;
                    let _ = tx.send((m, verdict));
                });
            }
        }
        while running.join_next().await.is_some() {}
        drop(tx);
        if let Err(e) = flusher.await {
            tracing::error!(error = %e, "the outcome flusher stopped");
        }
        Ok(total)
    }

    /// Everything one campaign's messages need this tick, or why they wait.
    async fn content(
        &self,
        campaign_id: Uuid,
        ws: Uuid,
        defs: &mut HashMap<Uuid, Arc<Vec<Definition>>>,
        domains: &mut Domains,
    ) -> Result<Content, Hold> {
        let db = &self.ctx.db;
        let wait = |what: &str, e: &dyn std::fmt::Debug| {
            tracing::warn!(campaign = %campaign_id, error = ?e, "{what}");
            Hold::Wait(what.to_owned())
        };
        // Which rendition is current is the database's to say, every tick. The state map's copy of
        // `frozen_at` lags by up to a refresh (more when one fails), and a resume that re-froze in
        // that window would leave the old rendition in use, which fails `frozen_usable` and pauses
        // the campaign the operator just resumed. One primary-key read: the document itself comes
        // back only when it is not the one already held.
        let held = self
            .frozen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&campaign_id)
            .cloned();
        let (when, column): (Option<DateTime<Utc>>, Option<Value>) = sqlx::query_as(
            "select frozen_at, case when frozen_at is distinct from $3 then frozen end
               from campaigns where id = $1 and workspace_id = $2",
        )
        .bind(campaign_id)
        .bind(ws)
        .bind(held.as_ref().and_then(|(at, _)| *at))
        .fetch_one(db)
        .await
        .map_err(|e| wait("the campaign could not be read", &e))?;
        let frozen = match (column, held) {
            (Some(column), _) => {
                let f = Arc::new(Frozen::from_column(&column).map_err(|e| {
                    Hold::Pause(format!(
                        "its frozen content cannot be read ({e}); cancel it"
                    ))
                })?);
                self.frozen
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(campaign_id, (when, Arc::clone(&f)));
                f
            }
            (None, Some((at, f))) if at == when => f,
            (None, _) => {
                return Err(Hold::Pause(
                    "the campaign is sending with no frozen content; cancel it".to_owned(),
                ));
            }
        };
        let defs = match defs.get(&ws) {
            Some(d) => Arc::clone(d),
            None => {
                let d = Arc::new(
                    definitions_for(db, ws)
                        .await
                        .map_err(|e| wait("the custom fields could not be read", &e))?,
                );
                defs.insert(ws, Arc::clone(&d));
                d
            }
        };
        frozen_usable(&frozen, &defs).map_err(|r| Hold::Pause(r.to_string()))?;
        // Keyed with the workspace, as the read it saves is scoped: a hit is as tenant-safe as it.
        let domain_key = (ws, frozen.sending_domain_id);
        let (domain, key) = match domains.get(&domain_key) {
            Some((d, k)) => (Arc::clone(d), k.clone()),
            None => {
                let domain = sending_domains::by_id(db, ws, frozen.sending_domain_id)
                    .await
                    .map_err(|e| wait("the sending domain could not be read", &e))?
                    .ok_or_else(|| {
                        Hold::Pause(format!(
                            "its sending domain {} was deleted; add it back and resume, or \
                             cancel this campaign",
                            frozen.from.rsplit('@').next().unwrap_or_default()
                        ))
                    })?;
                sending_domains::gate(&domain).map_err(Hold::Pause)?;
                let key = match signing_key(&self.ctx, &domain).await {
                    Ok(key) => key.map(Arc::new),
                    Err(Outcome::Transient(reason)) => return Err(Hold::Wait(reason)),
                    Err(other) => return Err(Hold::Pause(reason_of(other))),
                };
                let domain = Arc::new(domain);
                domains.insert(domain_key, (Arc::clone(&domain), key.clone()));
                (domain, key)
            }
        };
        Ok(Content {
            frozen,
            defs,
            domain,
            key,
        })
    }
}

fn reason_of(o: Outcome) -> String {
    match o {
        Outcome::Sent { response } => response,
        Outcome::Transient(r) | Outcome::Failed(r) | Outcome::Bounced(r) | Outcome::Unknown(r) => r,
        Outcome::Deferred { reason, .. } => reason,
    }
}

/// Hands `held` back unattempted. An error is logged: the claim's lease brings them back anyway.
async fn release(db: &PgPool, held: &[Claimed], wait: Duration, reason: &str) {
    if held.is_empty() {
        return;
    }
    if let Err(e) = queue::release(db, held, wait, reason).await {
        tracing::error!(error = %e, "claimed messages could not be released");
    }
}

/// One message, from the write-ahead to the relay's answer.
async fn deliver(ctx: &Ctx, m: &Claimed, r: &Recipient, c: &Content, route: &Route) -> Verdict {
    let Some(v) = usize::try_from(m.variant)
        .ok()
        .and_then(|i| c.frozen.variants.get(i))
    else {
        return Verdict::Failed(format!(
            "variant {} is not in the frozen content",
            m.variant
        ));
    };
    // The link prefix the rendition was frozen with, so the header and the body carry one URL.
    let Some(base) = v.rendition.unsubscribe_base() else {
        return Verdict::Failed("the frozen rendition has no unsubscribe link".to_owned());
    };
    let token = token::issue(
        &ctx.secrets,
        &Token {
            workspace_id: m.workspace_id,
            contact_id: m.contact_id,
            purpose: Purpose::Unsubscribe,
            list_id: None,
            issued_at: Utc::now().timestamp(),
            // `consent_log.token_jti`: which message's link was followed.
            nonce: m.id.to_string(),
            kid: None,
            ref_id: Some(m.campaign_id),
        },
    );
    let unsubscribe_url: url::Url = match format!("{base}{token}").parse() {
        Ok(url) => url,
        Err(e) => return Verdict::Failed(format!("the unsubscribe link is not a URL: {e}")),
    };
    let contact = Contact {
        email: &r.email,
        first_name: &r.first_name,
        last_name: &r.last_name,
        timezone: r.timezone.as_deref(),
        attributes: &r.attributes,
        unsubscribe_token: &token,
    };
    let rendered = match v
        .rendition
        .ready(v.rendition.fingerprint(), &c.defs)
        .and_then(|ready| ready.render(&contact))
    {
        Ok(rendered) => rendered,
        Err(e) => return Verdict::Failed(format!("the message could not be rendered: {e}")),
    };
    let message = match mail::build(&Outgoing {
        from: &c.frozen.from,
        from_name: v.from_name.as_deref(),
        to: &r.email,
        subject: &render_subject(&v.subject, &contact),
        text: &rendered.text,
        html: Some(&rendered.html),
        unsubscribe_url: &unsubscribe_url,
        feedback_id: &format!("{}:{}:{}:nm", m.campaign_id, m.variant, m.workspace_id),
        message_id_domain: &c.domain.domain,
        reply_to: c.frozen.reply_to.as_deref(),
        message_id: m.id,
    }) {
        Ok(message) => message,
        Err(e) => return Verdict::Failed(format!("the message could not be built: {e}")),
    };
    let bytes = match signed(&c.domain, c.key.as_deref(), &message).await {
        Ok(bytes) => bytes,
        Err(outcome) => return verdict(outcome, None, &r.email),
    };
    let envelope = match (c.frozen.from.parse(), r.email.parse()) {
        (Ok(from), Ok(to)) => match Envelope::new(Some(from), vec![to]) {
            Ok(envelope) => envelope,
            Err(e) => return Verdict::Failed(format!("the envelope could not be built: {e}")),
        },
        _ => return Verdict::Failed("the envelope's addresses could not be parsed".to_owned()),
    };
    let (outcome, relay) = submit(
        ctx,
        m.workspace_id,
        &route.relays,
        route.paid_for,
        &route.creds,
        &envelope,
        &bytes,
    )
    .await;
    verdict(outcome, relay, &r.email)
}

/// Spec §4.7.4's table. `email` is the recipient's, which a bounce suppresses.
fn verdict(o: Outcome, relay: Option<Uuid>, email: &str) -> Verdict {
    match (o, relay) {
        (Outcome::Sent { response }, Some(relay)) => Verdict::Sent { relay, response },
        // `submit` names the relay of every decided outcome; were it ever missing, a delivery
        // is still not to be retried.
        (Outcome::Sent { response }, None) => Verdict::Unknown(response),
        (Outcome::Transient(r), _) => Verdict::Retry(r),
        (Outcome::Deferred { reason, .. }, _) => Verdict::Deferred(reason),
        (Outcome::Failed(r), _) => Verdict::Failed(r),
        (Outcome::Bounced(reason), _) => Verdict::Bounced {
            reason,
            email: email.to_owned(),
        },
        (Outcome::Unknown(r), _) => Verdict::Unknown(r),
    }
}

/// Records outcomes as they arrive: every [`FLUSH_EVERY`], or at [`FLUSH_AT`], and whatever is left
/// when the tick closes the channel. A flush that fails is kept and tried again: the claim token
/// makes a replay touch nothing another claim holds.
async fn flush(
    db: PgPool,
    secrets: Arc<Secrets>,
    mut rx: mpsc::UnboundedReceiver<(Claimed, Verdict)>,
) {
    let mut held: Vec<(Claimed, Verdict)> = Vec::new();
    let mut every = tokio::time::interval(FLUSH_EVERY);
    loop {
        tokio::select! {
            got = rx.recv() => match got {
                Some(outcome) => {
                    held.push(outcome);
                    if held.len() >= FLUSH_AT {
                        record(&db, &secrets, &mut held).await;
                    }
                }
                None => break,
            },
            _ = every.tick() => record(&db, &secrets, &mut held).await,
        }
    }
    for _ in 0..3 {
        if held.is_empty() {
            return;
        }
        record(&db, &secrets, &mut held).await;
        if !held.is_empty() {
            tokio::time::sleep(FLUSH_EVERY).await;
        }
    }
    if !held.is_empty() {
        // The sweep records each `submitting` one `unknown` once its lease runs out; a skipped
        // one is claimed again and skipped again.
        tracing::error!(
            outcomes = held.len(),
            "outcomes could not be recorded; the sweep will"
        );
    }
}

async fn record(db: &PgPool, secrets: &Secrets, held: &mut Vec<(Claimed, Verdict)>) {
    if held.is_empty() {
        return;
    }
    match queue::record(db, secrets, held).await {
        Ok(()) => held.clear(),
        Err(e) => tracing::warn!(error = %e, "a flush of outcomes failed; it will be retried"),
    }
}
