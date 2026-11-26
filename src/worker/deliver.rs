// SPDX-License-Identifier: AGPL-3.0-only

//! Signing and submitting one message, shared by every sender: confirmations, test sends and
//! campaign messages. Moved out of `confirmation` unchanged but for four things: the relays'
//! passwords come from a [`Creds`] read once per tick (spec M2c §4.7.9), [`submit`] says which relay
//! decided, a [`Outcome::Deferred`] drains that relay's bucket before the next one is tried, and a
//! relay skipped for its rate limit or its credential reads as `Deferred` rather than `Transient`
//! (it is the relay's trouble, so a campaign message spends no attempt on it). The outcome
//! [`submit`] returns has the recipient's address taken out of its text, because every sender
//! stores that text.
//!
//! **The rule [`submit`] keeps: a send is covered by a token taken from the relay it goes through.**
//! The tick spends on the first relay with tokens, while `submit` sends through the first relay
//! that is reachable. With relay 1 exhausted and relay 2 funded, a debit of relay 2 followed by a
//! send through relay 1 drives relay 1 past its limit without bound. So falling through to a relay
//! the tick did not pay for takes a token for that relay first, and a relay that will not grant
//! one is skipped however reachable it is.

use std::collections::HashMap;

use lettre::{Message, address::Envelope};
use uuid::Uuid;

use super::confirmation::Ctx;
use crate::{
    mail::dkim,
    relay::{
        self, Relay, bucket,
        transport::{DEFER_COOL_DOWN, Outcome},
    },
    sending_domains::{SendingDomain, Signing},
};

/// The relays' passwords, read once per tick. A relay whose password could not be read has no
/// entry and is skipped by [`submit`]. No `Debug`: these are secrets.
pub struct Creds(HashMap<Uuid, Option<String>>);

impl Creds {
    /// Each relay's password, decrypted once. A relay deleted since it was listed, or whose
    /// password cannot be decrypted, is left out and logged by id, never by value.
    pub async fn load(ctx: &Ctx, workspace_id: Uuid, relays: &[Relay]) -> Creds {
        let mut map = HashMap::with_capacity(relays.len());
        for r in relays {
            match relay::password(&ctx.db, &ctx.secrets, workspace_id, r.id).await {
                Ok(password) => {
                    map.insert(r.id, password);
                }
                Err(e) => {
                    tracing::warn!(relay = %r.id, error = ?e, "a relay's password could not be read")
                }
            }
        }
        Creds(map)
    }
}

/// Relay-level trouble that is not the message's: nothing was submitted, so it spends no attempt.
fn deferred(reason: String) -> Outcome {
    Outcome::Deferred {
        reason,
        cool_down: DEFER_COOL_DOWN,
    }
}

/// Offers the message to each relay in failover order until one of them answers decisively, and
/// **takes a token for every relay but the one the tick already paid for**. Returns the outcome and
/// the relay that decided it, `None` when none did.
///
/// **[`Outcome::Transient`] and [`Outcome::Deferred`] fall through to the next relay; nothing else
/// does.** A `Failed` is the message being refused and the next relay would refuse it too; an
/// `Unknown` may already have been queued, and offering it to a second relay is the double send the
/// whole classification exists to prevent. A `Deferred` relay's bucket is drained first, so neither
/// this tick nor another worker offers it more until its cool-down has passed.
///
/// A relay skipped for its rate limit or its credential (deleted mid-tick included) is relay-level
/// trouble, so it reads as `Deferred`. **A `Transient` from any relay outranks every later
/// `Deferred`**, a skip or a relay's own `455` alike: the message was refused once on its own
/// account, and that spends the attempt it should.
///
/// **The outcome's text never holds the recipient's address** ([`redacted`]): every sender stores
/// it, and this is the one place all three pass through.
pub async fn submit(
    ctx: &Ctx,
    workspace_id: Uuid,
    relays: &[Relay],
    paid_for: Option<Uuid>,
    creds: &Creds,
    envelope: &Envelope,
    bytes: &[u8],
) -> (Outcome, Option<Uuid>) {
    let mut last = deferred("no relay would take the message".to_owned());
    // The one preference: a message-level `Transient` already seen is kept over relay-level news.
    let prefer = |last: Outcome, next: Outcome| match last {
        transient @ Outcome::Transient(_) => transient,
        _ => next,
    };
    for relay in relays {
        // The pool, never a transaction (ADR 0005): one statement.
        if paid_for != Some(relay.id) {
            match bucket::take(
                &ctx.db,
                workspace_id,
                relay.id,
                relay.rate_limit_per_minute,
                1,
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => {
                    last = prefer(
                        last,
                        deferred(format!(
                            "relay {} is at its rate limit for this minute",
                            relay.name
                        )),
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        relay = %relay.id,
                        error = %e,
                        "the relay bucket could not be debited"
                    );
                    last = prefer(
                        last,
                        deferred(format!(
                            "relay {}'s rate limit could not be read",
                            relay.name
                        )),
                    );
                    continue;
                }
            }
        }
        let Some(password) = creds.0.get(&relay.id) else {
            last = prefer(
                last,
                deferred(format!("relay {}'s password could not be read", relay.name)),
            );
            continue;
        };
        match ctx
            .transports
            .send(relay, password.as_deref(), envelope, bytes)
            .await
        {
            Outcome::Transient(reason) => last = Outcome::Transient(reason),
            Outcome::Deferred { reason, cool_down } => {
                if let Err(e) = bucket::drain(&ctx.db, relay.id, cool_down).await {
                    tracing::warn!(relay = %relay.id, error = %e, "a deferring relay could not be drained");
                }
                last = prefer(last, Outcome::Deferred { reason, cool_down });
            }
            decided => return (redacted(decided, envelope), Some(relay.id)),
        }
    }
    (redacted(last, envelope), None)
}

