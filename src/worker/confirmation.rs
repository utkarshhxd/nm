// SPDX-License-Identifier: AGPL-3.0-only

//! The `confirmation_email` handler: what one claimed row becomes between the queue and a relay.
//!
//! The order of the steps is the design, so it is written down here rather than left to be read
//! off the code. Each refusal comes before the work it would have wasted, and each one is
//! recorded on the row with a reason, because `failed_at` with no `last_error` is what makes
//! "why did this confirmation never arrive?" unanswerable six months later.
//!
//! 1. **Parse the payload.** A payload this build cannot read fails *that row*. It never panics:
//!    one malformed row must not take the other fifteen in the tick with it.
//! 2. **Staleness.** `Purpose::Confirm` is good for fourteen days, the token is stamped at
//!    enqueue and nothing re-issues it, so a row older than that carries a link that is already
//!    dead. Mailing it sends someone to a `link-expired` page, which is worse than sending
//!    nothing. The window is read from [`Purpose::ttl_secs`] and is **not** restated here.
//! 3. **Suppression, at send time.** [`blocking_suppression`], never `mailable_list_members`: the
//!    view matches plaintext and is therefore blind to a GDPR erasure tombstone, which keeps a
//!    hash and no plaintext (spec §5.7). A confirmation to an erased address is the exact failure
//!    M1d's erasure work exists to prevent.
//! 4. **The sending domain**, then the relays in failover order.
//! 5. **The unverified-domain gate.** A domain whose DNS checks are not all passing is not sent
//!    from unless an admin has acknowledged it. The settings screen already tells operators that
//!    a send will not happen until the box is ticked; this is what makes that sentence true,
//!    because a tick-box the send path never reads is decoration.
//! 6. **Build, sign, submit.**
//! 7. **Record**: `Sent` completes, `Transient` defers with backoff, `Failed` and `Unknown` give
//!    up. **`Unknown` is never retried** (spec §5.8): a second copy of a confirmation email is
//!    indistinguishable from the product being broken.
//!
//! **The unsubscribe URL is derived here and nowhere else.** [`crate::mail::build`] enforces only
//! that it is `https`; *whose* host it names is this module's decision alone. Spec §5.5's
//! guarantee — that a click can never land on a relay's suppression list instead of our
//! `consent_log` — rests on this one function reading [`Config::public_url`] and issuing the
//! token with M1d's own machinery. OCI Email Delivery adds a one-click header of its own when it
//! finds no HTTPS one, and a click on *that* link suppresses the person in Oracle's list and
//! nowhere in ours: no `consent_log` row, nothing a preference centre or a GDPR export can see.
//!
//! **The bucket is debited once per claimed batch (spec §5.4), by the tick, and this handler is
//! told which relay that debit bought.** `handle` takes the relay the tick paid for and
//! [`submit`] sends through it without spending again.
//!
//! **What this handler does own is the token for a relay it falls over to.** The rule the whole
//! bucket exists for is that *a send is covered by a token taken from the relay it goes through*,
//! and failover is the one place those two can come apart: the tick spends on the first relay
//! with tokens, while `submit` sends through the first relay that is reachable. With relay 1
//! exhausted and relay 2 funded, a debit of relay 2 followed by a send through relay 1 drives
//! relay 1 past its limit without bound — the bucket stops meaning anything in exactly the
//! configuration it exists for. So falling through to a relay the tick did not pay for takes a
//! token for that relay first, and a relay that will not grant one is skipped however reachable
//! it is. That is what a rate limit is.

use std::sync::Arc;

use chrono::Utc;
use lettre::address::Envelope;
use serde::Deserialize;
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

use crate::{
    config::Config,
    crypto::secrets::Secrets,
    error::{ApiError, ApiResult},
    mail::{self, Outgoing},
    public::token::{self, Purpose, Token},
    relay::{
        self,
        transport::{Outcome, Transports},
    },
    sending_domains::{SendingDomain, not_passing},
    suppressions::blocking_suppression,
    users::normalize_email,
    worker::{
        Claimed, complete, defer,
        deliver::{Creds, signed, signing_key, submit},
        fail,
    },
};

