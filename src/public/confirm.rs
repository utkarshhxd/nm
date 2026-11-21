// SPDX-License-Identifier: AGPL-3.0-only

//! `GET /c/{token}` — the second half of double opt-in. The link in the confirmation email is the
//! only credential: there is no session here, and an operator's browser that happens to be signed
//! in to this instance gets exactly the same answer as a stranger's. Nothing in this module reads
//! the session; `tests/public_pages.rs` walks this directory to keep it that way.

use axum::{
    Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};

use crate::{
    app::AppState,
    consent, contacts,
    error::{ApiError, ApiResult},
    http::request_meta::RequestMeta,
    lists::{self, MembershipStatus},
    public::{
        page::{UNKNOWN_IP, notice, with},
        token::{self, Purpose},
    },
};

pub fn routes() -> Router<AppState> {
    Router::new().route("/c/{token}", get(confirm))
}

/// What an expired link says, and what a link for a contact or membership that is no longer there
/// says, are deliberately different pages — the first can offer a new one, the second cannot.
///
/// Neither names an address, and the expired page is reached **before any database access**, so it
/// is the same page whether or not the address exists (spec M1d invariant 6). That is the point of
/// it: an attacker holding an old link must not learn from the answer whether the person is still
/// a contact here.
///
/// **The expired page tells the reader a new email can be sent and gives them no button to send
/// one, and that is deliberate — do not "finish" it.** Invariant 6 says an expired link offers to
/// re-send *without disclosing whether the address exists*, and the two halves fight: a resend
/// control reachable from an expired token would have to act on the contact the token names, and
/// anything it does that a token for a deleted contact cannot do is an address-existence oracle,
/// which is spec §4's third anti-goal. Sending unconditionally is worse — it makes this page a
/// free mailer, aimed by whoever holds an old link. The signup form (Task 9) is where a new
/// confirmation is asked for, and it answers identically for every address by construction.
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
    // and parsing is the work an attacker would be making us do. `From<IpAddr> for Key` buckets
    // an IPv6 address to its /64, so this cannot be dodged by sending from a fresh address; do
    // not bucket here as well, and do not key on anything but the address.
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
    // Position 2, before the membership row below and the consent row after it, both of which take
    // this list row through their foreign keys. A list deleted since the link was issued locks
    // nothing and the update below matches nothing, which is the answer this route already gives.
    // `docs/adr/0005-one-lock-order-for-every-path.md`.
    lists::lock_lists(&mut tx, token.workspace_id, &[list_id]).await?;
    // Position 4. A contact deleted since the link was issued took its membership with it, so the
    // update below matches nothing and the page says the link is no longer valid -- which is what
    // it already did, now reached in order instead of through a foreign key at statement end.
    contacts::lock_contact(&mut tx, token.workspace_id, token.contact_id).await?;
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
            //
            // **`Source::Form` is an assumption, and this is where it is made.** It says where the
            // *request to subscribe* came from, not where the confirmation did, and it is true
            // only because `public::forms::enqueue_confirmation` is the sole issuer of a
            // `Purpose::Confirm` token in the tree. A `pending` membership can also be written by
            // `lists::add_member`, by `selection::subscribe` and by the importer, which record
            // `requested` with `operator`, `api` or `import`; the moment M2 or an admin "resend
            // confirmation" issues a confirm token for one of those, this row will say the person
            // signed up through a hosted form and the log will be wrong about the how — which is
            // spec §4's first anti-goal, in the one table that is append-only and cannot be
            // corrected afterwards. There is no `Source` meaning "a confirmation link", so the
            // honest fix when that day comes is to read the source off the membership row this
            // statement just promoted (`list_memberships.source`, `0018`) and map it, or to add a
            // source value. Whoever issues the second kind of confirm token owns this line.

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
                    // What sent the link, when the link says so: `ref_id` is M2's campaign or
                    // message id. M1d issues no token carrying one, so this is `None` today.
                    source_ref: token.ref_id.map(|id| id.to_string()),
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
