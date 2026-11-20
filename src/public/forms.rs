// SPDX-License-Identifier: AGPL-3.0-only

//! `/f/{workspace}/{slug}` — the hosted signup page and the submission behind it. The most
//! exposed surface in the product: unauthenticated, reachable by anyone who can guess two slugs,
//! and the only route whose job includes **telling the caller nothing**.
//!
//! `{workspace}` is `workspaces.slug` and `{slug}` is the form's, which is unique per workspace
//! (spec §7.2, amended 2026-11-20): a single-segment path cannot serve two tenants who both call
//! a form `newsletter`.
//!
//! **The one answer.** Spec invariants 7 and 9 require `POST` to answer identically for a new
//! address, an existing subscriber, a suppressed address, a honeypot trip and a rate-limit
//! refusal. Every one of those paths ends at [`accepted`], and nothing between here and there may
//! branch the response on anything the submitter supplied. The two answers that are *not* that
//! page both depend on the form rather than on the address: [`not_found`] for two slugs that name
//! no form, and [`try_again`] for a submission whose own fields do not validate — neither can be
//! asked a question about an address and answer it.
//!
//! Nothing here reads the session; `tests/public_pages.rs` walks this directory to keep it that
//! way.

use std::collections::HashMap;

use axum::{
    Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::Utc;
use serde_json::{Map, Value};
use sqlx::PgConnection;
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    app::AppState,
    consent,
    contacts::{self, NewContact},
    error::{ApiError, ApiResult},
    forms::{self, Form, FormTarget},
    http::request_meta::RequestMeta,
    lists::{MembershipSource, MembershipStatus},
    public::{
        page::{self, UNKNOWN_IP, notice, with},
        token::{self, Purpose, Token},
    },
    suppressions,
};

pub fn routes() -> Router<AppState> {
    Router::new().route("/f/{workspace}/{slug}", get(show).post(submit))
}

/// The hidden input a script fills in and a person never sees. It is named for something a form
/// plausibly collects, because a field named `honeypot` is one `grep` away from being skipped.
///
/// It can never collide with a real field: [`input`] names every real one `email`, `first_name`,
/// `last_name`, `timezone` or `custom.<key>`, and a custom key is `[a-z][a-z0-9_]*`
/// (`custom_fields::validate_key`), so no operator can create a field that lands on this name.
const HONEYPOT: &str = "website";

/// Two slugs that name no form. A workspace that does not exist, a workspace whose form does not
/// exist, and another workspace's form are deliberately one answer — both slugs are guessable,
/// and telling the three apart would say which operators are customers here.
fn not_found() -> Response {
    notice(
        StatusCode::NOT_FOUND,
        "Page not found",
        "<p>There is no signup form at this address.</p>",
    )
}

/// `GET /f/{workspace}/{slug}` — the page itself.
///
/// **A tripped limit is a `429` here and the success page on `POST`, and the difference is the
/// point.** This GET is a person at a browser who can read a refusal and open the page again,
/// which is how `GET /c/{token}` and `GET /u/{token}` answer; it discloses nothing about any
/// address, because no address has been named yet. The `POST` below cannot answer this way
/// without telling a scraper it has been seen (spec §8.9).
async fn show(
    State(state): State<AppState>,
    meta: RequestMeta,
    Path((workspace, slug)): Path<(String, String)>,
) -> ApiResult<Response> {
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
                 <p class=\"muted\">Wait a minute and open the page again.</p>",
            ),
        )
            .into_response());
    }
    let mut db = state.pool.acquire().await?;
    let Some(form) = forms::by_slug(&mut db, &workspace, &slug).await? else {
        return Ok(with(quota.headers, not_found()));
    };
    Ok(with(quota.headers, render(&form, None)))
}