/// What stands for the recipient's address in a stored reply.
const RECIPIENT: &str = "<recipient>";

/// `o` with each of the envelope's recipients taken out of its text. A relay's reply is stored
/// (`messages.error` and `messages.response`, a job's `last_error`), and one such as `550 5.1.1
/// <bob@example.com>: Recipient address rejected` would keep the address after an erasure that
/// leaves only a uuid (spec M2c §4.2 ruling 5): `messages` has no foreign key to `contacts`.
fn redacted(o: Outcome, envelope: &Envelope) -> Outcome {
    let r = |text: String| {
        envelope.to().iter().fold(text, |text, to| {
            let bracketed = replace_ignoring_case(&text, &format!("<{to}>"), RECIPIENT);
            replace_ignoring_case(&bracketed, to.as_ref(), RECIPIENT)
        })
    };
    match o {
        Outcome::Sent { response } => Outcome::Sent {
            response: r(response),
        },
        Outcome::Transient(t) => Outcome::Transient(r(t)),
        Outcome::Failed(t) => Outcome::Failed(r(t)),
        Outcome::Unknown(t) => Outcome::Unknown(r(t)),
        Outcome::Deferred { reason, cool_down } => Outcome::Deferred {
            reason: r(reason),
            cool_down,
        },
    }
}

/// `text` with every copy of `needle`, matched without regard to ASCII case, replaced by `with`.
fn replace_ignoring_case(text: &str, needle: &str, with: &str) -> String {
    if needle.is_empty() {
        return text.to_owned();
    }
    // ASCII lowercasing changes no byte offset, so a match in `lower` is one in `text`.
    let (lower, needle) = (text.to_ascii_lowercase(), needle.to_ascii_lowercase());
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(i) = lower[at..].find(&needle) {
        out.push_str(&text[at..at + i]);
        out.push_str(with);
        at += i + needle.len();
    }
    out.push_str(&text[at..]);
    out
}

/// The domain's DKIM key, read and parsed once: `None` when the relay signs. `Err` is the outcome
/// to record instead.
pub async fn signing_key(
    ctx: &Ctx,
    domain: &SendingDomain,
) -> Result<Option<dkim::SigningKey>, Outcome> {
    if domain.signing == Signing::Relay {
        return Ok(None);
    }
    let der = match crate::sending_domains::dkim_private_key(
        &ctx.db,
        &ctx.secrets,
        domain.workspace_id,
        domain.id,
    )
    .await
    {
        // Wiped on the way out: this is a private key, and it is a copy `dkim_private_key` hands
        // over as a plain `Vec` for its caller to look after.
        Ok(Some(der)) => zeroize::Zeroizing::new(der),
        // `sending_domains_app_signing_has_key` makes this unwritable through the API; it is the
        // backstop, and it must not be a send of unsigned mail from a domain that promised a
        // signature.
        Ok(None) => {
            return Err(Outcome::Failed(format!(
                "{} is app-signed and has no DKIM key",
                domain.domain
            )));
        }
        Err(e) => {
            tracing::warn!(error = ?e, "the DKIM key could not be read");
            return Err(Outcome::Transient(
                "the DKIM key could not be read".to_owned(),
            ));
        }
    };
    dkim::key_from_der(&der)
        .map(Some)
        .map_err(|e| Outcome::Failed(format!("the stored DKIM key cannot sign: {e}")))
}

/// The message as it goes on the wire: DKIM-signed by this product when the domain says so, and
/// lettre's own bytes when the relay signs. `key` is [`signing_key`]'s answer for `domain`.
pub async fn signed(
    domain: &SendingDomain,
    key: Option<&dkim::SigningKey>,
    m: &Message,
) -> Result<Vec<u8>, Outcome> {
    if domain.signing == Signing::Relay {
        return Ok(m.formatted());
    }
    let Some(key) = key else {
        return Err(Outcome::Failed(format!(
            "{} is app-signed and has no DKIM key",
            domain.domain
        )));
    };
    dkim::signed_bytes(m, key, &domain.domain, &domain.dkim_selector)
        .map_err(|e| Outcome::Failed(format!("the message could not be signed: {e}")))
}
