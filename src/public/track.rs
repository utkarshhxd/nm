// SPDX-License-Identifier: AGPL-3.0-only

//! `GET /o/{token}`, the open pixel, and `GET /l/{token}/{link}`, the click redirect (M2d spec
//! §4.1). The token names one message and nothing else: `base64url(message_id ‖ mac)`, so an
//! erased contact leaves only a uuid behind, and a click can only ever reach a URL the campaign's
//! own freeze wrote into its link table.
//!
//! Nothing here reads the session; `tests/public_pages.rs` walks this directory to keep it that
//! way.

use axum::{
    Router,
    extract::{Path, State, rejection::PathRejection},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use uuid::Uuid;

use crate::{
    app::AppState,
    crypto::{secrets::Secrets, tokens},
    error::ApiResult,
    http::request_meta::RequestMeta,
    public::page::{UNKNOWN_IP, notice, with},
    template::render::{UrlCheck, check_static_url},
};

/// Domain-separation label for the tracking-token key: a tracking token can never be replayed as
/// an unsubscribe or confirm token, nor those as it.
pub const TRACK_DOMAIN: &str = "nm:track-token:v1";

/// A transparent 1×1 GIF89a. Every answer the pixel route gives is these bytes.
pub const PIXEL: [u8; 43] = [
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

/// The first 16 bytes of HMAC-SHA256 over the message id, under the tracking subkey.
fn mac(secrets: &Secrets, message_id: Uuid) -> [u8; 16] {
    let full = tokens::hmac_sha256(
        secrets.derive_key(TRACK_DOMAIN).as_slice(),
        message_id.as_bytes(),
    );
    full[..16].try_into().expect("16 of 32 bytes")
}

/// `base64url(message_id ‖ mac)`, 43 characters. Deterministic: every attempt at one message
/// carries the same token (spec §4.1.1).
pub fn issue(secrets: &Secrets, message_id: Uuid) -> String {
    let mut raw = [0u8; 32];
    raw[..16].copy_from_slice(message_id.as_bytes());
    raw[16..].copy_from_slice(&mac(secrets, message_id));
    URL_SAFE_NO_PAD.encode(raw)
}

/// The message a token names, or `None` for anything this instance did not issue. The MAC is
/// compared in constant time and nothing touches the database, so a forged token costs a hash.
pub fn parse(secrets: &Secrets, raw: &str) -> Option<Uuid> {
    // The engine refuses non-zero trailing bits, so each message has exactly one spelling.
    let bytes: [u8; 32] = URL_SAFE_NO_PAD.decode(raw).ok()?.try_into().ok()?;
    let id = Uuid::from_slice(&bytes[..16]).ok()?;
    tokens::constant_time_eq(&mac(secrets, id), &bytes[16..]).then_some(id)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/o/{token}", get(open))
        .route("/l/{token}/{link}", get(click))
}

/// `GET /o/{token}`. Spec §4.1.4: the same `200` GIF for a forged, an unknown and a real token,
/// within budget or not, and no `RateLimit` headers, so the answer says nothing about either. A
/// `HEAD` is a probe, not a person (§4.1.6), and records nothing.
async fn open(
    State(state): State<AppState>,
    meta: RequestMeta,
    method: Method,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let quota = state.track_limit.check(meta.ip.unwrap_or(UNKNOWN_IP));
    if method == Method::GET
        && quota.over.is_none()
        && let Some(id) = path.ok().and_then(|Path(raw)| parse(&state.secrets, &raw))
    {
        // shortcut: one statement per open, in the request (spec §4.2.3); add an in-memory buffer
        // when §9's 5,000 req/s is measured, in M7.
        let recorded = sqlx::query!(
            "update messages set opens = opens + 1, opened_at = coalesce(opened_at, now())
              where id = $1",
            id,
        )
        .execute(&state.pool)
        .await;
        if let Err(e) = recorded {
            tracing::error!(message_id = %id, error = %e, "an open could not be recorded");
        }
    }
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("image/gif"))],
        PIXEL,
    )
        .into_response()
}

/// Every refusal a click can get but the rate limit: a bad MAC, no such message, an index past
/// the table, a stored URL that is not `http`/`https`. One page, so none of them is an oracle.
fn not_valid() -> Response {
    notice(
        StatusCode::NOT_FOUND,
        "This link is not valid",
        "<p>This link cannot be followed. It may have been truncated by an email client.</p>\n\
         <p class=\"muted\">Open the email again and use the link from there.</p>",
    )
}

/// The `GET /u/{token}` refusal, written out here for the reason `unsubscribe::too_many_requests`
/// gives: each public route decides for itself what a tripped limit means.
fn too_many_requests(headers: HeaderMap, retry_after_secs: u64) -> Response {
    let mut headers = headers;
    headers.insert(header::RETRY_AFTER, retry_after_secs.into());
    (
        headers,
        notice(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many requests",
            "<p>Too many requests have come from your network in the last minute.</p>\n\
             <p class=\"muted\">Wait a minute and open the link again.</p>",
        ),
    )
        .into_response()
}