/// `POST /f/{workspace}/{slug}`.
///
/// **No `RateLimit-*` headers on this route, unlike every other limited route in this tree.**
/// They are the refusal's last remaining tell: `RateLimit-Remaining: 0` on the answer to a
/// submission says "you have been counted and this one did nothing", which is exactly what spec
/// §8.9 forbids this route from saying. Nothing is lost by dropping them — the caller this route
/// serves is a browser posting a form, not a client that can back off — and the request is still
/// counted, because `check` is still called. `GET` above keeps them.
///
/// The limiter's verdict is read **before** the form is looked up and obeyed **after**: the
/// answer a refused submission gives is this form's own success page, so the form has to be
/// loaded to produce it.
///
/// **So the limiter sheds no database load here**, unlike every route that refuses early: a
/// refused submission still runs `by_slug` and `lists_of`. Two indexed reads, bounded at 60 a
/// minute per address — but a flood from many addresses is bounded by the number of addresses and
/// not by this limiter, and the pool is what would feel it. Shedding earlier would mean answering
/// a refusal with something that is not this form's page, which is the one thing spec §8.9
/// forbids.
async fn submit(
    State(state): State<AppState>,
    meta: RequestMeta,
    Path((workspace, slug)): Path<(String, String)>,
    body: String,
) -> ApiResult<Response> {
    let over = state.public_limit.check(meta.ip.unwrap_or(UNKNOWN_IP)).over;
    let mut db = state.pool.acquire().await?;
    let Some(form) = forms::by_slug(&mut db, &workspace, &slug).await? else {
        return Ok(not_found());
    };
    // The lists as they are **now**. `form_lists.list_id` cascades, so a form validated with "at
    // least one list" can have none left (`0023_forms_and_jobs.sql`); that form accepts no
    // submission and answers exactly as every other refusal does.
    let lists = lists_of(&mut db, &form).await?;

    let submitted = parse(&body);
    // **The shape is checked here, before the limiter's verdict and the honeypot are allowed to
    // decide anything.** It is the second half of the same requirement that moved this check
    // ahead of the address lookups: if a refused submission skipped validation, the same bytes in
    // would get two answers out — the error page under the limit, the success page over it — and
    // that is precisely the "you have been counted" tell this route drops its `RateLimit-*`
    // headers to avoid. Everything the check reads is the submission and the workspace's custom
    // field definitions, so it can answer differently without answering a question about the
    // address.
    //
    // It costs a refused submission one more indexed read; see the load note above.
    let checked =
        match contacts::check(&mut db, form.workspace_id, new_contact(&form, &submitted)).await {
            Ok(checked) => checked,
            // A malformed address, a name too long, a custom field that will not take what was typed:
            // the shape of what was submitted, which the submitter can see and fix.
            Err(ApiError::Validation(_)) => {
                return Ok(render(&form, Some("Check the details and try again.")));
            }
            Err(e) => return Err(e),
        };
    drop(db);

    // A script fills in every input it can see. A person cannot see this one, so a value in it
    // means the submission was not made by a person — and the answer is the success page, so the
    // script has nothing to learn and no reason to come back with it empty.
    let honeypot = !first(&submitted, HONEYPOT).is_empty();
    if over.is_none() && !honeypot {
        apply(&state, &meta, &form, &lists, &checked).await?;
    }
    Ok(accepted(&form, &lists))
}

