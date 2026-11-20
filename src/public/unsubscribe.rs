// SPDX-License-Identifier: AGPL-3.0-only

//! `/u/{token}` — the address RFC 8058's `List-Unsubscribe` header points at, and the one a
//! human clicks at the bottom of an email. One route, two methods, one token: spec §6.3's "one
//! page, three doors". Both methods parse [`Purpose::Unsubscribe`], because the RFC 8058 POST
//! must land on the same token the link in the footer carries.
//!
//! Nothing here reads the session; `tests/public_pages.rs` walks this directory to keep it that
//! way.

use std::collections::{HashMap, HashSet};

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use sqlx::PgConnection;
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    app::AppState,
    consent,
    error::ApiResult,
    http::request_meta::RequestMeta,
    lists::{MembershipSource, MembershipStatus},
    public::{
        page::{self, UNKNOWN_IP, notice, with},
        token::{self, Purpose, Token},
    },
    suppressions,
};

pub fn routes() -> Router<AppState> {
    Router::new().route("/u/{token}", get(preferences).post(apply))
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

/// The refusal this route gives a human who is over the per-IP limit.
///
/// **Written out here rather than shared with `confirm.rs`, and not to be shared with Task 9.**
/// Each public route decides for itself what a tripped limit means: `GET /c/{token}` and this GET
/// answer `429`, the one-click POST below ignores the verdict entirely, and spec §8.9 requires
/// `POST /f/{slug}` to answer a rate-limited submission exactly as a successful one. A
/// `page::too_many_requests()` helper would be a refusal those last two could inherit by reaching
/// for the obvious name.
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

/// `GET /u/{token}` — the preference center. Spec §6.3: the unsubscribe link offers "or just
/// change what you get", which is the highest-leverage retention surface an email product has.
///
/// Unlike the one-click POST, this is a person at a browser who can read a refusal and open the
/// link again, so a tripped limit is answered `429` the way `GET /c/{token}` answers it.
async fn preferences(
    State(state): State<AppState>,
    meta: RequestMeta,
    Path(raw): Path<String>,
) -> ApiResult<Response> {
    let quota = state.public_limit.check(meta.ip.unwrap_or(UNKNOWN_IP));
    if let Some(retry_after_secs) = quota.over {
        return Ok(too_many_requests(quota.headers, retry_after_secs));
    }
    let Ok(token) = token::parse(&state.secrets, &raw, Purpose::Unsubscribe) else {
        return Ok(with(quota.headers, no_longer_valid()));
    };
    let mut db = state.pool.acquire().await?;
    // A contact deleted since the link was issued: the token still verifies and the foreign key
    // is gone. A sentence, not a 500, and not an empty page that would read as "you are on
    // nothing" (Review Focus 2).
    if contact_email(&mut db, token.workspace_id, token.contact_id)
        .await?
        .is_none()
    {
        return Ok(with(quota.headers, no_longer_valid()));
    }
    let topics = topics(&mut db, token.workspace_id, token.contact_id).await?;
    Ok(with(quota.headers, render(&topics, None)))
}

/// `POST /u/{token}` — two doors behind one method, told apart by the body.
///
/// `List-Unsubscribe=One-Click` (RFC 8058 §3.1) is the mail client's button: it sends it without
/// asking the person anything further, so it must be the whole of the act — every list in the
/// workspace, not the one the email came from. Anything else is the preference form, whose
/// checkboxes say what the person wants.
///
/// **The rate limiter's verdict is read and deliberately not obeyed for one-click.** `check`
/// returns the `RateLimit` headers and an `over` flag, and the flag is advisory — the handler
/// decides. For one-click the decision is to count the request, carry the headers, and unsubscribe
/// anyway: an unsubscribe that does not stick is spec §4's second anti-goal, and a `429` from a
/// mail client's one-click button is exactly that, with nobody to retry it. That is safe because
/// the route is token-authenticated and idempotent — `status <> 'unsubscribed'` means a repeat
/// writes no consent row and no second `unsubscribed_at`. See `http::rate_limit::MAX_KEYS`, which
/// names this route as the one that may not answer `429`. **The preference form is refused**, like
/// the GET: it is a person who can try again, and one who did not ask for everything to stop.
///
/// **The exemption is granted to a genuine link, not to a string in a body.** The token is
/// verified *before* the verdict is applied, so only a one-click POST that this instance actually
/// signed is let past a tripped limit. Deciding it from `is_one_click` alone would have made the
/// whole route unlimitable by anyone willing to type the RFC's field name.
async fn apply(
    State(state): State<AppState>,
    meta: RequestMeta,
    Path(raw): Path<String>,
    body: String,
) -> ApiResult<Response> {
    let quota = state.public_limit.check(meta.ip.unwrap_or(UNKNOWN_IP));
    let parsed = token::parse(&state.secrets, &raw, Purpose::Unsubscribe);
    let one_click_asked = is_one_click(&body) && parsed.is_ok();
    if !one_click_asked && let Some(retry_after_secs) = quota.over {
        return Ok(too_many_requests(quota.headers, retry_after_secs));
    }
    let Ok(token) = parsed else {
        return Ok(with(quota.headers, no_longer_valid()));
    };
    if one_click_asked {
        one_click(&state, &meta, &token).await?;
        // RFC 8058 §3.2: a 200 and nothing to read. The mail client is not a browser and shows
        // the person nothing, so there is no page to render.
        return Ok(with(quota.headers, StatusCode::OK.into_response()));
    }
    let answer = save(&state, &meta, &token, &body).await?;
    Ok(with(quota.headers, answer))
}

/// RFC 8058 §3.1 names the pair exactly. Case-insensitively on both halves, because the field name
/// comes from a header name and clients are not consistent about either.
///
/// An empty or unparseable body has no such pair, so it is treated as a preference form with every
/// box unticked. That is the safe way round: it unsubscribes from the lists the page would have
/// shown rather than silently doing nothing.
fn is_one_click(body: &str) -> bool {
    form_urlencoded::parse(body.as_bytes()).any(|(name, value)| {
        name.eq_ignore_ascii_case("list-unsubscribe") && value.eq_ignore_ascii_case("one-click")
    })
}

/// One list as the preference center shows it.
struct Topic {
    id: Uuid,
    name: String,
    status: Option<MembershipStatus>,
}

/// The address of the contact the token names, or `None` if it is no longer a contact here.
async fn contact_email(
    db: &mut PgConnection,
    workspace_id: Uuid,
    contact_id: Uuid,
) -> ApiResult<Option<String>> {
    sqlx::query_scalar!(
        "select email from contacts where workspace_id = $1 and id = $2",
        workspace_id,
        contact_id,
    )
    .fetch_optional(db)
    .await
    .map_err(Into::into)
}

/// Spec invariant 10, and the whole of it: the workspace's `public` lists, **plus** any non-public
/// list this contact already has a membership row on, **and nothing else**.
///
/// Both halves of that are failure modes the spec names as anti-goals. Showing a list somebody was
/// never on discloses the workspace's private lists to whoever holds the link; hiding one they are
/// on leaves them no way to leave it. A membership row of any status counts as "already on" — an
/// `unsubscribed` row is a list they knew about and may want back, and leaving it off the page
/// would make leaving a private list a one-way door.
///
/// **One query, not one per list.** The latency floor in spec §9 exists to catch an N+1 here,
/// which is the only way this page gets slow.
async fn topics(
    db: &mut PgConnection,
    workspace_id: Uuid,
    contact_id: Uuid,
) -> ApiResult<Vec<Topic>> {
    sqlx::query_as!(
        Topic,
        r#"select l.id, l.name, m.status as "status?: MembershipStatus"
             from lists l
             left join list_memberships m
               on m.workspace_id = l.workspace_id and m.list_id = l.id and m.contact_id = $2
            where l.workspace_id = $1 and (l.visibility = 'public' or m.contact_id is not null)
            order by lower(l.name), l.id"#,
        workspace_id,
        contact_id,
    )
    .fetch_all(db)
    .await
    .map_err(Into::into)
}

/// The page: a checkbox per list, ticked where the membership is `subscribed`, and a second form
/// that is the RFC 8058 body a mail client would have sent — so the "unsubscribe from everything"
/// button and the one-click POST are the same door, not two implementations of it.
///
/// **Where the escaping is, and what it is correct for.** `page::esc` is applied to the list name,
/// which an operator typed and which reaches element text only. The one attribute value on this
/// page is a list's `Uuid`, rendered from a `Uuid` and not from anything a person can type, and it
/// is quoted. `esc` is correct for both of those contexts; it would **not** be enough for an
/// unquoted attribute or for a value that becomes a URL, and neither exists here. Keep it that
/// way: a `title="{}"` added later is fine, a `href="{}"` is not without a scheme check.
///
/// No JavaScript, so the form works from an email client's browser with scripting off (spec §7.1).
///
/// **`saved` carries what changed, and the page says it in two places, because one of them is not
/// heard.** This form posts and the browser navigates: there is no script to update a region in
/// place, and a live region that is already in the document when it loads is not an update, so a
/// screen reader announces the new document — its `<title>` — and not the `role="status"` below.
/// So the title is the announcement: "Your preferences are saved" rather than the same "Your email
/// preferences" the page had before the submission, which is a page that reads as though nothing
/// happened. The region is kept for the reader that does announce one at load and for the day this
/// page is enhanced with script; the sentence inside it names the lists, because "your preferences
/// are saved" does not tell somebody who unticked one topic which one stopped.
///
/// **The `<h1>` does not follow the title, and that is why this calls `page::shell` rather than
/// `page::notice`.** A heading describes what is under it (WCAG 2.4.6), and what is under this one
/// is a form for changing preferences, not a receipt — so it stays "Your email preferences"
/// whichever way the page was reached. Letting it track the title would make the saved page say
/// "Your preferences are saved" twice in a row to a screen reader, once as the document's name and
/// once as the heading of a form that has saved nothing yet.
fn render(topics: &[Topic], saved: Option<&str>) -> Response {
    let mut body = String::new();
    if let Some(summary) = saved {
        body.push_str(&format!("<p role=\"status\">{}</p>\n", page::esc(summary)));
    }
    if topics.is_empty() {
        body.push_str("<p>There are no lists here for you to choose from.</p>\n");
    } else {
        body.push_str(
            "<p>Tick the emails you want. Anything you untick stops.</p>\n\
             <form method=\"post\">\n<ul class=\"topics\">\n",
        );
        for topic in topics {
            let checked = if topic.status == Some(MembershipStatus::Subscribed) {
                " checked"
            } else {
                ""
            };
            body.push_str(&format!(
                "<li><label><input type=\"checkbox\" name=\"list\" value=\"{}\"{checked}> {}\
                 </label></li>\n",
                topic.id,
                page::esc(&topic.name),
            ));
        }
        body.push_str("</ul>\n<button type=\"submit\">Save preferences</button>\n</form>\n");
    }
    body.push_str(
        "<form method=\"post\">\n<button type=\"submit\" name=\"List-Unsubscribe\" \
         value=\"One-Click\">Unsubscribe from everything</button>\n</form>\n",
    );
    let title = if saved.is_some() {
        "Your preferences are saved"
    } else {
        "Your email preferences"
    };
    page::shell(title, "Your email preferences", body)
}

/// Applies the form. The candidate set is **recomputed from the database inside the transaction**,
/// never taken from the body: a list id the page would not have shown — a private list this
/// contact was never on, or one deleted since the page was rendered — is simply not among the
/// candidates and is ignored. That is what makes Review Focus 3 a non-event in both directions,
/// and what stops invariant 10 being bypassed by posting an id.
///
/// **A tick subscribes outright, even on a double-opt-in list.** Everywhere else in M1d a
/// `double_optin` list produces a `pending` membership and a `requested` consent row. Here it must
/// not: double opt-in exists to prove the address belongs to the person using it, and a valid
/// token delivered to that address already is that proof. Asking them to confirm again asks them
/// to prove the same thing twice and leaves a `pending` row for somebody who has plainly agreed.
/// So nothing here reads `lists.double_optin`, and `Action::Requested` is unreachable from this
/// function.
///
/// The action is read off the status the row had **before** the write, which is why the candidate
/// query runs inside the transaction: a row that was not there is `Subscribed`, one that was
/// `pending` is `Confirmed` — the same word `GET /c/{token}` records for the identical transition
/// under the identical evidence — and one that was `unsubscribed` is `Resubscribed`. The SQL
/// guards (`status <> 'subscribed'`, `status <> 'unsubscribed'`) decide *whether* a row is
/// recorded, so a concurrent writer cannot produce a consent row for a change that did not happen.
async fn save(
    state: &AppState,
    meta: &RequestMeta,
    token: &Token,
    body: &str,
) -> ApiResult<Response> {
    let wanted: HashSet<Uuid> = form_urlencoded::parse(body.as_bytes())
        .filter(|(name, _)| name == "list")
        .filter_map(|(_, value)| value.parse().ok())
        .collect();

    let mut tx = state.pool.begin().await?;
    let Some(email) = contact_email(&mut tx, token.workspace_id, token.contact_id).await? else {
        return Ok(no_longer_valid());
    };
    let before = topics(&mut tx, token.workspace_id, token.contact_id).await?;
    let prior: HashMap<Uuid, Option<MembershipStatus>> =
        before.iter().map(|t| (t.id, t.status)).collect();
    let mut on = vec![];
    let mut off = vec![];
    for topic in &before {
        if wanted.contains(&topic.id) {
            if topic.status != Some(MembershipStatus::Subscribed) {
                on.push(topic.id);
            }
        } else if matches!(
            topic.status,
            Some(MembershipStatus::Pending | MembershipStatus::Subscribed)
        ) {
            off.push(topic.id);
        }
    }
    // Asked for at least one of the lists the page offered, whether or not it changed anything.
    let asked_for_any = before.iter().any(|topic| wanted.contains(&topic.id));

    // The workspace-wide opt-out was theirs to set through one-click, so it is theirs to clear by
    // asking for a list again — otherwise this page shows boxes that tick and change nothing a
    // send would honour. Only `unsubscribed_all` is lifted; a bounce, a complaint, an operator's
    // block and an erasure tombstone are not a subscriber's to overturn. No consent row: the
    // per-list rows below are the record of what they asked for, and there is no action in spec
    // §7.1's list that means "a block was lifted".
    //
    // **Before the membership writes, for the reason `one_click` sets out in full**: the erase
    // path takes the suppressions row and then the membership rows, so this has to take them in
    // that order too or the two deadlock.
    if asked_for_any {
        suppressions::lift_unsubscribed_all(&mut tx, &state.secrets, token.workspace_id, &email)
            .await?;
    }

    // **Unticking every box is not a workspace-wide opt-out, and that is a decision.** Unticking
    // means what it says: not these lists. Somebody who unticks four topics may well intend to
    // come back, and a `suppressions` row is permanent, survives erasure and covers lists that do
    // not exist yet — more than they said. Somebody who does want out entirely says so with the
    // "Unsubscribe from everything" button `render` puts on this page, which posts the RFC 8058
    // body and so goes through `one_click`, suppression and all. One door for "not this", one for
    // "none of it", and the durable one is the one that was asked for explicitly.
    let subscribed: Vec<Uuid> = sqlx::query_scalar!(
        r#"insert into list_memberships
               (workspace_id, contact_id, list_id, status, subscribed_at, source)
           select $1, $2, l, 'subscribed', now(), $4::text
             from unnest($3::uuid[]) l
           on conflict (contact_id, list_id) do update
              set status = 'subscribed', subscribed_at = now(), unsubscribed_at = null,
                  source = excluded.source
            where list_memberships.status <> 'subscribed'
           returning list_id"#,
        token.workspace_id,
        token.contact_id,
        &on[..],
        MembershipSource::PreferenceCenter as MembershipSource,
    )
    .fetch_all(&mut *tx)
    .await?;
    let unsubscribed: Vec<Uuid> = sqlx::query_scalar!(
        r#"update list_memberships set status = 'unsubscribed', unsubscribed_at = now()
            where workspace_id = $1 and contact_id = $2 and list_id = any($3)
              and status <> 'unsubscribed'
           returning list_id"#,
        token.workspace_id,
        token.contact_id,
        &off[..],
    )
    .fetch_all(&mut *tx)
    .await?;

    // What the saved page will say, built here because the two `Vec<Uuid>`s below are consumed by
    // the consent loops and because `before` is the only place the names are. They are the lists
    // the two statements actually changed, not the lists the body asked for: a box that was
    // already ticked is not news, and saying it is would make the page agree with the request
    // rather than with the database.
    let summary = summarise(&named(&before, &subscribed), &named(&before, &unsubscribed));

    // The subscriber's own request, so the IP and the user agent really are theirs and are kept as
    // evidence. `token_jti` is the nonce: *which* link was followed, not merely that one was.
    let cx = consent::Context {
        ip: meta.ip,
        user_agent: meta.user_agent.clone(),
        // What sent the link, when the link says: `ref_id` is M2's campaign or message id. M1d
        // issues no token carrying one, so this is `None` today and the column it fills is the
        // one `0020` built for "whatever caused the change".
        source_ref: token.ref_id.map(|id| id.to_string()),
        token_jti: Some(token.nonce.clone()),
    };
    for list_id in subscribed {
        let action = match prior.get(&list_id).copied().flatten() {
            None => consent::Action::Subscribed,
            Some(MembershipStatus::Pending) => consent::Action::Confirmed,
            Some(_) => consent::Action::Resubscribed,
        };
        record(&mut tx, token, list_id, action, &cx).await?;
    }
    for list_id in unsubscribed {
        record(&mut tx, token, list_id, consent::Action::Unsubscribed, &cx).await?;
    }

    let after = topics(&mut tx, token.workspace_id, token.contact_id).await?;
    tx.commit().await?;
    Ok(render(&after, Some(&summary)))
}

/// The names, in the order the page shows them, of the lists in `ids`.
fn named<'a>(topics: &'a [Topic], ids: &[Uuid]) -> Vec<&'a str> {
    topics
        .iter()
        .filter(|t| ids.contains(&t.id))
        .map(|t| t.name.as_str())
        .collect()
}

