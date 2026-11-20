// SPDX-License-Identifier: AGPL-3.0-only

//! `GET /c/{token}` — the second half of double opt-in. The link in the confirmation email is the
//! only credential: there is no session here, and an operator's browser that happens to be signed
//! in to this instance gets exactly the same answer as a stranger's. Nothing in this module reads
//! the session; `tests/public_pages.rs` walks this directory to keep it that way.

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use std::net::{IpAddr, Ipv4Addr};

use crate::{
    app::AppState,
    consent,
    error::{ApiError, ApiResult},
    http::request_meta::RequestMeta,
    lists::MembershipStatus,
    public::{
        page,
        token::{self, Purpose},
    },
};

pub fn routes() -> Router<AppState> {
    Router::new().route("/c/{token}", get(confirm))
}

/// The bucket a request whose client address we cannot see is counted in. `RequestMeta` has no IP
/// only when the server was built without `ConnectInfo`, which production never is; one shared
/// bucket for that case limits them together rather than letting them past the limiter.
const UNKNOWN_IP: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

/// A page with a status: every answer this route gives a browser is HTML, including the refusals.
/// A subscriber who clicks a dead link gets a sentence, not an RFC 9457 document.
fn notice(status: StatusCode, title: &str, body: &str) -> Response {
    (status, page::shell(title, body.to_owned())).into_response()
}

/// What an expired link says, and what a link for a contact or membership that is no longer there
/// says, are deliberately different pages — the first can offer a new one, the second cannot.
///
/// Neither names an address, and the expired page is reached **before any database access**, so it
/// is the same page whether or not the address exists (spec M1d invariant 6). That is the point of
/// it: an attacker holding an old link must not learn from the answer whether the person is still
/// a contact here.
fn expired() -> Response {
    notice(
        StatusCode::UNPROCESSABLE_ENTITY,
        "This link has expired",
        "<p>Confirmation links are good for 14 days. This one is older than that, so it can no \
         longer be used.</p>\n<p class=\"muted\">Sign up again and a new confirmation email will \
         be sent.</p>",
    )
}

fn no_longer_valid() -> Response {
    notice(
        StatusCode::UNPROCESSABLE_ENTITY,
        "This link is no longer valid",
        "<p>This confirmation link cannot be used. It may have been replaced by a newer one, or \
         the subscription it was for may no longer exist.</p>\n<p class=\"muted\">If you still \
         want these emails, sign up again.</p>",
    )
}

async fn confirm(
    State(state): State<AppState>,
    meta: RequestMeta,
    Path(raw): Path<String>,
) -> ApiResult<Response> {
    // Before the token is even looked at, and keyed on the address rather than the workspace: the
    // workspace is inside the token, so there is nothing else to count against until it parses,
    // and parsing is the work an attacker would be making us do.
    let quota = state.public_limit.check(meta.ip.unwrap_or(UNKNOWN_IP));
    if let Some(retry_after_secs) = quota.over {
        let mut headers = quota.headers;
        headers.insert(header::RETRY_AFTER, retry_after_secs.into());
        return Ok((
            headers,
            notice(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many requests",
                "<p>Too many requests have come from your network in the last minute.</p>\n\
                 <p class=\"muted\">Wait a minute and open the link again.</p>",
            ),
        )
            .into_response());
    }
    let token = match token::parse(&state.secrets, &raw, Purpose::Confirm) {
        Ok(token) => token,
        // An unsubscribe or preference token, a forged one, and a mistyped one are one answer:
        // `parse` tells them apart only to decide whether a new link can be offered.
        Err(ApiError::Unprocessable("link-expired")) => return Ok(with(quota.headers, expired())),
        Err(_) => return Ok(with(quota.headers, no_longer_valid())),
    };
    // `parse` refuses a `Confirm` token without one, so this is unreachable; it is written as a
    // branch rather than an `expect` because a panic in a handler is a 500 for a stranger.
    let Some(list_id) = token.list_id else {
        return Ok(with(quota.headers, no_longer_valid()));
    };

    let mut tx = state.pool.begin().await?;
    // One statement, so the change and the status it replaced come from one snapshot. `status` is
    // the row as it was **before** this update, because the sub-select reads the same snapshot the
    // CTE did: a confirmed row reads `pending` here, which is why `confirmed` is tested first.
    //
    // `status = 'pending'` is the whole guard. A membership that was unsubscribed is not revived
    // by a confirmation link that predates the unsubscribe (spec M1d invariant 1), and a contact
    // deleted since the link was issued took the membership with it, so the update matches
    // nothing and the page says so instead of failing (Review Focus 2).
    let row = sqlx::query!(
        r#"with changed as (
               update list_memberships set status = 'subscribed', subscribed_at = now()
                where workspace_id = $1 and contact_id = $2 and list_id = $3
                  and status = 'pending'
               returning 1
           )
           select exists (select 1 from changed) as "confirmed!",
                  (select status from list_memberships
                    where workspace_id = $1 and contact_id = $2
                      and list_id = $3) as "status: MembershipStatus""#,
        token.workspace_id,
        token.contact_id,
        list_id,
    )
    .fetch_one(&mut *tx)
    .await?;

    let answer = match (row.confirmed, row.status) {
        (true, _) => {
            // The subscriber's own request, so the IP and the user agent really are theirs and are
            // kept as evidence — the one place in the product where that is true. `token_jti` is
            // the nonce, which says *which* link was followed and not merely that one was.
            consent::record(
                &mut tx,
                token.workspace_id,
                token.contact_id,
                Some(list_id),
                consent::Action::Confirmed,
                consent::Source::Form,
                &consent::Context {
                    ip: meta.ip,
                    user_agent: meta.user_agent,
                    source_ref: None,
                    token_jti: Some(token.nonce),
                },
            )
            .await?;
            notice(
                StatusCode::OK,
                "You're subscribed",
                "<p>Your email address is confirmed and you are on the list.</p>\n\
                 <p class=\"muted\">Every email you get from here has an unsubscribe link in \
                 it.</p>",
            )
        }
        // Invariant 5: a link used twice confirms once and says so. `Pending` lands here as well
        // as `Subscribed`, because the only way a row is still pending after an update that did
        // not match it is that another request confirmed it between the two.
        (false, Some(MembershipStatus::Pending | MembershipStatus::Subscribed)) => notice(
            StatusCode::OK,
            "Already confirmed",
            "<p>This address was already confirmed, so there was nothing left to do.</p>\n\
             <p class=\"muted\">You are on the list.</p>",
        ),
        (false, _) => no_longer_valid(),
    };
    tx.commit().await?;
    Ok(with(quota.headers, answer))
}

/// Puts the `RateLimit` headers on an answer. Every answer carries them, refusals included, as the
/// authenticated limited routes do.
fn with(headers: HeaderMap, page: Response) -> Response {
    (headers, page).into_response()
}