/// Writes the submission. Every early return writes nothing and answers as a success: a
/// suppressed address, an erased one and an address another request created a moment ago are all
/// the one page (the transaction is dropped, so it rolls back).
///
/// There is no "invalid" answer here. The shape was checked in [`submit`], before the limiter and
/// the honeypot, so by the time this runs the only questions left are ones about the address —
/// and every answer to one of those is the same page.
///
/// **The order of the writes matters.** `suppressions::is_erased`, inside `contacts::create`,
/// takes an advisory lock on the address; `contacts::routes::erase_contact` takes the same lock,
/// writes the tombstone, and only then deletes the contact — which cascades to
/// `list_memberships`. Taking the address before the membership rows, as this does, is taking
/// them in the same order as the erase path, so the two cannot deadlock.
async fn apply(
    state: &AppState,
    meta: &RequestMeta,
    form: &Form,
    lists: &[(Uuid, bool)],
    checked: &contacts::Checked,
) -> ApiResult<()> {
    // A form with no lists left subscribes nobody, so it writes nothing at all — not even the
    // contact row. `form_lists.list_id` cascades, so this is a live state and not a hypothetical
    // (`0023_forms_and_jobs.sql`), and a contact with no membership and no consent row is the
    // wrong artefact for this product to leave behind: nothing would say where it came from or
    // what it agreed to. It is decided by the form's configuration, which is the same for every
    // submitter, so it tells nobody anything.
    if lists.is_empty() {
        return Ok(());
    }

    let email = checked.email();
    let mut tx = state.pool.begin().await?;
    // **A bounce is let through; every other block refuses.** A workspace-wide opt-out
    // (`docs/adr/0001`), a complaint, an operator's manual block and an erasure tombstone are all
    // consent decisions, and a public form is the one caller with no operator behind it to
    // overrule one. `Bounced` is not a consent decision — it is a deliverability fact, with no
    // door a subscriber can knock on, and somebody re-typing their address into a signup form is
    // usually fixing exactly that. The double opt-in confirmation is itself the deliverability
    // test, so letting them past costs one email to an address that may fail again.
    //
    // **It asks "is there a block that is not a bounce", not "what is the strongest block".**
    // `blocking_suppression` answers the second question, `order by … limit 1` with workspace
    // before instance and address before domain, so reading the reason off its answer would let a
    // workspace-scope bounce *mask* an instance-wide complaint or a domain block — and the
    // live-uniqueness index does not forbid that combination, it only forbids two rows in one
    // slot. `is_suppressed_excluding_bounce` tests every row instead, and a reason this build has
    // never seen counts as blocking.
    //
    // shortcut: this early return is a **timing** oracle, and byte-identity does not close it.
    // Measured on the dev container, fastest of five, warm: a new address 8.1-9.1 ms, a
    // suppressed one 2.7-3.0 ms — the suppressed path skips the contact insert, two membership
    // statements, the consent row and the `jobs` row. About 5 ms and a factor of three: not
    // visible in one sample over the internet, and perfectly visible in a few hundred. Closing it
    // means spending the same time either way — a fixed deadline the handler sleeps to, which
    // costs every honest submission the slowest path's latency and has to be re-tuned whenever
    // the writes change. Do that when an existence oracle at this resolution is worth 5 ms to
    // somebody; until then it is recorded here rather than unmeasured.
    if suppressions::is_suppressed_excluding_bounce(
        &mut *tx,
        &state.secrets,
        form.workspace_id,
        email,
    )
    .await?
    {
        return Ok(());
    }
    let contact_id = match contact_id(&mut tx, form.workspace_id, email).await? {
        Some(id) => id,
        None => {
            match contacts::create_checked(&mut tx, &state.secrets, form.workspace_id, checked)
                .await
            {
                Ok(id) => id,
                // `email-taken`: another submission of the same address committed between the select
                // above and this insert. The unique violation has already aborted this transaction,
                // so there is nothing to do but roll it back — and nothing is lost, because the
                // request that won wrote what this one would have.
                // `address-erased`: the erasure tombstone is a `gdpr_erasure` suppression, which
                // the check above already refuses, so this is unreachable — and it is the second
                // guard either way, which is why the check above being wrong about erasure would
                // still not create a contact. Kept as an arm rather than an `unreachable!`,
                // because a panic in this handler is a 500 for a stranger.
                Err(ApiError::Conflict(_)) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    };

    // Read inside the transaction, before the writes. It decides which lists are waiting on a
    // confirmation email, and nothing else: the SQL guards below decide what is written.
    let prior = statuses(&mut tx, form.workspace_id, contact_id, lists).await?;
    let double: Vec<Uuid> = lists.iter().filter(|l| l.1).map(|l| l.0).collect();
    let single: Vec<Uuid> = lists.iter().filter(|l| !l.1).map(|l| l.0).collect();

    // **Both statements below are `do nothing` on conflict: a submission creates a membership or
    // it changes nothing.** Anyone can type anyone's address into a public form, so every
    // transition this route could make to an *existing* row is a transition made by a stranger.
    // That is plainly right for `unsubscribed` (invariant 1), and it is right for `pending` too:
    // promoting a pending membership is confirmation, confirmation is the token's job, and a
    // `confirmed` consent row with no `token_jti` written because somebody guessed an address is
    // a forged record of consent in an append-only log. It stays `pending`; whether another
    // confirmation email goes out is `RESEND_AFTER_MINUTES`'s decision below.
    //
    // `Action::Confirmed` and `Action::Resubscribed` are therefore both unreachable from this
    // module, and that is the invariant, not an omission.
    let requested: Vec<Uuid> = sqlx::query_scalar!(
        "insert into list_memberships (workspace_id, contact_id, list_id, status, source) \
         select $1, $2, l, 'pending', $4::text from unnest($3::uuid[]) l \
         on conflict (contact_id, list_id) do nothing \
         returning list_id",
        form.workspace_id,
        contact_id,
        &double[..],
        MembershipSource::Form as MembershipSource,
    )
    .fetch_all(&mut *tx)
    .await?;

    // Single opt-in: `subscribed` outright, and only where there was no row at all.
    let subscribed: Vec<Uuid> = sqlx::query_scalar!(
        "insert into list_memberships \
             (workspace_id, contact_id, list_id, status, subscribed_at, source) \
         select $1, $2, l, 'subscribed', now(), $4::text from unnest($3::uuid[]) l \
         on conflict (contact_id, list_id) do nothing \
         returning list_id",
        form.workspace_id,
        contact_id,
        &single[..],
        MembershipSource::Form as MembershipSource,
    )
    .fetch_all(&mut *tx)
    .await?;

    // The submitter's own request, so the IP and the user agent really are theirs and are kept as
    // evidence — `consent_log` is the table an erasure cascades, which is why they go here and
    // **nowhere else**. Nothing on this path writes an `audit_log` row: that table has no foreign
    // keys and nothing deletes from it, so a subscriber's IP in it would outlive their erasure
    // (`0020_consent_log.sql`). `contacts::create` does not audit — the authenticated route above
    // it does — so reusing it here carries nothing across.
    //
    // `token_jti` is `None`: no link was followed. `source_ref` is the form's id, and the audit
    // row for every write to that form carries the whole definition, so what the form asked for
    // at the moment of consent is recoverable years later.
    let cx = consent::Context {
        ip: meta.ip,
        user_agent: meta.user_agent.clone(),
        source_ref: Some(form.id.to_string()),
        token_jti: None,
    };
    for list_id in &requested {
        consent::record(
            &mut tx,
            form.workspace_id,
            contact_id,
            Some(*list_id),
            consent::Action::Requested,
            consent::Source::Form,
            &cx,
        )
        .await?;
    }

    // Every list this contact is `pending` on now: the ones just asked for, **and** the ones that
    // were already waiting for a confirmation. The second group is the resend, and it writes no
    // consent row — nothing changed, and `requested` was recorded when they were first asked.
    // `prior` was read inside this transaction before the writes, so this needs no second query.
    //
    // It is every list and not only the double-opt-in ones, because a membership can be `pending`
    // on a list that is single opt-in today: the flag is mutable, and `GET /c/{token}` promotes a
    // pending row whatever the list now says. Leaving those out would make a row that only a
    // token can settle one that nothing can ever send a token for.
    let waiting = requested.iter().copied().chain(
        lists
            .iter()
            .map(|l| l.0)
            .filter(|l| prior.get(l) == Some(&MembershipStatus::Pending)),
    );
    for list_id in waiting {
        enqueue_confirmation(
            &mut tx,
            state,
            form.workspace_id,
            contact_id,
            list_id,
            email,
        )
        .await?;
    }
    for list_id in subscribed {
        // Always `Subscribed`: the statement above only returns rows it inserted, so every one of
        // these is a membership that did not exist a moment ago.
        consent::record(
            &mut tx,
            form.workspace_id,
            contact_id,
            Some(list_id),
            consent::Action::Subscribed,
            consent::Source::Form,
            &cx,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// How long one `(contact, list)` must wait between confirmation emails, in minutes.
///
/// **It exists because the alternative was a dead end that reports success.** Until this was
/// added, a submission naming an address whose membership was already `pending` enqueued nothing
/// — one confirmation per address and list, ever — and still answered "Check your email". A
/// person whose confirmation went to spam, or who mistyped and fixed it, had no way left to
/// confirm, and the page told them to wait for an email that would never come. Re-submitting the
/// form is the resend this milestone has: spec §8.6 has an expired confirmation offer to re-send,
/// and `public::confirm`'s expired page says in so many words that signing up again is how.
///
/// **What the window is for.** This route is unauthenticated, so the address in the box need not
/// belong to whoever typed it. With no throttle, a submission loop sends a victim one email per
/// request — 60 a minute, the per-IP limit, from each address an attacker has. Fifteen minutes
/// keyed on the contact and the list caps that at four an hour per list however many IPs are
/// used, which is what an ordinary signup form on the internet permits, and is longer than
/// anybody waits before retrying a lost email. It is **not** keyed on the IP: the victim is the
/// address, and an attacker chooses their own address freely (`PUBLIC_PER_MINUTE` explains why
/// that bound is a different kind of bound).
///
/// Nothing stores this but the `jobs` rows themselves, so there is no state to expire and nothing
/// to clean up. That is also why those rows may not be deleted while they are young: `0026`'s
/// `jobs_throttle_retention` trigger refuses a delete inside a day, which is a superset of this
/// window. Raising this constant past a day means raising the trigger's interval with it.
const RESEND_AFTER_MINUTES: i64 = 15;

/// One `jobs` row per double-opt-in list, each carrying **its own** `Purpose::Confirm` token:
/// `token::parse` requires a `list_id` on exactly the confirm tokens, so one token cannot cover
/// two lists. A form on two double-opt-in lists honestly sends two emails — they are two
/// consents.
///
/// **The contact and the list are columns; the payload carries what is genuinely free-form.**
/// `0026` promoted them out of the `jsonb` so that the database can see them: composite foreign
/// keys carrying `workspace_id` make a cross-tenant job unwritable, the contact key cascades so a
/// GDPR erasure reaches this table, and the throttle below gets an index. The payload is
/// `{"email", "token"}`, and the table comment on `jobs` is the contract M2 reads.
///
/// The payload keeps the already-issued token **string**, beside the token and not inside it:
/// `public::token::Token` is `deny_unknown_fields` and `Purpose::Unsubscribe` has no TTL, so
/// anything a job needs travels alongside. Keeping it was re-decided in the `0026` wave against
/// having M2 re-issue the token at send time: with the contact key cascading, the credential now
/// dies with the person rather than outliving their erasure; a `Confirm` token is self-expiring
/// where an unsubscribe token is not, so a backup of this table goes inert in fourteen days; and
/// it is what lets this milestone prove end to end that a queued row yields a link the confirm
/// route accepts, which re-issuing elsewhere would replace with a test that mints its own.
///
/// **Enqueues nothing if this `(contact, list)` already had one inside [`RESEND_AFTER_MINUTES`]**,
/// decided in the same statement as the insert rather than by a read followed by a write, so a
/// second submission cannot slip between the two. Two genuinely concurrent submissions can still
/// both insert, since neither transaction sees the other's uncommitted row; the result is two
/// emails instead of one, which is the bound being off by one and not the bound being absent.
///
/// **The question is an index probe, and its cost does not depend on the rest of the instance.**
/// `jobs_confirmation_resend` is `(workspace_id, contact_id, list_id, created_at desc)` partial on
/// `kind = 'confirmation_email'` (`0026`), which is this predicate exactly. Until that index
/// existed the only one serving it was `jobs_pkey` with a UUIDv7 lower bound standing in for
/// `created_at`, and that bound means "every row enqueued instance-wide in the last fifteen
/// minutes" — read by an unauthenticated route, so one tenant's campaign send set the cost of
/// another tenant's signup form. The 50,000-row figure that used to stand here (2 buffers,
/// 0.045 ms) was measured on M1d's own trickle, where fifteen minutes holds nothing.
///
/// Re-measured on the shape this route will actually meet: 50,001 rows over 30 days, of which
/// 20,001 fall in the fifteen-minute window because the tenant next door is sending a campaign.
/// Both predicates run for the same submitting tenant, and each is measured twice — once where
/// the window holds a row for this (contact, list), and once where it does not, which is the
/// ordinary path and the one an attacker takes.
///
/// | predicate | plan | buffers | time |
/// |---|---|---|---|
/// | this one, resend | index-only on `jobs_confirmation_resend` | 5 | 0.049 ms |
/// | this one, first signup | index-only on `jobs_confirmation_resend` | 3 | 0.067 ms |
/// | the old one, resend | bitmap on `jobs_pkey` | 841 | 3.6 ms |
/// | the old one, first signup | bitmap on `jobs_pkey` | 841 | 4.0 ms |
///
/// The old figures are the window, so they rise with the instance's send volume and are paid by
/// whichever tenant submits a form next. The new ones do not move: the index is entered at this
/// workspace, this contact and this list.
///
/// **What it counts: every `confirmation_email` row for this pair in the window, whatever state
/// it is in** — queued, claimed, completed or given up. That is deliberate and it is the reason
/// `0025_jobs_completed_at.sql` exists: this throttle has no state of its own, so a worker that
/// deleted a row on success would make the question answer "no" every time and turn an
/// unauthenticated route back into a mailer, with nothing going red. The predicate names no
/// `completed_at` and no `failed_at` precisely so that adding one later is a visible decision
/// rather than a silent narrowing.
async fn enqueue_confirmation(
    tx: &mut PgConnection,
    state: &AppState,
    workspace_id: Uuid,
    contact_id: Uuid,
    list_id: Uuid,
    email: &str,
) -> ApiResult<()> {
    let token = token::issue(
        &state.secrets,
        &Token {
            workspace_id,
            contact_id,
            purpose: Purpose::Confirm,
            list_id: Some(list_id),
            issued_at: Utc::now().timestamp(),
            nonce: Uuid::now_v7().to_string(),
            // M1d has one key and nothing to attribute a link to. Both absent, so the payload is
            // the shape every link issued before these fields existed already had.
            kid: None,
            ref_id: None,
        },
    );
    let cutoff = Utc::now() - chrono::Duration::minutes(RESEND_AFTER_MINUTES);
    sqlx::query!(
        "insert into jobs (id, workspace_id, kind, contact_id, list_id, payload) \
         select $1, $2, 'confirmation_email', $4, $5, $3 \
          where not exists (select 1 from jobs \
                             where workspace_id = $2 and kind = 'confirmation_email' \
                               and contact_id = $4 and list_id = $5 \
                               and created_at > $6)",
        Uuid::now_v7(),
        workspace_id,
        serde_json::json!({ "email": email, "token": token }),
        contact_id,
        list_id,
        cutoff,
    )
    .execute(tx)
    .await?;
    Ok(())
}

/// The form's lists and each one's `double_optin`, which is **the list's** decision and never the
/// form's (spec §7.3). A form on one of each produces a `pending` membership and a `subscribed`
/// one from a single submission.
async fn lists_of(db: &mut PgConnection, form: &Form) -> ApiResult<Vec<(Uuid, bool)>> {
    let rows = sqlx::query!(
        "select id, double_optin from lists where workspace_id = $1 and id = any($2)",
        form.workspace_id,
        &form.list_ids[..],
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.double_optin)).collect())
}

/// The contact with this address in this workspace, if there is one.
async fn contact_id(
    db: &mut PgConnection,
    workspace_id: Uuid,
    email: &str,
) -> ApiResult<Option<Uuid>> {
    sqlx::query_scalar!(
        "select id from contacts where workspace_id = $1 and email = $2",
        workspace_id,
        email,
    )
    .fetch_optional(db)
    .await
    .map_err(Into::into)
}

/// The status this contact has on each of the form's lists, before the writes.
async fn statuses(
    db: &mut PgConnection,
    workspace_id: Uuid,
    contact_id: Uuid,
    lists: &[(Uuid, bool)],
) -> ApiResult<HashMap<Uuid, MembershipStatus>> {
    let ids: Vec<Uuid> = lists.iter().map(|l| l.0).collect();
    let rows = sqlx::query!(
        r#"select list_id, status as "status: MembershipStatus" from list_memberships
            where workspace_id = $1 and contact_id = $2 and list_id = any($3)"#,
        workspace_id,
        contact_id,
        &ids[..],
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| (r.list_id, r.status)).collect())
}

/// What the submission said, by input name. A list per name because a `<select multiple>` sends
/// its name once per chosen value.
fn parse(body: &str) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (name, value) in form_urlencoded::parse(body.as_bytes()) {
        out.entry(name.into_owned())
            .or_default()
            .push(value.into_owned());
    }
    out
}

fn first<'a>(submitted: &'a HashMap<String, Vec<String>>, name: &str) -> &'a str {
    submitted
        .get(name)
        .and_then(|values| values.first())
        .map_or("", |value| value.trim())
}

