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
    public::token::{Purpose, Token, issue, parse},
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
    }
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