/// What the saved page says happened, as a sentence about lists rather than about a form.
///
/// Plain text: [`render`] puts it through `page::esc`, because these are names an operator typed.
fn summarise(on: &[&str], off: &[&str]) -> String {
    let mut parts = Vec::new();
    if !on.is_empty() {
        parts.push(format!("You will now get {}.", on.join(", ")));
    }
    if !off.is_empty() {
        parts.push(format!("You have unsubscribed from {}.", off.join(", ")));
    }
    if parts.is_empty() {
        // A save that changed nothing — the same boxes posted back, or a second submission of the
        // same form. "Your preferences are saved" on its own reads as though something happened.
        parts.push("Nothing changed.".to_owned());
    }
    parts.join(" ")
}

/// One consent row for one list this route changed, `Source::PreferenceCenter`.
async fn record(
    tx: &mut PgConnection,
    token: &Token,
    list_id: Uuid,
    action: consent::Action,
    cx: &consent::Context,
) -> ApiResult<()> {
    consent::record(
        tx,
        token.workspace_id,
        token.contact_id,
        Some(list_id),
        action,
        consent::Source::PreferenceCenter,
        cx,
    )
    .await
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
///
/// **The order of the two writes is load-bearing: the address first, the memberships second.**
/// `contacts::routes::erase_contact` takes `suppressions::lock_address`, writes the tombstone, and
/// only then deletes the contact — which cascades to `list_memberships` and takes those row locks.
/// Doing it the other way round here would be an ABBA cycle with that path, and Postgres resolves
/// one by aborting a side with `40P01`. If the victim were this POST the handler would answer
/// `500` and roll the sweep back, which is an RFC 8058 one-click that silently failed with nobody
/// to retry it — the one failure this route is least allowed to have. Taking the same locks in the
/// same order as the erase path costs nothing, because the suppression does not depend on what the
/// sweep returned.
async fn one_click(state: &AppState, meta: &RequestMeta, token: &Token) -> ApiResult<()> {
    let mut tx = state.pool.begin().await?;
    let email = sqlx::query_scalar!(
        "select email from contacts where workspace_id = $1 and id = $2",
        token.workspace_id,
        token.contact_id,
    )
    .fetch_optional(&mut *tx)
    .await?;
    // First, and before any membership row is locked: see the ordering paragraph above.
    let suppressed = match &email {
        Some(email) => {
            suppressions::unsubscribe_all(&mut tx, &state.secrets, token.workspace_id, email)
                .await?
        }
        None => false,
    };
    let changed: Vec<Uuid> = sqlx::query_scalar!(
        "update list_memberships set status = 'unsubscribed', unsubscribed_at = now() \
          where workspace_id = $1 and contact_id = $2 and status <> 'unsubscribed' \
         returning list_id",
        token.workspace_id,
        token.contact_id,
    )
    .fetch_all(&mut *tx)
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
        // As above: the token's `ref_id` when it carries one, which M1d never issues and M2 fills.
        source_ref: token.ref_id.map(|id| id.to_string()),
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
    // records nothing — the same rule the memberships follow. The row goes in here, after the
    // sweep, although the write it records happened before it: `created_at` is transaction-start
    // time for every row in this transaction, so the order they are inserted in says nothing.
    if suppressed {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The sentence the saved page announces. Every branch, because the empty one is the one a
    /// reader is most likely to meet — posting the same boxes back — and the one that would
    /// otherwise print "You will now get ." with nothing after it.
    #[test]
    fn summarise_names_what_changed_and_says_so_when_nothing_did() {
        assert_eq!(
            summarise(&["Weekly digest"], &[]),
            "You will now get Weekly digest."
        );
        assert_eq!(
            summarise(&[], &["News", "Offers"]),
            "You have unsubscribed from News, Offers."
        );
        assert_eq!(
            summarise(&["News"], &["Offers"]),
            "You will now get News. You have unsubscribed from Offers."
        );
        assert_eq!(summarise(&[], &[]), "Nothing changed.");
    }

    /// `named` follows the page's order and not the order the two statements returned rows in,
    /// which is the server's choice and not the reader's.
    #[test]
    fn named_keeps_the_order_the_page_shows() {
        let topic = |name: &str| Topic {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            status: None,
        };
        let topics = vec![topic("Alpha"), topic("Beta"), topic("Gamma")];
        let ids = vec![topics[2].id, topics[0].id];
        assert_eq!(named(&topics, &ids), ["Alpha", "Gamma"]);
    }
}