/// The contact to create from what was submitted. Blank inputs are left out rather than written
/// as empty values, so a form whose optional fields were skipped does not overwrite anything with
/// nothing.
///
/// `contacts::check` validates all of it, the address included: this function decides only
/// *which* input feeds which column. An unknown input name — anything the page did not render —
/// is ignored by construction, because this walks the form's fields rather than the body.
///
/// **It is only ever used to create a contact, never to update one.** A submission naming an
/// address that is already a contact writes no field of theirs, so a stranger who guesses an
/// address cannot overwrite that person's name, time zone or custom data from a public form. That
/// falls out of `apply` taking the existing id and stopping, and it is written down here because
/// a later "it would be helpful to keep their details fresh" is how it would be undone.
fn new_contact(form: &Form, submitted: &HashMap<String, Vec<String>>) -> NewContact {
    let mut new = NewContact {
        email: first(submitted, "email").to_owned(),
        ..NewContact::default()
    };
    let mut attributes = Map::new();
    for field in &form.fields {
        let (name, _, _) = input(&field.target);
        let value = first(submitted, &name);
        if value.is_empty() {
            continue;
        }
        match &field.target {
            FormTarget::Email => {}
            FormTarget::FirstName => new.first_name = Some(value.to_owned()),
            FormTarget::LastName => new.last_name = Some(value.to_owned()),
            FormTarget::Timezone => new.timezone = Some(value.to_owned()),
            FormTarget::Custom(key) => {
                attributes.insert(key.clone(), Value::from(value));
            }
        }
    }
    if !attributes.is_empty() {
        new.attributes = Some(Value::Object(attributes));
    }
    new
}

