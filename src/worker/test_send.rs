// SPDX-License-Identifier: AGPL-3.0-only

//! The `campaign_test` handler (spec M2c §4.8): one test copy of a campaign, already rendered at
//! request time with the preview's sample contact, delivered to one explicit address.
//!
//! The steps are the confirmation handler's, in its order, read from the payload: parse, then
//! **the suppression check before anything is built** (a test honours suppressions, though not
//! consent or audience, and the API has already answered `202`, so a refusal is recorded on the
//! job with its reason), then the sending domain and its gate, the relays, build, sign, submit.
//!
//! The unsubscribe link is `{public_url}/u/test`, which answers "no longer valid": the parts were
//! rendered with that token, and the header carries the same URL. `Feedback-ID` starts `test:` so
//! a test never counts as the campaign.

use lettre::address::Envelope;
use serde::Deserialize;
use uuid::Uuid;

use super::confirmation::Ctx;
use crate::{
    mail::{self, Outgoing},
    relay::{self, transport::Outcome},
    sending_domains::{self, not_passing},
    suppressions::blocking_suppression,
    users::normalize_email,
    worker::{
        Claimed, complete, defer,
        deliver::{Creds, signed, signing_key, submit},
        fail,
    },
};

pub const TEST_KIND: &str = "campaign_test";

/// What `campaign::routes::test_campaign` queues.
#[derive(Deserialize)]
struct Payload {
    to: String,
    subject: String,
    html: String,
    text: String,
    from: String,
    from_name: Option<String>,
    reply_to: Option<String>,
    sending_domain_id: Uuid,
}

/// Sends one claimed `campaign_test` job and records what became of it on the row, exactly as
/// `confirmation::handle` records its own.
pub async fn handle(ctx: &Ctx, job: &Claimed, paid_for: Option<Uuid>) -> Outcome {
    let outcome = attempt(ctx, job, paid_for).await;
    let recorded = match &outcome {
        Outcome::Sent { .. } => complete(&ctx.db, job.id).await,
        Outcome::Transient(reason) | Outcome::Deferred { reason, .. } => {
            defer(&ctx.db, job, reason).await
        }
        Outcome::Failed(reason) | Outcome::Unknown(reason) => fail(&ctx.db, job.id, reason).await,
    };
    if let Err(e) = recorded {
        tracing::error!(job = %job.id, error = %e, "a test send's outcome could not be recorded");
    }
    outcome
}

fn db_error(what: &str, e: &impl std::fmt::Debug) -> Outcome {
    tracing::warn!(error = ?e, "{what}");
    Outcome::Transient(what.to_owned())
}

async fn attempt(ctx: &Ctx, job: &Claimed, paid_for: Option<Uuid>) -> Outcome {
    let p: Payload = match serde_json::from_value(job.payload.clone()) {
        Ok(p) => p,
        Err(e) => return Outcome::Failed(format!("the payload is not a test send: {e}")),
    };
    // A column, not payload (`jobs_campaign_shape`); read as the launch reads it.
    let campaign_id: Option<Uuid> = match sqlx::query_scalar(
        "select campaign_id from jobs where id = $1 and workspace_id = $2",
    )
    .bind(job.id)
    .bind(job.workspace_id)
    .fetch_one(&ctx.db)
    .await
    {
        Ok(id) => id,
        Err(e) => return db_error("the test send's campaign could not be read", &e),
    };
    let Some(campaign_id) = campaign_id else {
        return Outcome::Failed("a campaign_test must name its campaign".to_owned());
    };
    let Ok(to) = normalize_email(&p.to) else {
        return Outcome::Failed("the test address is not a usable email address".to_owned());
    };

    match blocking_suppression(&ctx.db, &ctx.secrets, job.workspace_id, &to).await {
        // The address is not quoted: an erasure tombstone exists because the plaintext was
        // destroyed, and `last_error` is stored and shown.
        Ok(Some(block)) => {
            return Outcome::Failed(format!(
                "the address is suppressed: {:?}, {:?} scope, {:?}-wide",
                block.reason, block.scope, block.level
            ));
        }
        Ok(None) => {}
        Err(e) => return db_error("the suppression check failed", &e),
    }

    let domain = match sending_domains::by_id(&ctx.db, job.workspace_id, p.sending_domain_id).await
    {
        Ok(Some(domain)) => domain,
        Ok(None) => {
            return Outcome::Failed("the campaign's sending domain no longer exists".to_owned());
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
        return Outcome::Transient("the workspace has no enabled relay".to_owned());
    }

    let base = ctx.config.public_url.as_str().trim_end_matches('/');
    let unsubscribe_url: url::Url = match format!("{base}/u/test").parse() {
        Ok(url) => url,
        Err(e) => return Outcome::Failed(format!("public_url does not make a usable link: {e}")),
    };
    let message = match mail::build(&Outgoing {
        from: &p.from,
        from_name: p.from_name.as_deref(),
        to: &to,
        subject: &p.subject,
        text: &p.text,
        html: Some(&p.html),
        unsubscribe_url: &unsubscribe_url,
        feedback_id: &format!("test:{campaign_id}:{}:nm", job.workspace_id),
        message_id_domain: &domain.domain,
        reply_to: p.reply_to.as_deref(),
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
    let envelope = match (p.from.parse(), to.parse()) {
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
