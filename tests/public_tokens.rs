// SPDX-License-Identifier: AGPL-3.0-only

//! The signed token the public pages are reached by. Pure unit-level over `issue`/`parse`: no
//! database, so no harness.
//!
//! Two purposes, not three: `/u/{token}` is both the RFC 8058 one-click POST and the preference
//! center, so `Purpose::Preference` and its 90-day TTL were deleted in Task 5. See `Purpose`.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use nm::{
    config::SecretKey,
    crypto::secrets::Secrets,
    error::ApiError,
    public::token::{PUBLIC_DOMAIN, Purpose, Token, issue, parse},
};
use uuid::Uuid;

const DAY: i64 = 24 * 60 * 60;

fn secrets(byte: u8) -> Secrets {
    Secrets::new(&SecretKey([byte; 32]))
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn token(purpose: Purpose, age_days: i64) -> Token {
    Token {
        workspace_id: Uuid::now_v7(),
        contact_id: Uuid::now_v7(),
        list_id: (purpose == Purpose::Confirm).then(Uuid::now_v7),
        purpose,
        issued_at: now() - age_days * DAY,
        nonce: "n0nce".into(),
        kid: None,
        ref_id: None,
    }
}

/// Signs a payload the way `issue` does, for payloads `Token` cannot produce: an older build's,
/// and a newer build's. `derive_key` is `hmac_sha256(master, label)`, which is what `secrets`
/// builds and what this reproduces.
fn sign(master: u8, payload: &serde_json::Value) -> String {
    let payload = serde_json::to_vec(payload).unwrap();
    let key = nm::crypto::tokens::hmac_sha256(&[master; 32], PUBLIC_DOMAIN.as_bytes());
    let mac = nm::crypto::tokens::hmac_sha256(&key, &payload);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(mac)
    )
}

/// Changes the first character, which is always fully significant in base64.
fn flip(part: &str) -> String {
    let first = if part.starts_with('A') { 'B' } else { 'A' };
    format!("{first}{}", &part[1..])
}

#[test]
fn a_token_round_trips_through_issue_and_parse() {
    let s = secrets(7);
    let t = token(Purpose::Confirm, 0);
    assert_eq!(parse(&s, &issue(&s, &t), Purpose::Confirm).unwrap(), t);
}

#[test]
fn a_token_with_one_byte_altered_is_refused() {
    let s = secrets(7);
    let raw = issue(&s, &token(Purpose::Unsubscribe, 0));
    let (payload, mac) = raw.split_once('.').unwrap();
    for altered in [
        format!("{}.{mac}", flip(payload)),
        format!("{payload}.{}", flip(mac)),
    ] {
        assert!(matches!(
            parse(&s, &altered, Purpose::Unsubscribe),
            Err(ApiError::Unprocessable("link-invalid"))
        ));
    }
}

#[test]
fn a_token_signed_with_another_secret_is_refused() {
    let raw = issue(&secrets(7), &token(Purpose::Unsubscribe, 0));
    assert!(matches!(
        parse(&secrets(8), &raw, Purpose::Unsubscribe),
        Err(ApiError::Unprocessable("link-invalid"))
    ));
}

#[test]
fn a_confirm_token_older_than_14_days_is_refused() {
    let s = secrets(7);
    assert!(
        parse(
            &s,
            &issue(&s, &token(Purpose::Confirm, 13)),
            Purpose::Confirm
        )
        .is_ok()
    );
    assert!(matches!(
        parse(
            &s,
            &issue(&s, &token(Purpose::Confirm, 15)),
            Purpose::Confirm
        ),
        Err(ApiError::Unprocessable("link-expired"))
    ));
}

#[test]
fn an_unsubscribe_token_two_years_old_is_accepted() {
    let s = secrets(7);
    let t = token(Purpose::Unsubscribe, 2 * 365);
    assert_eq!(parse(&s, &issue(&s, &t), Purpose::Unsubscribe).unwrap(), t);
}

#[test]
fn the_purpose_is_part_of_what_is_signed() {
    let s = secrets(7);
    let raw = issue(&s, &token(Purpose::Unsubscribe, 0));
    let (payload, mac) = raw.split_once('.').unwrap();
    let original = URL_SAFE_NO_PAD.decode(payload).unwrap();
    // **`list_id` moves with the purpose, so the forgery is structurally valid.** `parse` checks
    // the MAC and then checks that `list_id` is present exactly for `Confirm`. A payload that said
    // `confirm` while keeping `list_id: null` would be refused by that second check too, and this
    // test would pass with the signature verification deleted — which it did, until a review
    // caught it. Only the signature may be what refuses the payload below.
    let mut json: serde_json::Value = serde_json::from_slice(&original).unwrap();
    json["purpose"] = serde_json::json!("confirm");
    json["list_id"] = serde_json::json!(Uuid::now_v7());
    let swapped = serde_json::to_vec(&json).unwrap();
    assert_ne!(swapped, original, "the purpose must appear in the payload");

    // The premise, asserted rather than assumed: this payload parses when it is honestly signed.
    let honest = issue(&s, &serde_json::from_slice::<Token>(&swapped).unwrap());
    assert!(parse(&s, &honest, Purpose::Confirm).is_ok());

    let forged = format!("{}.{mac}", URL_SAFE_NO_PAD.encode(&swapped));
    assert!(matches!(
        parse(&s, &forged, Purpose::Confirm),
        Err(ApiError::Unprocessable("link-invalid"))
    ));
}