/// One input's `name`, `type` and `autocomplete`.
///
/// The name is the contract between the hosted page and the embed snippet an operator pastes into
/// their own site (spec §7.3), so it is derived from the target and never from the label. Custom
/// fields are namespaced under `custom.` — a key cannot contain a `.`, so no custom field can
/// collide with a built-in name or with the honeypot.
///
/// `autocomplete` is WCAG 2.2 success criterion 1.3.5: an input that asks for the person's own
/// details has to say which.
///
/// shortcut: every input is a text box except the address. A custom field of kind `number`,
/// `boolean`, `date` or `select` is typed into as text and validated by `contacts::create`, which
/// will refuse what does not fit — so the page says "check the details" where a checkbox or a
/// `<select>` would have made the mistake impossible. Rendering those needs the workspace's
/// custom-field *definitions* on this path, which nothing here loads today; load them when the
/// admin UI starts offering non-text fields on a form.
fn input(target: &FormTarget) -> (String, &'static str, &'static str) {
    match target {
        FormTarget::Email => ("email".to_owned(), "email", "email"),
        FormTarget::FirstName => ("first_name".to_owned(), "text", "given-name"),
        FormTarget::LastName => ("last_name".to_owned(), "text", "family-name"),
        FormTarget::Timezone => ("timezone".to_owned(), "text", "off"),
        FormTarget::Custom(key) => (format!("custom.{key}"), "text", "off"),
    }
}