/// `GET /l/{token}/{link}`. Spec §4.1.5: a `302` only to an `http`/`https` URL at index `link` of
/// the frozen link table of this message's own campaign and variant. Nothing else in the request
/// is read, so a query string cannot name a target.
async fn click(
    State(state): State<AppState>,
    meta: RequestMeta,
    method: Method,
    path: Result<Path<(String, String)>, PathRejection>,
) -> ApiResult<Response> {
    let quota = state.track_limit.check(meta.ip.unwrap_or(UNKNOWN_IP));
    if let Some(retry_after_secs) = quota.over {
        return Ok(too_many_requests(quota.headers, retry_after_secs));
    }
    let named = path.ok().and_then(|Path((raw, link))| {
        let link = i32::try_from(link.parse::<u32>().ok()?).ok()?;
        Some((parse(&state.secrets, &raw)?, link))
    });
    let Some((id, link)) = named else {
        return Ok(with(quota.headers, not_valid()));
    };
    // A plain read. The table is written only by the freeze; the join's workspace predicate is
    // the tenancy pin `messages_campaign_fk` already holds.
    let stored = sqlx::query_scalar!(
        r#"select c.frozen #>> array['variants', m.variant::text, 'rendition', 'links', $2]
             from messages m
             join campaigns c on c.id = m.campaign_id and c.workspace_id = m.workspace_id
            where m.id = $1"#,
        id,
        link.to_string(),
    )
    .fetch_optional(&state.pool)
    .await?
    .flatten();
    // Re-checked, not trusted: a stored URL that does not re-parse as http(s) is never followed.
    let target = stored
        .and_then(|s| match check_static_url(&s) {
            UrlCheck::Allowed(u) if matches!(u.scheme(), "http" | "https") => Some(u),
            _ => None,
        })
        .and_then(|u| HeaderValue::from_str(u.as_str()).ok());
    let Some(target) = target else {
        return Ok(with(quota.headers, not_valid()));
    };
    if method == Method::GET {
        // A click implies an open (spec §4.3.5): `opened_at`, never `opens`.
        let recorded = sqlx::query!(
            "with m as (update messages set clicks = clicks + 1,
                                            clicked_at = coalesce(clicked_at, now()),
                                            opened_at = coalesce(opened_at, now())
                         where id = $1 returning workspace_id, campaign_id, variant)
             insert into message_clicks (message_id, link, workspace_id, campaign_id, variant)
             select $1, $2, workspace_id, campaign_id, variant from m
             on conflict (message_id, link)
             do update set clicks = message_clicks.clicks + 1, last_at = now()",
            id,
            link,
        )
        .execute(&state.pool)
        .await;
        if let Err(e) = recorded {
            tracing::error!(message_id = %id, error = %e, "a click could not be recorded");
        }
    }
    let mut headers = quota.headers;
    headers.insert(header::LOCATION, target);
    Ok((StatusCode::FOUND, headers).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::SecretKey,
        public::token::{self, Purpose, Token},
    };

    fn secrets(byte: u8) -> Secrets {
        Secrets::new(&SecretKey([byte; 32]))
    }

    #[test]
    fn a_token_is_43_characters_and_round_trips_to_its_message() {
        let s = secrets(7);
        let id = Uuid::now_v7();
        let t = issue(&s, id);
        assert_eq!(t.len(), 43, "{t}");
        assert!(
            t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "{t}"
        );
        assert_eq!(parse(&s, &t), Some(id));
        assert_eq!(issue(&s, id), t, "one token per message");
        assert_ne!(issue(&s, Uuid::now_v7()), t);
        // Another instance's key does not sign for this one.
        assert_eq!(parse(&secrets(8), &t), None);
    }

    #[test]
    fn a_token_with_any_byte_changed_is_refused() {
        let s = secrets(7);
        let t = issue(&s, Uuid::now_v7());
        for at in 0..t.len() {
            for c in ['A', 'B', '-', '_', 'z'] {
                let mut altered = t.clone().into_bytes();
                if altered[at] == c as u8 {
                    continue;
                }
                altered[at] = c as u8;
                let altered = String::from_utf8(altered).unwrap();
                assert_eq!(parse(&s, &altered), None, "position {at} as {c}: {altered}");
            }
        }
        // Shorter, longer, padded, empty.
        for bad in [
            t[..42].to_owned(),
            format!("{t}A"),
            format!("{t}="),
            String::new(),
        ] {
            assert_eq!(parse(&s, &bad), None, "{bad}");
        }
    }

    #[test]
    fn an_unsubscribe_token_is_not_a_tracking_token() {
        let s = secrets(7);
        let id = Uuid::now_v7();
        let unsubscribe = token::issue(
            &s,
            &Token {
                workspace_id: Uuid::now_v7(),
                contact_id: Uuid::now_v7(),
                purpose: Purpose::Unsubscribe,
                list_id: None,
                issued_at: 0,
                nonce: id.to_string(),
                kid: None,
                ref_id: None,
            },
        );
        assert_eq!(parse(&s, &unsubscribe), None);
        // Nor the reverse: the same id and bytes under the public key's label do not verify.
        assert!(token::parse(&s, &issue(&s, id), Purpose::Unsubscribe).is_err());
        let mut raw = [0u8; 32];
        raw[..16].copy_from_slice(id.as_bytes());
        let public =
            tokens::hmac_sha256(s.derive_key(token::PUBLIC_DOMAIN).as_slice(), id.as_bytes());
        raw[16..].copy_from_slice(&public[..16]);
        assert_eq!(parse(&s, &URL_SAFE_NO_PAD.encode(raw)), None);
    }

    #[test]
    fn the_pixel_is_a_43_byte_transparent_gif() {
        assert_eq!(&PIXEL[..6], b"GIF89a");
        assert_eq!(PIXEL.len(), 43);
        // Graphic control extension: transparency on, colour index 0.
        assert_eq!(&PIXEL[19..24], &[0x21, 0xf9, 0x04, 0x01, 0x00]);
        assert_eq!(PIXEL[42], 0x3b, "the trailer");
    }
}
