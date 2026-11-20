// SPDX-License-Identifier: AGPL-3.0-only

//! Lists: the named sets of contacts a campaign is sent to, and each contact's subscription status
//! on each one. An unsubscribe is a membership row kept with `unsubscribed`, never a deleted row.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::Principal;
use crate::consent;
use crate::error::{ApiError, ApiResult, FieldError};

pub mod routes;

/// Stored as text under a `check` constraint, not a Postgres enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Visibility {
    Public,
    Private,
}

/// What a membership row says. It means the same on every list: `Pending` is written only on a
/// double-opt-in list, `Subscribed` is written at once on a single-opt-in one, and nothing here
/// depends on the list's setting. A `Pending` row can outlive the list's
/// double opt-in being switched off: it still means "not confirmed" and is not mailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum MembershipStatus {
    Pending,
    Subscribed,
    Unsubscribed,
}

impl MembershipStatus {
    /// Only `Subscribed` may be mailed. The send-time view must agree with this.
    pub fn is_mailable(self) -> bool {
        self == MembershipStatus::Subscribed
    }
}

/// How a membership was created, for the consent log. `form` has no writer yet (Task 9's hosted
/// signup pages) and is therefore not a variant; the table's `check` already admits it. A row
/// written before the column existed reads as NULL, never as one of these.
///
/// `PreferenceCenter` was added by `0022`, with the reasoning in that migration: the subscriber
/// changing their own preferences is not `ui` (nobody signed in), not `api`, and not a NULL,
/// which would assert the row predates `0018`. It is deliberately not `form` either — a form
/// follows the list's `double_optin` and may write `pending`, where the preference center never
/// does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum MembershipSource {
    Ui,
    Api,
    Bulk,
    Import,
    PreferenceCenter,
}

