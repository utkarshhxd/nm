// SPDX-License-Identifier: AGPL-3.0-only

//! Suppressions: the standing blocks that say "never email this address, whatever else says
//! otherwise". A block is an address or a whole domain, in one workspace or (with a null
//! `workspace_id`) across the instance. Lifting sets `lifted_at` and keeps the row.
//!
//! Every address row stores the address AND `crypto::suppression_hmac` of it. The hash is what lets
//! a GDPR erasure tombstone (no plaintext) keep blocking; it is never serialised in any response.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgExecutor};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    crypto::{secrets::Secrets, suppression_hmac},
    error::{ApiError, ApiResult, FieldError},
    pagination::{Page, PageParams},
    users::normalize_email,
};

pub mod routes;

/// Why an address is blocked. Stored as text under a `check` constraint, not a Postgres enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Reason {
    UnsubscribedAll,
    Bounced,
    Complained,
    Manual,
    /// Written only by erasing a contact; it keeps the hash and no plaintext. Nothing in the API
    /// creates one directly, and nothing lifts one.
    GdprErasure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Scope {
    /// One address.
    Address,
    /// Every address on exactly this domain: `spam.test` does NOT cover `mail.spam.test`.
    Domain,
}

/// A suppression as the API shows it. There is deliberately no hash field here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Suppression {
    pub id: Uuid,
    /// Null for an instance-wide suppression, which applies to every workspace.
    pub workspace_id: Option<Uuid>,
    pub scope: Scope,
    /// Address scope only; null on an erasure tombstone, whose plaintext was destroyed.
    pub email: Option<String>,
    /// Domain scope only.
    pub domain: Option<String>,
    pub reason: Reason,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub created_by_user_id: Option<Uuid>,
    /// Set when lifted. A lifted row stays, as history.
    pub lifted_at: Option<DateTime<Utc>>,
    pub lifted_by_user_id: Option<Uuid>,
}

/// A suppression to add. `scope` decides which of `email` and `domain` is required, and the other
/// must be absent.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewSuppression {
    pub scope: Scope,
    /// Required for `address`. Normalised (trimmed, NFC, lowercased) before it is stored.
    pub email: Option<String>,
    /// Required for `domain`, such as `spam.example`. Matches that exact domain only: subdomains are NOT covered (`spam.example` leaves `mail.spam.example` mailable).
    pub domain: Option<String>,
    /// Anything but `gdpr_erasure`, which only erasing a contact writes.
    pub reason: Reason,
    /// Up to 500 characters; line breaks are allowed.
    pub note: Option<String>,
}

/// `true` if `email` may not be mailed from `workspace_id`: an address block or a domain block, in
/// this workspace or instance-wide. The single-address path (a contact being created or imported),
/// and the only one that sees an erasure tombstone, because it matches on the HMAC rather than on
/// plaintext. A domain block is exact: it does not cover subdomains, so a send-time check must not
/// assume it does.
///
/// Normalises `email` itself, so a caller cannot get a bypass by passing another Unicode form. An
/// address that cannot be normalised at all counts as suppressed: this is a safety check, and it
/// must fail closed, not answer "go ahead" about something it could not read.
pub async fn is_suppressed(
    db: impl PgExecutor<'_>,
    secrets: &Secrets,
    workspace_id: Uuid,
    email: &str,
) -> sqlx::Result<bool> {
    let Ok(email) = normalize_email(email) else {
        return Ok(true);
    };
    let hmac = suppression_hmac(secrets, &email);
    // A normalised address has exactly one `@`.
    let domain = email.split_once('@').map_or("", |(_, d)| d);
    // `(workspace_id = $1 or workspace_id is null)` is workspace scope and instance scope; the two
    // `exists` are address scope and domain scope. Each reaches its own partial index.
    sqlx::query_scalar!(
        r#"select (
             exists (select 1 from suppressions where lifted_at is null and scope = 'address'
                       and email_hmac = $2 and (workspace_id = $1 or workspace_id is null))
             or exists (select 1 from suppressions where lifted_at is null and scope = 'domain'
                       and domain = $3 and (workspace_id = $1 or workspace_id is null))
           ) as "blocked!""#,
        workspace_id,
        hmac.as_slice(),
        domain,
    )
    .fetch_one(db)
    .await
}

/// The domain of `x@<raw>` under exactly the rules `normalize_email` applies to an address, so a
/// domain block and an address on that domain can never disagree about the spelling.
fn normalize_domain(raw: &str) -> Result<String, FieldError> {
    normalize_email(&format!("x@{raw}"))
        .ok()
        .and_then(|e| e.split_once('@').map(|(_, d)| d.to_owned()))
        .ok_or_else(|| FieldError::new("domain", "must be a valid domain name"))
}

fn validate_note(raw: &str) -> Result<String, FieldError> {
    let note = raw.trim();
    if note.chars().count() <= 500 && !note.chars().any(|c| c.is_control() && c != '\n') {
        return Ok(note.to_owned());
    }
    Err(FieldError::new(
        "note",
        "must be at most 500 characters with no control characters",
    ))
}