/// Everything one message needs that is not on the job row. Task 11's tick builds one and shares
/// it across the sends in a tick, so each field is behind an `Arc` the tick already holds.
pub struct Ctx {
    pub db: PgPool,
    pub secrets: Arc<Secrets>,
    pub config: Arc<Config>,
    pub transports: Arc<Transports>,
}

/// The mailbox a confirmation comes from, on the workspace's sending domain.
///
/// shortcut: one fixed local part for every workspace, because nothing stores a from-address yet;
/// upgrade when M2c gives a campaign one of its own. It is a `no-reply` because M2a has no inbound
/// path at all — no bounce handling, no reply handling — and a mailbox that silently swallows
/// replies is worse than one that says it does not take them.
const FROM_LOCAL_PART: &str = "no-reply";

const SUBJECT: &str = "Please confirm your subscription";

/// What `public::forms` queues, as `comment on table jobs` states it: the address the form was
/// submitted with, and the already-issued confirm token, carried verbatim.
#[derive(Deserialize)]
struct Payload {
    email: String,
    token: String,
}

/// Sends one claimed `confirmation_email`, and records what became of it on the row.
///
/// `paid_for` is the relay the tick has already debited a token on for this message, so this
/// message may go through it without spending again. `None` means nothing was paid for and every
/// relay tried takes a token first.
///
/// Returns the outcome as well, for the tick's own accounting. It never panics: every refusal is
/// an [`Outcome`], so one unreadable row cannot take the rest of the batch with it.
pub async fn handle(ctx: &Ctx, job: &Claimed, paid_for: Option<Uuid>) -> Outcome {
    let outcome = attempt(ctx, job, paid_for).await;
    let recorded = match &outcome {
        Outcome::Sent { .. } => complete(&ctx.db, job.id).await,
        // A relay's own limit is retried as any 4xx is here: M2c's no-attempt deferral is the
        // campaign queue's, and this path keeps M2a's backoff (spec M2c §9).
        Outcome::Transient(reason) | Outcome::Deferred { reason, .. } => {
            defer(&ctx.db, job, reason).await
        }
        // Both give up, and `Unknown` gives up precisely because it might have worked.
        Outcome::Failed(reason) | Outcome::Unknown(reason) => fail(&ctx.db, job.id, reason).await,
    };
    if let Err(e) = recorded {
        // Nothing else can be done from here, and silence would be the worst of it: a `complete`
        // that did not land means this row is claimed again when its lease expires, and the
        // message goes twice.
        tracing::error!(
            job = %job.id,
            error = %e,
            "a confirmation email's outcome could not be recorded; the row may be claimed again"
        );
    }
    outcome
}