impl MembershipSource {
    /// A single add by a signed-in user is the web UI; by a workspace key it is the API. A
    /// cookie session is taken to be the UI: it cannot tell a script that reuses a cookie.
    pub fn of_single_add(principal: &Principal) -> Self {
        match principal {
            Principal::User { .. } => Self::Ui,
            Principal::ApiKey { .. } => Self::Api,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct List {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    /// `true`: a new member is `pending` until they confirm. `false`: a new member is
    /// `subscribed` at once.
    pub double_optin: bool,
    /// Unused until the preference center, which shows the `public` lists as topics.
    pub visibility: Visibility,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One list a contact is on, as the contact's page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ContactList {
    pub list_id: Uuid,
    pub name: String,
    pub status: MembershipStatus,
    pub subscribed_at: Option<DateTime<Utc>>,
    pub unsubscribed_at: Option<DateTime<Utc>>,
}

/// A contact's standing on one list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ListMember {
    pub contact_id: Uuid,
    pub email: String,
    pub status: MembershipStatus,
    /// Null while `pending`, and after an unsubscribe is undone on a double-opt-in list.
    pub subscribed_at: Option<DateTime<Utc>>,
    /// Set only while the status is `unsubscribed`.
    pub unsubscribed_at: Option<DateTime<Utc>>,
}

/// Trimmed, 1..=100 characters, no control characters.
pub(crate) fn validate_name(raw: &str) -> Result<String, FieldError> {
    let name = raw.trim();
    if (1..=100).contains(&name.chars().count()) && !name.chars().any(char::is_control) {
        return Ok(name.to_owned());
    }
    Err(FieldError::new(
        "name",
        "must be 1-100 characters with no control characters",
    ))
}

/// At most 1000 characters; line breaks are allowed.
pub(crate) fn validate_description(raw: &str) -> Result<String, FieldError> {
    let text = raw.trim();
    if text.chars().count() <= 1000 && !text.chars().any(|c| c.is_control() && c != '\n') {
        return Ok(text.to_owned());
    }
    Err(FieldError::new(
        "description",
        "must be at most 1000 characters with no control characters",
    ))
}

/// The list, or `404` if it is not in this workspace.
pub async fn get(db: &mut PgConnection, workspace_id: Uuid, id: Uuid) -> ApiResult<List> {
    sqlx::query_as!(
        List,
        r#"select id, name, description, double_optin, visibility as "visibility: Visibility",
                  created_at, updated_at
           from lists where workspace_id = $1 and id = $2"#,
        workspace_id,
        id,
    )
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// Puts the contact on the list and returns the status that was written: `Subscribed` on a
/// single-opt-in list, `Pending` on a double-opt-in one. The caller passes the transaction and has
/// already authorised the actor. `404` if either id is not in this workspace.
///
/// A membership that already exists is left exactly as it is, whatever it says, and the status it
/// already has is what comes back. Adding someone twice must not demote a confirmed subscriber
/// back to `pending`, and an `unsubscribed` one stays `unsubscribed` (spec §8.1, M1d invariant 1):
/// an operator naming one address is still the operator asserting consent on the subscriber's
/// behalf, which is the thing consent law exists to forbid. Only the subscriber can undo their own
/// unsubscribe, through a token delivered to their own address — the preference center.
///
/// A write is a consent row, `Source::Operator`, in the same transaction: `requested` when the
/// status written is `pending`, `subscribed` otherwise. Leaving a membership as it is records
/// nothing.
pub async fn add_member(
    db: &mut PgConnection,
    workspace_id: Uuid,
    list_id: Uuid,
    contact_id: Uuid,
    source: MembershipSource,
    cx: &consent::Context,
) -> ApiResult<MembershipStatus> {
    let double_optin = sqlx::query_scalar!(
        "select double_optin from lists where workspace_id = $1 and id = $2",
        workspace_id,
        list_id,
    )
    .fetch_one(&mut *db)
    .await?;
    let status = if double_optin {
        MembershipStatus::Pending
    } else {
        MembershipStatus::Subscribed
    };
    let written = sqlx::query!(
        r#"insert into list_memberships (workspace_id, contact_id, list_id, status, subscribed_at, source)
           values ($1, $2, $3, $4, case when $4::text = 'subscribed' then now() end, $5)
           on conflict (contact_id, list_id) do nothing
           returning status as "status: MembershipStatus""#,
        workspace_id,
        contact_id,
        list_id,
        status as MembershipStatus,
        source as MembershipSource,
    )
    .fetch_optional(&mut *db)
    .await
    // The list exists in this workspace, so the only foreign key left to fail is the contact's.
    .map_err(|e| match e.as_database_error() {
        Some(d) if d.is_foreign_key_violation() => ApiError::NotFound,
        _ => e.into(),
    })?;
    match written {
        Some(w) => {
            // The action follows the status that was WRITTEN, not the `double_optin` flag it came
            // from: on a double-opt-in list the person has only been asked, not agreed.
            // Exhaustive on purpose, so a status added later must be given an action here before
            // this compiles rather than being logged as something it is not.
            let action = match w.status {
                MembershipStatus::Pending => consent::Action::Requested,
                MembershipStatus::Subscribed => consent::Action::Subscribed,
                MembershipStatus::Unsubscribed => consent::Action::Unsubscribed,
            };
            let source = consent::Source::Operator;
            consent::record(
                db,
                workspace_id,
                contact_id,
                Some(list_id),
                action,
                source,
                cx,
            )
            .await?;
            Ok(w.status)
        }
        None => Ok(sqlx::query_scalar!(
            r#"select status as "status: MembershipStatus" from list_memberships
               where workspace_id = $1 and contact_id = $2 and list_id = $3"#,
            workspace_id,
            contact_id,
            list_id,
        )
        .fetch_one(db)
        .await?),
    }
}

/// Marks the membership `unsubscribed` and stamps `unsubscribed_at`; the row stays. Doing it again
/// keeps the first stamp and records nothing: only a change is a consent row (`unsubscribed`,
/// `Source::Operator`, same transaction). `404` if the contact was never on this list, or it is
/// not in this workspace.
pub async fn unsubscribe_member(
    db: &mut PgConnection,
    workspace_id: Uuid,
    list_id: Uuid,
    contact_id: Uuid,
    cx: &consent::Context,
) -> ApiResult<()> {
    // One statement, so one snapshot: "did this change anything" and "is there a membership at
    // all" cannot disagree. Asking separately let a membership another transaction committed in
    // between turn a 404 into a `204` over a contact who is still subscribed.
    let done = sqlx::query!(
        r#"with changed as (
               update list_memberships
                  set status = 'unsubscribed', unsubscribed_at = now()
                where workspace_id = $1 and list_id = $2 and contact_id = $3
                  and status <> 'unsubscribed'
               returning 1
           )
           select exists (select 1 from changed) as "changed!",
                  exists (select 1 from list_memberships
                           where workspace_id = $1 and list_id = $2
                             and contact_id = $3) as "member!""#,
        workspace_id,
        list_id,
        contact_id,
    )
    .fetch_one(&mut *db)
    .await?;
    if done.changed {
        let source = consent::Source::Operator;
        let action = consent::Action::Unsubscribed;
        return consent::record(
            db,
            workspace_id,
            contact_id,
            Some(list_id),
            action,
            source,
            cx,
        )
        .await;
    }
    // Nothing changed: it was already unsubscribed (fine) or was never a member (404).
    if done.member {
        Ok(())
    } else {
        Err(ApiError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_subscribed_is_mailable() {
        assert!(MembershipStatus::Subscribed.is_mailable());
        assert!(!MembershipStatus::Pending.is_mailable());
        assert!(!MembershipStatus::Unsubscribed.is_mailable());
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(validate_name("  News  ").unwrap(), "News");
        for bad in ["", "   ", "a\nb", &"x".repeat(101)] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_description("a\nb").is_ok());
        assert!(validate_description("a\0b").is_err());
    }
}