/// Domain separation: a payload signed under the selection token's key must not pass here. Same
/// master secret on both sides, so only the label can be what refuses it.
#[test]
fn a_token_signed_under_another_domain_is_refused() {
    let payload = serde_json::to_vec(&token(Purpose::Unsubscribe, 0)).unwrap();
    let key = nm::crypto::tokens::hmac_sha256(&[7u8; 32], b"nm:selection:v3");
    let mac = nm::crypto::tokens::hmac_sha256(&key, &payload);
    let forged = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(mac)
    );
    assert!(matches!(
        parse(&secrets(7), &forged, Purpose::Unsubscribe),
        Err(ApiError::Unprocessable("link-invalid"))
    ));
}

/// Unsubscribe links never expire, so one presented at the confirm endpoint would confirm a
/// membership for ever. The caller names the purpose it serves; a genuine token for another is
/// refused.
#[test]
fn a_token_issued_for_another_purpose_is_refused() {
    let s = secrets(7);
    let raw = issue(&s, &token(Purpose::Unsubscribe, 0));
    assert!(matches!(
        parse(&s, &raw, Purpose::Confirm),
        Err(ApiError::Unprocessable("link-invalid"))
    ));
}

#[test]
fn a_token_issued_for_the_expected_purpose_is_accepted() {
    let s = secrets(7);
    let t = token(Purpose::Confirm, 0);
    assert_eq!(parse(&s, &issue(&s, &t), Purpose::Confirm).unwrap(), t);
}

/// A list belongs to a confirmation and to nothing else; a confirmation with no list has nothing
/// to confirm. Both halves are signed, so only `parse` can refuse them.
#[test]
fn list_id_is_present_exactly_when_the_purpose_is_confirm() {
    let s = secrets(7);
    let mut stray = token(Purpose::Unsubscribe, 0);
    stray.list_id = Some(Uuid::now_v7());
    let mut missing = token(Purpose::Confirm, 0);
    missing.list_id = None;
    for t in [stray, missing] {
        assert!(matches!(
            parse(&s, &issue(&s, &t), t.purpose),
            Err(ApiError::Unprocessable("link-invalid"))
        ));
    }
}

/// **The compatibility guarantee, and the only one that matters.** `kid` and `ref_id` were added
/// in M1d's final wave; every unsubscribe link already in an inbox was issued without them and
/// never expires. The payload below is that link's — six fields, signed, built by hand rather
/// than by today's `issue`, which would only prove this build agrees with itself. It parses, and
/// the two absent fields read as `None`.
#[test]
fn a_link_issued_before_kid_and_ref_id_existed_still_parses() {
    let s = secrets(7);
    let want = token(Purpose::Unsubscribe, 2 * 365);
    let old = serde_json::json!({
        "workspace_id": want.workspace_id,
        "contact_id": want.contact_id,
        "purpose": "unsubscribe",
        "list_id": serde_json::Value::Null,
        "issued_at": want.issued_at,
        "nonce": want.nonce,
    });
    assert_eq!(
        parse(&s, &sign(7, &old), Purpose::Unsubscribe).unwrap(),
        want
    );
}

/// The other half of it: a link issued **today** carries neither key, so its bytes are what an
/// older build can still read. `skip_serializing_if` is what holds this, and nothing else would
/// notice if it went.
#[test]
fn an_issued_link_carries_neither_new_field_until_one_is_set() {
    let raw = issue(&secrets(7), &token(Purpose::Unsubscribe, 0));
    let payload = URL_SAFE_NO_PAD
        .decode(raw.split_once('.').unwrap().0)
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "contact_id",
            "issued_at",
            "list_id",
            "nonce",
            "purpose",
            "workspace_id"
        ]
    );
}

/// What M2 will issue: a key id to verify under after a rotation, and the campaign that sent the
/// link, which the public routes copy into `consent_log.source_ref`.
#[test]
fn a_link_carrying_a_key_id_and_a_ref_round_trips() {
    let s = secrets(7);
    let mut t = token(Purpose::Unsubscribe, 0);
    t.kid = Some(2);
    t.ref_id = Some(Uuid::now_v7());
    assert_eq!(parse(&s, &issue(&s, &t), Purpose::Unsubscribe).unwrap(), t);
}

/// `deny_unknown_fields` stays. `serde(default)` is what lets an old payload through, and the
/// two are not the same relaxation: a payload naming something this build has never heard of is
/// still refused, signature or no signature.
#[test]
fn a_payload_naming_a_field_this_build_does_not_know_is_refused() {
    let t = token(Purpose::Unsubscribe, 0);
    let ahead = serde_json::json!({
        "workspace_id": t.workspace_id,
        "contact_id": t.contact_id,
        "purpose": "unsubscribe",
        "list_id": serde_json::Value::Null,
        "issued_at": t.issued_at,
        "nonce": t.nonce,
        "sent_by": "a field from a later milestone",
    });
    assert!(matches!(
        parse(&secrets(7), &sign(7, &ahead), Purpose::Unsubscribe),
        Err(ApiError::Unprocessable("link-invalid"))
    ));
}