/// Everything but the recording. Each step returns the outcome to record rather than an error,
/// so the difference between "never offer this again" and "offer it again later" is made where
/// the reason is known.
///
/// **A database error is [`Outcome::Transient`], never [`Outcome::Failed`].** `Failed` destroys a
/// message; a connection that dropped while a lookup was in flight says nothing about whether the
/// message should ever be sent.
async fn attempt(ctx: &Ctx, job: &Claimed, paid_for: Option<Uuid>) -> Outcome {
    let payload: Payload = match serde_json::from_value(job.payload.clone()) {
        Ok(payload) => payload,
        Err(e) => return Outcome::Failed(format!("the payload is not {{email, token}}: {e}")),
    };
    // `jobs_confirmation_email_shape` makes both of these non-null for this kind, so this is the
    // backstop for a writer that is not `public::forms` rather than an expected path.
    let (Some(contact_id), Some(list_id)) = (job.contact_id, job.list_id) else {
        return Outcome::Failed(
            "a confirmation_email must name a contact and a list (jobs_confirmation_email_shape)"
                .to_owned(),
        );
    };

    if let Some(ttl) = Purpose::Confirm.ttl_secs() {
        let age = Utc::now()
            .signed_duration_since(job.created_at)
            .num_seconds();
        if age > ttl {
            return Outcome::Failed(format!(
                "queued {age} s ago, past the {ttl} s a confirmation link is good for: mailing it \
                 would lead to link-expired"
            ));
        }
    }

    // Normalised here rather than left to `blocking_suppression`, which treats an address it
    // cannot read as an `Internal` error — and a database error is retried. An address that does
    // not normalise today will not normalise on the fifth attempt either, so it gives up now
    // instead of burning `max_attempts` on something that cannot improve. The normalised form is
    // then what is checked AND what is mailed, which is the form `contacts` stores and every
    // other path in the product sends to.
    let Ok(email) = normalize_email(&payload.email) else {
        return Outcome::Failed(
            "the queued address is not a usable email address, so it cannot be checked or mailed"
                .to_owned(),
        );
    };

    match blocking_suppression(&ctx.db, &ctx.secrets, job.workspace_id, &email).await {
        // The address is deliberately not quoted: an erasure tombstone exists because the
        // plaintext was destroyed, and `last_error` is stored and shown.
        Ok(Some(block)) => {
            return Outcome::Failed(format!(
                "the address is suppressed: {:?}, {:?} scope, {:?}-wide",
                block.reason, block.scope, block.level
            ));
        }
        Ok(None) => {}
        Err(e) => return db_error("the suppression check failed", &e),
    }

    let domain = match sending_domain(&ctx.db, job.workspace_id).await {
        Ok(Some(domain)) => domain,
        // A workspace with no sending domain is a configuration error that will not fix itself:
        // there is no from-address to build a message from, and no retry produces one.
        Ok(None) => {
            return Outcome::Failed("the workspace has no sending domain to send from".to_owned());
        }
        Err(e) => return db_error("the sending domain could not be read", &e),
    };

    let unverified = not_passing(&domain);
    if !unverified.is_empty() && domain.send_unverified_acknowledged_at.is_none() {
        return Outcome::Failed(format!(
            "{} has not passed its {} DNS check{}, and no admin has acknowledged sending from it \
             anyway",
            domain.domain,
            unverified.join(", "),
            if unverified.len() == 1 { "" } else { "s" },
        ));
    }

    let relays = match relay::enabled_relays(&ctx.db, job.workspace_id).await {
        Ok(relays) => relays,
        Err(e) => return db_error("the relays could not be read", &e),
    };
    if relays.is_empty() {
        // `Transient`, where a missing sending domain is `Failed`, and the difference is what an
        // operator is plausibly in the middle of: a relay is disabled and re-enabled in the course
        // of ordinary maintenance, and a confirmation that waited a minute for that is better than
        // one destroyed by it. It does not wait for ever — `defer` fails the row once `attempts`
        // reaches `max_attempts`, so a workspace that genuinely has no relay still ends with a row
        // that says so.
        return Outcome::Transient("the workspace has no enabled relay".to_owned());
    }

    let from = format!("{FROM_LOCAL_PART}@{}", domain.domain);
    let unsubscribe_url = match unsubscribe_url(ctx, job.workspace_id, contact_id) {
        Ok(url) => url,
        Err(reason) => return Outcome::Failed(reason),
    };
    let message = match mail::build(&Outgoing {
        from: &from,
        from_name: domain.from_name.as_deref(),
        to: &email,
        subject: SUBJECT,
        text: &text_body(ctx, &payload.token, &unsubscribe_url),
        html: None,
        unsubscribe_url: &unsubscribe_url,
        feedback_id: &format!("confirmation:{list_id}:{}:nm", job.workspace_id),
        message_id_domain: &domain.domain,
        reply_to: None,
        message_id: Uuid::now_v7(),
    }) {
        Ok(message) => message,
        Err(e) => return Outcome::Failed(format!("the message could not be built: {e}")),
    };
    let key = match signing_key(ctx, &domain).await {
        Ok(key) => key,
        Err(outcome) => return outcome,
    };
    let bytes = match signed(&domain, key.as_ref(), &message).await {
        Ok(bytes) => bytes,
        Err(outcome) => return outcome,
    };
    // Both addresses were parsed once already by `mail::build`; this is lettre's envelope, which
    // takes them again.
    let envelope = match (from.parse(), email.parse()) {
        (Ok(from), Ok(to)) => match Envelope::new(Some(from), vec![to]) {
            Ok(envelope) => envelope,
            Err(e) => return Outcome::Failed(format!("the envelope could not be built: {e}")),
        },
        _ => return Outcome::Failed("the envelope's addresses could not be parsed".to_owned()),
    };

    let creds = Creds::load(ctx, job.workspace_id, &relays).await;
    submit(
        ctx,
        job.workspace_id,
        &relays,
        paid_for,
        &creds,
        &envelope,
        &bytes,
    )
    .await
    .0
}

