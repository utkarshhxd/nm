// SPDX-License-Identifier: AGPL-3.0-only

//! The consent log: one append-only row per consent change, so that years later the question
//! "prove this person agreed" has an answer. A row is never updated, and is removed only when the
//! contact it belongs to is erased (the database cascades that; nothing here deletes).

use std::net::IpAddr;

use sqlx::PgConnection;
use uuid::Uuid;

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
pub async fn record(
    tx: &mut PgConnection,
    workspace_id: Uuid,
    contact_id: Uuid,
    list_id: Option<Uuid>,
    action: Action,
    source: Source,
    cx: &Context,
) -> sqlx::Result<()> {
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
    .await?;
    Ok(())
}
