// SPDX-License-Identifier: AGPL-3.0-only

//! The consent log: one append-only row per consent change, so that years later the question
//! "prove this person agreed" has an answer. A row is never updated, and is removed only when the
//! contact it belongs to is erased (the database cascades that; nothing here deletes).

use std::net::IpAddr;

use serde::Serialize;
use sqlx::PgConnection;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};

pub mod routes;

/// What changed — and for `requested`, what did not: the person was **asked** and has not
/// agreed. Stored as text under a `check` constraint, as `lists::MembershipStatus` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, sqlx::Type, ToSchema)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Action {
    /// A membership written `pending`: the person was asked, and has not agreed. It is its own
    /// action rather than a `subscribed` row read back through `lists.double_optin`, because that
    /// flag is mutable and unversioned — flipping it would otherwise re-decide, years later, what
    /// an old row meant.
    Requested,
    Subscribed,
    Confirmed,
    Unsubscribed,
    Resubscribed,
    Suppressed,
}

/// What caused the change. **The code path, never the actor**: a session-authenticated operator
/// using bulk subscribe is `Api`, because that is the writer the change went through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, sqlx::Type, ToSchema)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Source {
    Form,
    Import,
    Api,
    Operator,
    OneClick,
    PreferenceCenter,
    Bounce,
    Complaint,
}

/// The evidence kept with a row. `ip` and `user_agent` are personal data: never log a `Context`.
/// It has no `Debug` for that reason, and keeps none: a token nonce is not personal data, but the
/// struct it joins is.
#[derive(Clone, Default)]
pub struct Context {
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    /// The form id, the import id, the user id: whatever caused the change.
    pub source_ref: Option<String>,
    /// The nonce of the public token that authorised the change: *which* link was clicked, not
    /// merely that one was. Operator and API paths pass `None` and must keep doing so — a row
    /// naming a link the subscriber never followed reads as if they had.
    pub token_jti: Option<String>,
}

/// Appends one row. Takes the caller's connection so that it runs in the same transaction as the
/// membership write it records: a consent row must never outlive a rolled-back change. It checks
/// nothing itself; the composite foreign keys keep the contact and list in `workspace_id`.
///
/// The error is [`scrub`]bed here rather than at the call site, because this is the only function
/// that binds an IP and a user agent: `sqlx::Error` must not escape, or a bare `?` somewhere else
/// would put the failing row — evidence and all — into a log line.
pub async fn record(
    tx: &mut PgConnection,
    workspace_id: Uuid,
    contact_id: Uuid,
    list_id: Option<Uuid>,
    action: Action,
    source: Source,
    cx: &Context,
) -> ApiResult<()> {
    sqlx::query!(
        "insert into consent_log (id, workspace_id, contact_id, list_id, action, source, \
         source_ref, ip, user_agent, token_jti) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        Uuid::now_v7(),
        workspace_id,
        contact_id,
        list_id,
        action as Action,
        source as Source,
        cx.source_ref,
        cx.ip as Option<IpAddr>,
        cx.user_agent,
        cx.token_jti,
    )
    .execute(tx)
    .await
    .map_err(scrub)?;
    Ok(())
}

// There is deliberately no `record_many`. A set-based writer records its rows in the same
// statement that wrote them, as a `logged` CTE over that statement's `returning` — see
// `contacts::selection::run_recorded` and the importer's `write_memberships_and_tags`. A batch
// helper taking one `Action` for every row could not express the importer's per-row status
// (`pending` on a double-opt-in list, `subscribed` on a single-opt-in one, in one import), and it
// had to send every id back to Rust and out again, which is a per-row cost on a million-row
// import. `record` stays for the single-row writers and for the public routes.

/// What a failed consent write becomes on its way to the log: a message, the SQLSTATE and the
/// constraint, never the error. Postgres copies the failing row into a check violation's DETAIL,
/// and the row holds an IP address and a user agent; `ApiError::Internal` is logged with `{:?}`.
/// A constraint name comes from a migration, so it names no person and is kept — without it an
/// opaque SQLSTATE is undiagnosable.
///
/// `pub` for the test that proves the leak is closed; [`record`] applies it itself, so no caller
/// has to remember to.
pub fn scrub(e: sqlx::Error) -> ApiError {
    let db = e.as_database_error();
    let state = db.and_then(|d| d.code());
    ApiError::Internal(anyhow::anyhow!(
        "consent_log write failed (sqlstate {}, constraint {})",
        state.as_deref().unwrap_or("none"),
        db.and_then(|d| d.constraint()).unwrap_or("none"),
    ))
}

/// The `source_ref` an operator's or an API key's change is recorded under: who made it. It is
/// the whole of what an operator path may put in a consent row, which is why the set-based
/// writers take this and not a [`Context`] — there is nothing else of theirs to pass.
pub fn principal_ref(principal: &crate::auth::Principal) -> String {
    match principal {
        crate::auth::Principal::User { user_id, .. } => user_id,
        crate::auth::Principal::ApiKey { key_id, .. } => key_id,
    }
    .to_string()
}

impl Context {
    /// An operator's own change through the API: who made it, and no IP, user agent or token
    /// nonce, because the request's belong to the operator and not to the person whose consent
    /// this is.
    pub fn of_principal(principal: &crate::auth::Principal) -> Self {
        Context {
            source_ref: Some(principal_ref(principal)),
            ..Context::default()
        }
    }
}