/// A lookup that failed, recorded as something to try again later.
///
/// **The error goes to the log and the bounded `what` goes on the row.** `ApiError` has no
/// `Display`, and its `Debug` carries an `anyhow::Error` that formats its backtrace when one was
/// captured: source paths from the build machine, in a `last_error` column an operator reads in a
/// browser, growing without bound. What the row needs is which lookup failed, which is `what`.
fn db_error(what: &str, e: &ApiError) -> Outcome {
    tracing::warn!(error = ?e, "{what}");
    Outcome::Transient(what.to_owned())
}

/// The workspace's sending domain.
///
/// shortcut: a workspace sends from the first of its domains by name, because nothing yet records
/// *which* domain a given message is from — `sending_domains` is a list of the domains a workspace
/// *may* send as, and M2a has no campaign to carry a choice. Ordering by `domain` rather than by
/// `created_at` keeps the answer stable across a re-seeded database, and the unique index
/// `(workspace_id, domain)` serves it.
///
/// **What that costs, which is the part worth knowing before you add a domain:** adding a second
/// sending domain that sorts earlier silently moves every confirmation's `From`, `Message-ID` and
/// DKIM signature onto it, with nothing in the product saying so and no operator action that
/// chose it. A workspace with one domain — which is every workspace M2a is built for — never
/// meets this. `issues/41` has it; upgrade when M2c gives a campaign its own from-address.
async fn sending_domain(
    db: impl PgExecutor<'_> + Copy,
    workspace_id: Uuid,
) -> ApiResult<Option<SendingDomain>> {
    // Not the `query!` macro: `sending_domains` arrived in `0028` and the offline query cache is
    // built against a database that predates it.
    let domain: Option<String> = sqlx::query_scalar(
        "select domain from sending_domains where workspace_id = $1 order by domain limit 1",
    )
    .bind(workspace_id)
    .fetch_optional(db)
    .await?;
    match domain {
        Some(domain) => crate::sending_domains::for_domain(db, workspace_id, &domain).await,
        None => Ok(None),
    }
}

/// **The one place an unsubscribe URL comes from.** The origin is this instance's `public_url`
/// and the token is M1d's, issued here for this contact: an `Unsubscribe` token carries no list,
/// which `public::token::parse` requires, and `/u/{token}` is both the RFC 8058 one-click endpoint
/// and the preference centre a person opens.
fn unsubscribe_url(ctx: &Ctx, workspace_id: Uuid, contact_id: Uuid) -> Result<url::Url, String> {
    let token = token::issue(
        &ctx.secrets,
        &Token {
            workspace_id,
            contact_id,
            purpose: Purpose::Unsubscribe,
            list_id: None,
            issued_at: Utc::now().timestamp(),
            nonce: Uuid::now_v7().to_string(),
            // One key and nothing to attribute this link to; both absent, so the payload is the
            // shape every link issued before those fields existed already had.
            kid: None,
            ref_id: None,
        },
    );
    link(ctx, "u", &token)
}

/// `{public_url}/{prefix}/{token}`, as `workspaces::invites` builds its own link.
fn link(ctx: &Ctx, prefix: &str, token: &str) -> Result<url::Url, String> {
    let base = ctx.config.public_url.as_str().trim_end_matches('/');
    format!("{base}/{prefix}/{token}")
        .parse()
        .map_err(|e| format!("public_url does not make a usable {prefix} link: {e}"))
}

/// The message itself. Plain text, because there is no template machinery in this milestone and a
/// confirmation has one thing to say.
fn text_body(ctx: &Ctx, confirm_token: &str, unsubscribe_url: &url::Url) -> String {
    // A confirm link that cannot be built is not worth failing the message over on its own: the
    // same `public_url` produced the unsubscribe URL a moment ago, so this branch is unreachable
    // in practice, and an empty link is visibly broken rather than silently wrong.
    let confirm = link(ctx, "c", confirm_token).map_or_else(|e| e, |url| url.to_string());
    format!(
        "Someone asked to subscribe this address to our mailing list.\r\n\
         \r\n\
         To confirm, open this link:\r\n\
         {confirm}\r\n\
         \r\n\
         If this was not you, ignore this email and nothing will be sent.\r\n\
         \r\n\
         To be removed from our mail entirely:\r\n\
         {unsubscribe_url}\r\n"
    )
}
