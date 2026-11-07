// SPDX-License-Identifier: AGPL-3.0-only

use uuid::Uuid;

use crate::http::request_meta::{RequestMeta, truncate_user_agent};

/// Action names. Reference these constants; never type an action string at a call site.
pub mod actions {
    pub const AUTH_LOGIN_SUCCEEDED: &str = "auth.login.succeeded";
    pub const AUTH_LOGIN_FAILED: &str = "auth.login.failed";
    pub const AUTH_LOGOUT: &str = "auth.logout";
    pub const AUTH_PASSWORD_CHANGED: &str = "auth.password.changed";
    pub const AUTH_TOTP_ENABLED: &str = "auth.totp.enabled";
    pub const AUTH_TOTP_DISABLED: &str = "auth.totp.disabled";
    pub const AUTH_RECOVERY_CODE_USED: &str = "auth.recovery_code.used";
    pub const AUTH_PASSKEY_ADDED: &str = "auth.passkey.added";
    pub const AUTH_PASSKEY_REMOVED: &str = "auth.passkey.removed";
    pub const WORKSPACE_CREATED: &str = "workspace.created";
    pub const WORKSPACE_UPDATED: &str = "workspace.updated";
    pub const MEMBER_ROLE_CHANGED: &str = "member.role_changed";
    pub const MEMBER_REMOVED: &str = "member.removed";
    pub const INVITE_CREATED: &str = "invite.created";
    pub const INVITE_REVOKED: &str = "invite.revoked";
    pub const INVITE_ACCEPTED: &str = "invite.accepted";
    pub const API_KEY_CREATED: &str = "api_key.created";
    pub const API_KEY_REVOKED: &str = "api_key.revoked";
}

/// Who did it. The audit table has no foreign keys, so ids may outlive their rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    System,
    User(Uuid),
    ApiKey(Uuid),
}

#[derive(Debug, Clone)]
pub struct AuditEvent {
    pub workspace_id: Option<Uuid>,
    pub actor: Actor,
    pub action: &'static str,
    pub target_type: Option<&'static str>,
    pub target_id: Option<String>,
    /// SECURITY: this is stored in plaintext and read by admins. Never put a password, a TOTP or
    /// recovery code, a session or API token, or any key here (spec: secrets never appear in audit
    /// metadata). Record ids, names, roles and outcomes only. The types cannot enforce this.
    pub metadata: serde_json::Value,
}

/// Appends one audit row. Pass the transaction that performs the action (`&mut *tx`) so the row
/// commits or rolls back with it; pass `&pool` only for events with no accompanying write.
pub async fn record<'e>(
    db: impl sqlx::PgExecutor<'e>,
    meta: &RequestMeta,
    ev: AuditEvent,
) -> sqlx::Result<()> {
    let (user, key) = match ev.actor {
        Actor::System => (None, None),
        Actor::User(id) => (Some(id), None),
        Actor::ApiKey(id) => (None, Some(id)),
    };
    // Truncated here as well, since `RequestMeta` fields are public and can be built by hand.
    let user_agent = meta.user_agent.as_deref().map(truncate_user_agent);
    sqlx::query!(
        "insert into audit_log (id, workspace_id, actor_user_id, actor_api_key_id, action, \
         target_type, target_id, ip, user_agent, metadata) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        Uuid::now_v7(),
        ev.workspace_id,
        user,
        key,
        ev.action,
        ev.target_type,
        ev.target_id,
        meta.ip as Option<std::net::IpAddr>,
        user_agent,
        ev.metadata,
    )
    .execute(db)
    .await?;
    Ok(())
}
