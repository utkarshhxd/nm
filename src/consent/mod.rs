// SPDX-License-Identifier: AGPL-3.0-only

//! The consent log: one append-only row per consent change, so that years later the question
//! "prove this person agreed" has an answer. A row is never updated, and is removed only when the
//! contact it belongs to is erased (the database cascades that; nothing here deletes).

use std::net::IpAddr;

use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};

/// Stored as text under a `check` constraint, as `lists::MembershipStatus` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Action {
    Subscribed,
    Confirmed,
    Unsubscribed,
    Resubscribed,
    Suppressed,
}

/// What caused the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
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
/// It has no `Debug` for that reason.
#[derive(Clone, Default)]
pub struct Context {
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    /// The form id, the import id, the user id: whatever caused the change.
    pub source_ref: Option<String>,
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
         source_ref, ip, user_agent) values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        Uuid::now_v7(),
        workspace_id,
        contact_id,
        list_id,
        action as Action,
        source as Source,
        cx.source_ref,
        cx.ip as Option<IpAddr>,
        cx.user_agent,
    )
    .execute(tx)
    .await
    .map_err(scrub)?;
    Ok(())
}

/// Appends one row per `(contact_id, list_id)` pair, all with the same action, source and
/// evidence. For a set-based writer whose rows do have to come back to Rust first — the importer
/// reads them out of its own statement. A writer that can keep them in Postgres should: see the
/// `logged` CTE in `contacts::selection`. The same transaction and scrubbing rules as [`record`]
/// apply.
pub async fn record_many(
    tx: &mut PgConnection,
    workspace_id: Uuid,
    rows: &[(Uuid, Uuid)],
    action: Action,
    source: Source,
    cx: &Context,
) -> ApiResult<()> {
    let ids: Vec<Uuid> = rows.iter().map(|_| Uuid::now_v7()).collect();
    let (contacts, lists): (Vec<Uuid>, Vec<Uuid>) = rows.iter().copied().unzip();
    sqlx::query!(
        "insert into consent_log (id, workspace_id, contact_id, list_id, action, source, \
         source_ref, ip, user_agent) \
         select t.id, $1, t.contact_id, t.list_id, $5, $6, $7, $8::inet, $9 \
           from unnest($2::uuid[], $3::uuid[], $4::uuid[]) as t(id, contact_id, list_id)",
        workspace_id,
        &ids,
        &contacts,
        &lists,
        action as Action,
        source as Source,
        cx.source_ref,
        cx.ip as Option<IpAddr>,
        cx.user_agent,
    )
    .execute(tx)
    .await
    .map_err(scrub)?;
    Ok(())
}

/// What a failed consent write becomes on its way to the log: a message, the SQLSTATE and the
/// constraint, never the error. Postgres copies the failing row into a check violation's DETAIL,
/// and the row holds an IP address and a user agent; `ApiError::Internal` is logged with `{:?}`.
/// A constraint name comes from a migration, so it names no person and is kept — without it an
/// opaque SQLSTATE is undiagnosable.
///
/// `pub` for the test that proves the leak is closed; [`record`] and [`record_many`] apply it
/// themselves, so no caller has to remember to.
pub fn scrub(e: sqlx::Error) -> ApiError {
    let db = e.as_database_error();
    let state = db.and_then(|d| d.code());
    ApiError::Internal(anyhow::anyhow!(
        "consent_log write failed (sqlstate {}, constraint {})",
        state.as_deref().unwrap_or("none"),
        db.and_then(|d| d.constraint()).unwrap_or("none"),
    ))
}

impl Context {
    /// An operator's own change through the API: who made it, and no IP or user agent, because
    /// the request's belong to the operator and not to the person whose consent this is.
    pub fn of_principal(principal: &crate::auth::Principal) -> Self {
        let id = match principal {
            crate::auth::Principal::User { user_id, .. } => user_id,
            crate::auth::Principal::ApiKey { key_id, .. } => key_id,
        };
        Context {
            source_ref: Some(id.to_string()),
            ..Context::default()
        }
    }
}