/// The page, with `error` shown above the form when there is one.
///
/// **Where the escaping is.** Every operator-supplied value here — the headline, the button label,
/// each field's label — reaches element text or a quoted attribute value, and goes through
/// `page::esc`, which is correct for exactly those two contexts. The input names are built by
/// [`input`] from a fixed set plus a validated custom-field key, and are escaped anyway rather
/// than relying on the reader to know that.
///
/// No JavaScript: the form posts to itself and works with scripting off (spec §7.3).
fn render(form: &Form, error: Option<&str>) -> Response {
    let mut body = String::new();
    if let Some(error) = error {
        body.push_str(&format!("<p>{}</p>\n", page::esc(error)));
    }
    body.push_str("<form method=\"post\">\n");
    for field in &form.fields {
        let (name, kind, autocomplete) = input(&field.target);
        let name = page::esc(&name);
        body.push_str(&format!(
            "<p><label for=\"{name}\">{label}</label><br>\
             <input id=\"{name}\" name=\"{name}\" type=\"{kind}\" \
             autocomplete=\"{autocomplete}\"{required}></p>\n",
            label = page::esc(&field.label),
            required = if field.required { " required" } else { "" },
        ));
    }
    // `aria-hidden` and `tabindex="-1"` keep it away from a screen reader and out of the tab
    // order, so the decoy costs a person using either nothing at all. The CSS that hides it is in
    // `page::STYLE`.
    //
    // **The label says what to do with it, because sometimes it is visible.** Reader mode, a
    // style-stripping extension and a restrictive in-app webview all drop the inline `<style>`,
    // and this field then appears to a sighted person like any other. Labelled "Website" it reads
    // as a plausible optional field, they fill it in, and their submission is silently discarded
    // — which is the one failure this control must never produce. A script matches on the input's
    // `name`, which is unchanged, and does not read the label.
    body.push_str(&format!(
        "<div class=\"hp\" aria-hidden=\"true\">\
         <label for=\"{HONEYPOT}\">Leave this field empty</label>\
         <input id=\"{HONEYPOT}\" name=\"{HONEYPOT}\" type=\"text\" tabindex=\"-1\" \
         autocomplete=\"off\"></div>\n"
    ));
    body.push_str(&format!(
        "<button type=\"submit\">{}</button>\n</form>\n",
        page::esc(label_or(&form.button_label, "Subscribe")),
    ));
    notice(StatusCode::OK, label_or(&form.headline, "Sign up"), &body)
}

