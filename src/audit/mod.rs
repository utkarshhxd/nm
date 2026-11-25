// SPDX-License-Identifier: AGPL-3.0-only

use uuid::Uuid;

use crate::http::request_meta::{RequestMeta, truncate_user_agent};

pub mod routes;

/// Action names. Reference these constants; never type an action string at a call site.
pub mod actions {
    pub const AUTH_LOGIN_SUCCEEDED: &str = "auth.login.succeeded";
    pub const AUTH_LOGIN_FAILED: &str = "auth.login.failed";
    pub const AUTH_LOGIN_MFA_REQUIRED: &str = "auth.login.mfa_required";
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
    pub const CUSTOM_FIELD_CREATED: &str = "custom_field.created";
    pub const CUSTOM_FIELD_UPDATED: &str = "custom_field.updated";
    pub const CUSTOM_FIELD_ARCHIVED: &str = "custom_field.archived";
    pub const CUSTOM_FIELD_PURGED: &str = "custom_field.purged";
    pub const CONTACT_CREATED: &str = "contact.created";
    pub const CONTACT_UPDATED: &str = "contact.updated";
    pub const CONTACT_DELETED: &str = "contact.deleted";
    pub const CONTACT_ERASED: &str = "contact.erased";
    pub const CONTACTS_BULK_TAGGED: &str = "contacts.bulk_tagged";
    pub const CONTACTS_BULK_UNTAGGED: &str = "contacts.bulk_untagged";
    pub const CONTACTS_BULK_SUBSCRIBED: &str = "contacts.bulk_subscribed";
    pub const CONTACTS_BULK_UNSUBSCRIBED: &str = "contacts.bulk_unsubscribed";
    pub const CONTACTS_BULK_DELETED: &str = "contacts.bulk_deleted";
    pub const CONTACTS_EXPORTED: &str = "contacts.exported";
    pub const IMPORT_UPLOADED: &str = "import.uploaded";
    pub const IMPORT_STARTED: &str = "import.started";
    pub const IMPORT_FINISHED: &str = "import.finished";
    pub const LIST_CREATED: &str = "list.created";
    pub const LIST_UPDATED: &str = "list.updated";
    pub const LIST_DELETED: &str = "list.deleted";
    pub const LIST_MEMBER_ADDED: &str = "list.member_added";
    // Written as `list.member_removed` until M1b, when it was renamed for what it does. Rows
    // already in the log keep the old string, on purpose: history is not rewritten.
    pub const LIST_MEMBER_UNSUBSCRIBED: &str = "list.member_unsubscribed";
    // Each of these carries the form's whole definition in its metadata, not just which keys the
    // request named. `consent_log.source_ref` holds a form id, so a consent row says "this person
    // submitted form X" and nothing else would record what X asked for at the moment they agreed.
    // An operator editing X's lists or its copy would otherwise rewrite the meaning of every
    // consent row already pointing at it, straight through an append-only log.
    pub const FORM_CREATED: &str = "form.created";
    pub const FORM_UPDATED: &str = "form.updated";
    pub const FORM_DELETED: &str = "form.deleted";
    pub const SEGMENT_CREATED: &str = "segment.created";
    pub const SEGMENT_UPDATED: &str = "segment.updated";
    pub const SEGMENT_DELETED: &str = "segment.deleted";
    pub const TAG_CREATED: &str = "tag.created";
    pub const TAG_UPDATED: &str = "tag.updated";
    pub const TAG_DELETED: &str = "tag.deleted";
    pub const TAG_APPLIED: &str = "tag.applied";
    pub const TAG_REMOVED: &str = "tag.removed";
    pub const SUPPRESSION_CREATED: &str = "suppression.created";
    pub const SUPPRESSION_LIFTED: &str = "suppression.lifted";
    // Metadata names the relay and its host. Never the password: spec §8 invariant 7.
    pub const RELAY_CREATED: &str = "relay.created";
    pub const RELAY_UPDATED: &str = "relay.updated";
    pub const RELAY_DELETED: &str = "relay.deleted";
    // Metadata names the domain and, on an update, which fields were sent. Never the DKIM key.
    pub const SENDING_DOMAIN_CREATED: &str = "sending_domain.created";
    pub const SENDING_DOMAIN_UPDATED: &str = "sending_domain.updated";
    pub const SENDING_DOMAIN_DELETED: &str = "sending_domain.deleted";
    // Metadata names the template and its kind, never the document: it is kept in the row, it is
    // large, and the audit log is kept for ever.
    pub const TEMPLATE_CREATED: &str = "template.created";
    pub const TEMPLATE_UPDATED: &str = "template.updated";
    pub const TEMPLATE_DELETED: &str = "template.deleted";
    // Metadata names which palette and footer keys were set, never their values: `footer.address`
    // is a postal address.
    pub const BRAND_KIT_UPDATED: &str = "brand_kit.updated";
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
