// SPDX-License-Identifier: AGPL-3.0-only

//! `/u/{token}` — the address RFC 8058's `List-Unsubscribe` header points at, and the one a
//! human clicks at the bottom of an email. One route, two methods, one token: spec §6.3's "one
//! page, three doors". Both methods parse [`Purpose::Unsubscribe`], because the RFC 8058 POST
//! must land on the same token the link in the footer carries.
//!
//! Nothing here reads the session; `tests/public_pages.rs` walks this directory to keep it that
//! way.

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use uuid::Uuid;

use crate::{
    app::AppState,
    consent,
    error::ApiResult,
    http::request_meta::RequestMeta,
    public::{
        page::{UNKNOWN_IP, notice, with},
        token::{self, Purpose, Token},
    },
    suppressions,
};

pub fn routes() -> Router<AppState> {
    Router::new().route("/u/{token}", post(apply))
}

/// An unsubscribe token never expires (spec §6.2), so there is no "this link is old" page here:
/// a token this instance did not issue, or one issued for another door, is the only refusal.
/// It names no address, so it is the same page whether or not the contact still exists.
fn no_longer_valid() -> Response {
    notice(
        StatusCode::UNPROCESSABLE_ENTITY,
        "This link is no longer valid",
        "<p>This unsubscribe link cannot be used. It may have been truncated by an email client, \
         or it may not be a link this service issued.</p>\n<p class=\"muted\">The footer of any \
         email you have had from here carries a fresh one.</p>",
    )
}

/// `POST /u/{token}` — RFC 8058 one-click. The mail client sends it without asking the person
/// anything further, so it must be the whole of the act: every list in the workspace, not the one
/// the email came from.
///
/// **The rate limiter's verdict is read and deliberately not obeyed here.** `check` returns the
/// `RateLimit` headers and an `over` flag, and the flag is advisory — the handler decides. For
/// this route the decision is to count the request, carry the headers, and unsubscribe anyway: an
/// unsubscribe that does not stick is spec §4's second anti-goal, and a `429` from a mail client's
/// one-click button is exactly that, with nobody to retry it. That is safe because the route is
/// token-authenticated and idempotent — `status <> 'unsubscribed'` means a repeat writes no
/// consent row and no second `unsubscribed_at`. See `http::rate_limit::MAX_KEYS`, which names this
/// route as the one that may not answer `429`.
async fn apply(
    State(state): State<AppState>,
    meta: RequestMeta,
    Path(raw): Path<String>,
) -> ApiResult<Response> {
    let quota = state.public_limit.check(meta.ip.unwrap_or(UNKNOWN_IP));
    let Ok(token) = token::parse(&state.secrets, &raw, Purpose::Unsubscribe) else {
        return Ok(with(quota.headers, no_longer_valid()));
    };
    one_click(&state, &meta, &token).await?;
    // RFC 8058 §3.2: a 200 and nothing to read. The mail client is not a browser and shows the
    // person nothing, so there is no page to render.
    Ok(with(quota.headers, StatusCode::OK.into_response()))
}

/// Unsubscribes the contact from every list in the workspace the token names, writes the
/// workspace-wide opt-out, and records one consent row per thing that actually changed.
///
/// **Two writes, not one, and the second is the one that makes this stick.**
/// `docs/adr/0001-contacts-have-no-status-column.md`: the workspace-wide opt-out is a
/// `suppressions` row keyed by address, because `list_memberships` dies with the contact row. A
/// membership sweep alone is undone by an erasure plus a re-import, or by an import naming this
/// address on a list that does not exist yet — spec M1d invariant 1 only protects a membership
/// that is *already* `unsubscribed`. See `suppressions::unsubscribe_all`.
///
/// A contact deleted since the link was issued took its memberships and its address with it: the
/// update matches nothing, there is nothing to suppress, and the route answers `200` rather than
/// failing (Review Focus 2).
///
/// The `status <> 'unsubscribed'` guard is what makes the rows follow the change rather than the
/// request. Two of these arrive at once more often than not — mail clients prefetch, and some send
/// the POST twice — and under READ COMMITTED the second statement blocks on the row lock, then
/// re-reads the row the first one committed and finds it no longer matches. Without the guard the
/// same person would appear to have unsubscribed twice. The suppression insert is idempotent the
/// same way, by the table's live-uniqueness index.
async fn one_click(state: &AppState, meta: &RequestMeta, token: &Token) -> ApiResult<()> {
    let mut tx = state.pool.begin().await?;
    let changed: Vec<Uuid> = sqlx::query_scalar!(
        "update list_memberships set status = 'unsubscribed', unsubscribed_at = now() \
          where workspace_id = $1 and contact_id = $2 and status <> 'unsubscribed' \
         returning list_id",
        token.workspace_id,
        token.contact_id,
    )
    .fetch_all(&mut *tx)
    .await?;
    let email = sqlx::query_scalar!(
        "select email from contacts where workspace_id = $1 and id = $2",
        token.workspace_id,
        token.contact_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    // The subscriber's own request, so the IP and the user agent really are theirs and are kept as
    // evidence — with `GET /c/{token}` and the preference center, the only places in the product
    // where that is true. `token_jti` is the nonce: *which* link was followed, not merely that one
    // was. One `record` per changed list rather than a set-based insert: the row count is the
    // number of lists in one workspace, all inside this transaction, and reusing the writer that
    // scrubs its own error beats a second hand-written statement that binds an IP.
    let cx = consent::Context {
        ip: meta.ip,
        user_agent: meta.user_agent.clone(),
        source_ref: None,
        token_jti: Some(token.nonce.clone()),
    };
    for list_id in changed {
        consent::record(
            &mut tx,
            token.workspace_id,
            token.contact_id,
            Some(list_id),
            consent::Action::Unsubscribed,
            consent::Source::OneClick,
            &cx,
        )
        .await?;
    }
    // `list_id` null: a workspace-wide opt-out has no list, which is what `0020` reserved the
    // nullable column for. Recorded only when a row was actually written, so a second click
    // records nothing — the same rule the memberships follow.
    if let Some(email) = email
        && suppressions::unsubscribe_all(&mut tx, &state.secrets, token.workspace_id, &email)
            .await?
    {
        consent::record(
            &mut tx,
            token.workspace_id,
            token.contact_id,
            None,
            consent::Action::Suppressed,
            consent::Source::OneClick,
            &cx,
        )
        .await?;
    }
    tx.commit().await.map_err(Into::into)
}