/// The one answer a submission gets.
///
/// **Reached by five paths that must not be told apart** (spec invariants 7 and 9): a new
/// address, an existing subscriber, a suppressed address, a honeypot trip and a rate-limit
/// refusal. Everything it reads is the form's own configuration, which is the same for every
/// submitter, and nothing it reads came from the submission.
///
/// *Byte-identical is necessary and not sufficient, and the gap is timing:* the suppressed and
/// honeypot paths stop before the writes a new address makes, so they answer sooner. The measured
/// difference and what closing it would cost are on the suppression branch in [`apply`].
///
/// **`redirect_url` is a link on this page and not a `303`, because a `303` does not work.**
/// `http::security_headers` sends `form-action 'self'` on every response, and Chromium checks a
/// redirect that follows a form POST against it. Measured through the e2e harness
/// (`web/e2e/public-form.spec.ts`, Playwright 1.65, Chromium): the POST is made, the server
/// answers `303`, and the navigation is refused with
///
/// > Sending form data to 'http://localhost:8090/f/formsco/redirecting' violates the following
/// > Content Security Policy directive: "form-action 'self'". The request has been blocked.
///
/// — naming the form's own URL, not the redirect's, which is why this reads as a policy bug
/// rather than a redirect bug until you run it. The submission still commits; only the
/// subscriber's last step is lost, silently, in the browser. The alternative was widening
/// `form-action`, which weakens the policy for every page in the product to fix one route, so it
/// was not taken. The e2e spec is the regression test: if a browser ever stops checking
/// redirects, it fails and this can be revisited.
fn accepted(form: &Form, lists: &[(Uuid, bool)]) -> Response {
    // The copy follows the form's lists, not the submitter: a form on a double-opt-in list sends
    // an email that has to be opened, and one on single-opt-in lists does not. Both are the same
    // page for every address.
    let (title, mut body) = if lists.iter().any(|l| l.1) {
        (
            "Check your email",
            "<p>Thank you. If there is anything left to do, it is in an email on its way to the \
             address you gave.</p>\n<p class=\"muted\">Open the link in it to finish signing \
             up.</p>\n"
                .to_owned(),
        )
    } else {
        (
            "Thank you",
            "<p>You are signed up.</p>\n<p class=\"muted\">Every email you get from here has an \
             unsubscribe link in it.</p>\n"
                .to_owned(),
        )
    };
    if let Some(url) = &form.redirect_url {
        // `esc` is doing one job here: the quotes that would end this attribute. It is not a URL
        // sanitiser and does not need to be — `forms::validate_redirect_url` stores the URL
        // parser's own serialisation of an absolute `http`/`https` URL, so a `javascript:` URL
        // cannot be in this column and the characters that would break out are percent-encoded
        // already. Do not add a second check here; add to that one if it is ever wrong.
        body.push_str(&format!(
            "<p><a href=\"{}\">Continue</a></p>\n",
            page::esc(url)
        ));
    }
    notice(StatusCode::OK, title, &body)
}

/// An operator's copy, or the built-in wording when they left it blank. `forms` defaults both
/// columns to the empty string.
fn label_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::{HONEYPOT, input};
    use crate::forms::FormTarget;

    /// The honeypot's name has to be one no operator can take, or a workspace with a custom field
    /// called `website` would trip it on every honest submission.
    #[test]
    fn no_real_input_can_be_named_like_the_honeypot() {
        let targets = [
            FormTarget::Email,
            FormTarget::FirstName,
            FormTarget::LastName,
            FormTarget::Timezone,
            FormTarget::Custom(HONEYPOT.to_owned()),
        ];
        for target in &targets {
            assert_ne!(input(target).0, HONEYPOT, "{target:?}");
        }
        // And the namespacing is what does it: a custom key cannot contain a `.`
        // (`custom_fields::validate_key`), so no key reaches a built-in name either.
        assert_eq!(
            input(&FormTarget::Custom("email".to_owned())).0,
            "custom.email"
        );
    }
}