/// The suppression, or `404` if it is not in this scope (`None` is the instance).
pub async fn get(
    db: &mut PgConnection,
    workspace_id: Option<Uuid>,
    id: Uuid,
) -> ApiResult<Suppression> {
    sqlx::query_as!(
        Suppression,
        r#"select id, workspace_id, scope as "scope: Scope", email, domain,
                  reason as "reason: Reason", note, created_at, created_by_user_id,
                  lifted_at, lifted_by_user_id
           from suppressions where id = $2 and workspace_id is not distinct from $1"#,
        workspace_id,
        id,
    )
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// Newest first; lifted rows only with `include_lifted`. `workspace_id` `None` lists the
/// instance-wide ones. The caller has authorised the actor.
pub async fn list(
    db: &mut PgConnection,
    workspace_id: Option<Uuid>,
    include_lifted: bool,
    page: &PageParams,
) -> ApiResult<Page<Suppression>> {
    let limit = page.limit();
    let rows = sqlx::query_as!(
        Suppression,
        r#"select id, workspace_id, scope as "scope: Scope", email, domain,
                  reason as "reason: Reason", note, created_at, created_by_user_id,
                  lifted_at, lifted_by_user_id
           from suppressions
           where (workspace_id = $1 or ($1::uuid is null and workspace_id is null))
             and ($2 or lifted_at is null) and ($3::uuid is null or id < $3)
           order by id desc limit $4"#,
        workspace_id,
        include_lifted,
        page.after()?,
        limit + 1,
    )
    .fetch_all(db)
    .await?;
    Ok(Page::from_rows(rows, limit, |s| s.id))
}

/// Adds a suppression (`workspace_id` `None`: instance-wide) and returns it. Validates every
/// field, stores the HMAC of an address along with the address, and refuses `gdpr_erasure`.
/// `409 suppression-exists` if the same address or domain is already blocked in that scope. The
/// caller passes the transaction and has already authorised the actor.
pub async fn create(
    db: &mut PgConnection,
    secrets: &Secrets,
    workspace_id: Option<Uuid>,
    created_by: Option<Uuid>,
    input: &NewSuppression,
) -> ApiResult<Suppression> {
    let mut errors = vec![];
    if input.reason == Reason::GdprErasure {
        errors.push(FieldError::new(
            "reason",
            "cannot be set here: erasing a contact writes it",
        ));
    }
    let note = input.note.as_deref().map(validate_note).transpose();
    // Exactly one of the two is wanted; the other must be absent, as the table's check requires.
    let (wanted, unwanted) = match input.scope {
        Scope::Address => (("email", &input.email), ("domain", &input.domain)),
        Scope::Domain => (("domain", &input.domain), ("email", &input.email)),
    };
    if unwanted.1.is_some() {
        errors.push(FieldError::new(unwanted.0, "must be absent for this scope"));
    }
    let value = match wanted.1.as_deref() {
        None => Err(FieldError::new(wanted.0, "is required for this scope")),
        Some(raw) if input.scope == Scope::Address => normalize_email(raw),
        Some(raw) => normalize_domain(raw),
    };
    errors.extend(note.as_ref().err().cloned());
    errors.extend(value.as_ref().err().cloned());
    let (Ok(note), Ok(value), true) = (note, value, errors.is_empty()) else {
        return Err(ApiError::Validation(errors));
    };

    let id = Uuid::now_v7();
    // The hash is computed for every address, whether or not the plaintext is ever erased.
    let (email, domain, hmac) = match input.scope {
        Scope::Address => {
            let hmac = suppression_hmac(secrets, &value);
            (Some(value), None, Some(hmac))
        }
        Scope::Domain => (None, Some(value), None),
    };
    sqlx::query!(
        "insert into suppressions (id, workspace_id, scope, email, domain, email_hmac, reason, \
         note, created_by_user_id) values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        id,
        workspace_id,
        input.scope as Scope,
        email,
        domain,
        hmac.as_ref().map(<[u8; 32]>::as_slice),
        input.reason as Reason,
        note,
        created_by,
    )
    .execute(&mut *db)
    .await
    // A fresh `id` can only collide on the live-uniqueness index.
    .map_err(|e| match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("suppression-exists"),
        _ => e.into(),
    })?;
    get(db, workspace_id, id).await
}

/// Lifts a live suppression and returns it as it now stands. `404` if it is not in this scope,
/// `409 suppression-already-lifted`, and `403 erasure-not-liftable` for a `gdpr_erasure` row: no
/// role may lift one, so an erasure cannot be undone by anyone. The caller passes the transaction
/// and has already authorised the actor.
pub async fn lift(
    db: &mut PgConnection,
    workspace_id: Option<Uuid>,
    id: Uuid,
    by: Option<Uuid>,
) -> ApiResult<Suppression> {
    // Locked, so two lifts racing cannot both pass the check.
    let row = sqlx::query!(
        r#"select reason as "reason: Reason", lifted_at from suppressions
           where id = $2 and workspace_id is not distinct from $1 for update"#,
        workspace_id,
        id,
    )
    .fetch_optional(&mut *db)
    .await?
    .ok_or(ApiError::NotFound)?;
    if row.reason == Reason::GdprErasure {
        return Err(ApiError::Forbidden("erasure-not-liftable"));
    }
    if row.lifted_at.is_some() {
        return Err(ApiError::Conflict("suppression-already-lifted"));
    }
    sqlx::query!(
        "update suppressions set lifted_at = now(), lifted_by_user_id = $2 where id = $1",
        id,
        by,
    )
    .execute(&mut *db)
    .await?;
    get(db, workspace_id, id).await
}
