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
    ///
    /// **Public so that nothing else has to say "fourteen days".** `comment on table jobs` makes a
    /// `confirmation_email` row older than this TTL a row to fail rather than mail, and
    /// `worker::confirmation` enforces that. A second constant meaning the same window is how the
    /// two drift, and the drift would be silent: the sender would keep mailing links the confirm
    /// page refuses, or stop mailing links it would still accept.
    pub fn ttl_secs(self) -> Option<i64> {
        match self {
            Purpose::Confirm => Some(14 * DAY),
            Purpose::Unsubscribe => None,
        }
    }
}

/// The field names are part of the token format: renaming one invalidates links already sent.
/// `deny_unknown_fields`: a payload carrying something this build does not know is refused, not
/// read.
///
/// **The two fields a later milestone needs are here, and this is the milestone that could add
/// them.** `kid` and `ref_id` were named in this comment as the things M2 would want — a key
/// version for rotation, a campaign id for click tracking — by a version of it that then shipped
/// neither. That was the defect: `Purpose::Unsubscribe` has no TTL, so these links sit in inboxes
/// for years, every field here was mandatory, and adding a mandatory field would have made every
/// one of those links a hard deserialize error. The documented escape — bumping `PUBLIC_DOMAIN` —
/// invalidates them all at once, which spec §6.2 forbids outright: "an email sits in an inbox for
/// years and RFC 8058 must still work." (2026-11-20)
///
/// **What the two of them cost an already-issued link: nothing.** `skip_serializing_if` is what
/// holds that — a link issued today carries neither key, so its bytes are what an older build
/// already reads, and a test pins the key list. On the way in, being `Option` is what lets a
/// payload that predates them deserialize: serde reads a missing `Option` field as `None`,
/// `deny_unknown_fields` or not. `serde(default)` is therefore belt and braces here and is kept
/// as the explicit statement of intent, because it is what would carry a field that is not an
/// `Option`.
///
/// So the door is narrower than it looked, which is worth writing down rather than leaving as
/// folklore: a later field added as an `Option` with this same pair of attributes also parses
/// every link already sent. What `deny_unknown_fields` still refuses is the other direction — an
/// older build reading a newer link, which a rolling deploy produces for as long as both are
/// serving, and which a test pins. A new field stays a deliberate act with a cost; it is not the
/// one-way door this comment used to describe.
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
    /// Which key signed this link. `None` means the master key this product has always had —
    /// `derive_key` takes no key id, so there is nothing else it can mean today, and every link
    /// M1d issues says it by saying nothing.
    ///
    /// It exists so that rotating the master key does not have to invalidate every outstanding
    /// unsubscribe link, which §6.2 forbids. `crypto::secrets` already carries a `KEY_ID` byte in
    /// its encrypted-blob header for the same reason; this was the one credential in the product
    /// that never expires and could not say which key signed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kid: Option<u8>,
    /// What sent this link: M2 fills it with a campaign or message id, M1d always leaves it
    /// `None`. Deliberately generic — a milestone that cannot add a field cannot add a *name*
    /// either.
    ///
    /// The public routes copy it into `consent_log.source_ref`, the column built to hold
    /// "whatever caused the change". Without it that column is permanently null for every
    /// one-click unsubscribe M2 will ever send, because the token carries nothing to attribute
    /// with and no later milestone could add it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_id: Option<Uuid>,
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
