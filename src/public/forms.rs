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
    users::normalize_email,
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
    drop(db);

    let submitted = parse(&body);
    // A script fills in every input it can see. A person cannot see this one, so a value in it
    // means the submission was not made by a person — and the answer is the success page, so the
    // script has nothing to learn and no reason to come back with it empty.
    let honeypot = !first(&submitted, HONEYPOT).is_empty();
    if over.is_none()
        && !honeypot
        && matches!(
            apply(&state, &meta, &form, &lists, &submitted).await?,
            Outcome::Invalid
        )
    {
        return Ok(render(&form, Some("Check the details and try again.")));
    }
    Ok(accepted(&form, &lists))
}

/// What a submission did, where the two answers differ. Only [`Outcome::Invalid`] is reachable
/// from what the submitter typed — and only from its **shape**, never from anything the database
/// knows about the address, which is what keeps invariant 7 true.
enum Outcome {
    Done,
    Invalid,
}

/// Writes the submission. Every early return is `Done`: a suppressed address, an erased one and
/// an address another request created a moment ago all answer as a success, and all write nothing
/// (the transaction is dropped, so it rolls back).
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
    submitted: &HashMap<String, Vec<String>>,
) -> ApiResult<Outcome> {
    let Ok(email) = normalize_email(first(submitted, "email")) else {
        return Ok(Outcome::Invalid);
    };
    let mut tx = state.pool.begin().await?;
    // `is_suppressed`, not `blocking_suppression`'s narrower cousins: a workspace-wide opt-out
    // (`docs/adr/0001`), a complaint, a bounce, an operator's block and an erasure tombstone all
    // mean "do not mail this address", and a public form is the one caller with no operator
    // behind it to overrule that. It fails closed on an address it cannot normalise, which is
    // unreachable here because `normalize_email` above already refused those.
    //
    // shortcut: this early return is a **timing** oracle, and byte-identity does not close it.
    // Measured on the dev container, fastest of five, warm: a new address 6.2-7.0 ms, a
    // suppressed one 2.0 ms — the suppressed path skips the contact insert, two membership
    // statements, the consent row and the `jobs` row. About 4 ms and a factor of three: not
    // visible in one sample over the internet, and perfectly visible in a few hundred. Closing it
    // means spending the same time either way — a fixed deadline the handler sleeps to, which
    // costs every honest submission the slowest path's latency and has to be re-tuned whenever
    // the writes change. Do that when an existence oracle at this resolution is worth 5 ms to
    // somebody; until then it is recorded here rather than unmeasured.
    if suppressions::is_suppressed(&mut *tx, &state.secrets, form.workspace_id, &email).await? {
        return Ok(Outcome::Done);
    }
    let contact_id = match contact_id(&mut tx, form.workspace_id, &email).await? {
        Some(id) => id,
        None => match contacts::create(
            &mut tx,
            &state.secrets,
            form.workspace_id,
            new_contact(form, submitted, &email),
        )
        .await
        {
            Ok(id) => id,
            // `email-taken`: another submission of the same address committed between the select
            // above and this insert. The unique violation has already aborted this transaction,
            // so there is nothing to do but roll it back — and nothing is lost, because the
            // request that won wrote what this one would have.
            // `address-erased`: unreachable, since the tombstone is a suppression and the check
            // above found it. Kept as an arm rather than an `unreachable!`, because a panic in
            // this handler is a 500 for a stranger.
            Err(ApiError::Conflict(_)) => return Ok(Outcome::Done),
            // A name too long, a custom field that will not take what was typed. The shape of
            // what was submitted, which the submitter can see and fix.
            Err(ApiError::Validation(_)) => return Ok(Outcome::Invalid),
            Err(e) => return Err(e),
        },
    };

    // Read inside the transaction, and used only to *name* what the writes below did: the SQL
    // guards are what decide whether anything happened. Same shape as the preference center.
    let prior = statuses(&mut tx, form.workspace_id, contact_id, lists).await?;
    let double: Vec<Uuid> = lists.iter().filter(|l| l.1).map(|l| l.0).collect();
    let single: Vec<Uuid> = lists.iter().filter(|l| !l.1).map(|l| l.0).collect();

    // Double opt-in: `pending`, and `do nothing` if there is any row already. That guard is
    // invariant 1 — a membership someone unsubscribed from is not revived by a form naming their
    // address — and it is also what bounds this route as a mailer: one confirmation email per
    // (address, list), ever, however many times the form is submitted.
    //
    // shortcut: a person who loses the confirmation email therefore cannot ask for another, and
    // the form is where spec §6 says they would. Give the resend its own throttle keyed on the
    // address rather than the IP before relaxing this — without one, an unauthenticated route
    // that re-enqueues on every submission will send a victim 60 emails a minute.
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

    // Single opt-in: `subscribed`. `status = 'pending'` is the whole guard — an `unsubscribed`
    // row stays unsubscribed (invariant 1, and the reason `Resubscribed` is unreachable from
    // here), and a row already `subscribed` changes nothing and so records nothing.
    let subscribed: Vec<Uuid> = sqlx::query_scalar!(
        "insert into list_memberships \
             (workspace_id, contact_id, list_id, status, subscribed_at, source) \
         select $1, $2, l, 'subscribed', now(), $4::text from unnest($3::uuid[]) l \
         on conflict (contact_id, list_id) do update \
            set status = 'subscribed', subscribed_at = now(), source = excluded.source \
          where list_memberships.status = 'pending' \
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
        enqueue_confirmation(
            &mut tx,
            state,
            form.workspace_id,
            contact_id,
            *list_id,
            &email,
        )
        .await?;
    }
    for list_id in subscribed {
        // `pending` before the write means the person had already been asked and this submission
        // is what settled it: the same word `GET /c/{token}` records for that transition.
        let action = match prior.get(&list_id) {
            Some(MembershipStatus::Pending) => consent::Action::Confirmed,
            _ => consent::Action::Subscribed,
        };
        consent::record(
            &mut tx,
            form.workspace_id,
            contact_id,
            Some(list_id),
            action,
            consent::Source::Form,
            &cx,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(Outcome::Done)
}

/// One `jobs` row per double-opt-in list, each carrying **its own** `Purpose::Confirm` token:
/// `token::parse` requires a `list_id` on exactly the confirm tokens, so one token cannot cover
/// two lists. A form on two double-opt-in lists honestly sends two emails — they are two
/// consents.
///
/// The payload carries the already-issued token **string**, beside the token and not inside it.
/// `public::token::Token`'s field list is frozen once the first unsubscribe link ships
/// (`deny_unknown_fields`, and `Purpose::Unsubscribe` has no TTL), so anything a job needs travels
/// alongside. The shape is `0023_forms_and_jobs.sql`'s, which M2's worker parses.
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
        },
    );
    sqlx::query!(
        "insert into jobs (id, workspace_id, kind, payload) \
         values ($1, $2, 'confirmation_email', $3)",
        Uuid::now_v7(),
        workspace_id,
        serde_json::json!({
            "contact_id": contact_id,
            "list_id": list_id,
            "email": email,
            "token": token,
        }),
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
/// `contacts::create` validates all of it: this function decides only *which* input feeds which
/// column. An unknown input name — anything the page did not render — is ignored by construction,
/// because this walks the form's fields rather than the body.
fn new_contact(form: &Form, submitted: &HashMap<String, Vec<String>>, email: &str) -> NewContact {
    let mut new = NewContact {
        email: email.to_owned(),
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
    // `page::STYLE`; a form page served with the stylesheet stripped shows an unlabelled extra
    // input, which is the failure mode worth having.
    body.push_str(&format!(
        "<div class=\"hp\" aria-hidden=\"true\"><label for=\"{HONEYPOT}\">Website</label>\
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
