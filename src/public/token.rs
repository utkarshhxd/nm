// SPDX-License-Identifier: AGPL-3.0-only

//! The token a subscriber's email link carries. The public pages (confirm, unsubscribe,
//! preferences) have no session, so the link itself is the credential: signed, never encrypted, and
//! in the same shape as the selection token (`contacts::selection`) so there is one scheme to
//! audit — `base64url(json).base64url(hmac)`, keyed by a domain-separated subkey.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    crypto::{secrets::Secrets, tokens},
    error::{ApiError, ApiResult},
};

/// Domain-separation label for the public-token key: a public link can never be replayed as a
/// selection token or any other HMAC'd value, nor the reverse.
pub const PUBLIC_DOMAIN: &str = "nm:public-token:v1";

const DAY: i64 = 24 * 60 * 60;

/// What the link may do. Part of the signed payload, so a confirm link cannot be turned into an
/// unsubscribe (or the reverse) by editing it.
///
/// **There is no `Preference` variant, and adding one back would be a mistake.** It existed until
/// Task 5, with a 90-day TTL, for a preference-center door of its own. Spec §6.3 settled on one
/// page and three doors instead: `/u/{token}` is both the RFC 8058 one-click POST and the
/// preference center a human opens, so both methods parse `Unsubscribe` and there is one token to
/// issue. A second variant that no route issues and no route accepts is a signed shape waiting to
/// be given a meaning by someone who does not know why it was dropped — and a 90-day TTL on this
/// link would contradict §6.2's "unsubscribe tokens do not expire" outright. Spec §6.2 carries a
/// dated note. (2026-11-20)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    Confirm,
    Unsubscribe,
}

impl Purpose {
    /// `None` for `Unsubscribe`: RFC 8058 one-click links sit in an inbox for years, and refusing
    /// to honour one is a compliance failure, not a safety measure. The preference center is
    /// reached by the same token, so it does not expire either.
    fn ttl_secs(self) -> Option<i64> {
        match self {
            Purpose::Confirm => Some(14 * DAY),
            Purpose::Unsubscribe => None,
        }
    }
}

/// The field names are part of the token format: renaming one invalidates links already sent. No
/// `serde(default)`, `deny_unknown_fields`: a payload this build did not write is refused, not read.
///
/// **This field list is final once the first unsubscribe link is sent, and there is no mechanism
/// left to change it.** The two rules above are inherited from the selection token
/// (`m1a-decisions.md`), where they are safe because that token lives an hour: adding a field
/// there breaks nothing, and the escape hatch for a payload whose *meaning* changed is to bump
/// the domain label, invalidating every outstanding token at once. Neither works here.
/// `Purpose::Unsubscribe` has no TTL, so links sit in inboxes for years; a new field makes every
/// one of them a hard deserialize error, and bumping `PUBLIC_DOMAIN` kills them all in one go.
/// Spec §6.2 forbids exactly that: "an email sits in an inbox for years and RFC 8058 must still
/// work."
///
/// So anything a later milestone needs in this payload — a campaign id for M2's click tracking, a
/// key version for rotation — has to be added **before** the first send, not after. A field that
/// cannot be added later is not a field that can be deferred. (2026-11-20)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub workspace_id: Uuid,
    pub contact_id: Uuid,
    pub purpose: Purpose,
    /// `Some` only for `Confirm`: the list being confirmed.
    pub list_id: Option<Uuid>,
    /// Unix seconds. The age is checked in `parse`, before any database access.
    pub issued_at: i64,
    /// Makes two links issued in the same second for the same contact differ.
    pub nonce: String,
}

fn mac(secrets: &Secrets, payload: &[u8]) -> [u8; 32] {
    tokens::hmac_sha256(secrets.derive_key(PUBLIC_DOMAIN).as_slice(), payload)
}

/// `base64url(json).base64url(hmac)`.
pub fn issue(secrets: &Secrets, token: &Token) -> String {
    let payload = serde_json::to_vec(token).expect("a public token always serialises");
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(mac(secrets, &payload)),
    )
}

/// `422 link-invalid` for anything this instance did not issue, `422 link-expired` for a genuine
/// link past its purpose's TTL. Two codes because the pages say different things. Neither carries
/// the token.
///
/// `expected` is the purpose of the endpoint calling: `Unsubscribe` links never expire, so one
/// accepted at the confirm endpoint would confirm a membership for ever.
pub fn parse(secrets: &Secrets, raw: &str, expected: Purpose) -> ApiResult<Token> {
    let invalid = || ApiError::Unprocessable("link-invalid");
    let (payload, sig) = raw.split_once('.').ok_or_else(invalid)?;
    let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| invalid())?;
    let sig = URL_SAFE_NO_PAD.decode(sig).map_err(|_| invalid())?;
    if !tokens::constant_time_eq(&mac(secrets, &payload), &sig) {
        return Err(invalid());
    }
    let token: Token = serde_json::from_slice(&payload).map_err(|_| invalid())?;
    // The caller says which door it is; a genuine token for another purpose is not a key to this one.
    if token.purpose != expected {
        return Err(invalid());
    }
    // A list belongs to a confirmation and to nothing else.
    if token.list_id.is_some() != (token.purpose == Purpose::Confirm) {
        return Err(invalid());
    }
    // A clock that went backwards makes the age negative: not expired.
    let age = chrono::Utc::now()
        .timestamp()
        .saturating_sub(token.issued_at);
    if token.purpose.ttl_secs().is_some_and(|ttl| age > ttl) {
        return Err(ApiError::Unprocessable("link-expired"));
    }
    Ok(token)
}
