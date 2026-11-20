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

/// Whose block it is: this workspace's own, or one set instance-wide that every workspace inherits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BlockLevel {
    Workspace,
    Instance,
}

/// The standing block that stops one address being mailed, and nothing else about it: not the
/// row's id, note or who made it, and never another address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct Block {
    pub reason: Reason,
    pub level: BlockLevel,
    /// Whether it blocks this one address or every address on its domain.
    pub scope: Scope,
}

/// The block that stops `email` being mailed from `workspace_id`, or `None`: an address block or a
/// domain block, in this workspace or instance-wide. The single-address path, matching on the HMAC
/// so it sees an erasure tombstone too. When several apply, the workspace's own is reported before
/// an instance-wide one, and an address block before a domain block. It is for the send-time check
/// (a later milestone) and for explaining a contact's page, NOT for creating or importing a
/// contact: it finds a block for every reason, so using it there would refuse anyone who ever
/// bounced or unsubscribed. Creating a contact asks `is_erased` instead.
///
/// A domain block is exact: it does not cover subdomains, so a send-time check must not assume it
/// does.
///
/// Normalises `email` itself, so a caller cannot get a bypass by passing another Unicode form. An
/// address that cannot be normalised at all is an `ApiError::Internal` (a 500), never `None`:
/// this is a safety check, and it must fail closed, not answer "go ahead" about something it could
/// not read. The message names the condition but not the address, which is personal data.
pub async fn blocking_suppression(
    db: impl PgExecutor<'_>,
    secrets: &Secrets,
    workspace_id: Uuid,
    email: &str,
) -> ApiResult<Option<Block>> {
    let Ok(email) = normalize_email(email) else {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "a stored address does not normalise, so its suppression status cannot be checked"
        )));
    };
    Ok(blocking(db, secrets, workspace_id, &email).await?)
}

/// `true` if `email` may not be mailed from `workspace_id`: `blocking_suppression` found a block.
/// An address that cannot be normalised counts as suppressed.
pub async fn is_suppressed(
    db: impl PgExecutor<'_>,
    secrets: &Secrets,
    workspace_id: Uuid,
    email: &str,
) -> sqlx::Result<bool> {
    let Ok(email) = normalize_email(email) else {
        return Ok(true);
    };
    Ok(blocking(db, secrets, workspace_id, &email).await?.is_some())
}

/// The one predicate behind both; `email` is already normalised.
async fn blocking(
    db: impl PgExecutor<'_>,
    secrets: &Secrets,
    workspace_id: Uuid,
    email: &str,
) -> sqlx::Result<Option<Block>> {
    let hmac = suppression_hmac(secrets, email);
    // A normalised address has exactly one `@`.
    let domain = email.split_once('@').map_or("", |(_, d)| d);
    // `(workspace_id = $1 or workspace_id is null)` is workspace scope and instance scope; the two
    // arms are address scope and domain scope. Each reaches its own partial index. At most two rows
    // per arm can be live (one per workspace, one instance-wide), so no arm needs a limit. `false
    // < true` puts a workspace block first, and `address < domain` an address block.
    let row = sqlx::query!(
        r#"(select reason as "reason!: Reason", workspace_id is null as "instance!",
                   scope as "scope!: Scope"
            from suppressions where lifted_at is null and scope = 'address'
              and email_hmac = $2 and (workspace_id = $1 or workspace_id is null))
           union all
           (select reason, workspace_id is null, scope
            from suppressions where lifted_at is null and scope = 'domain'
              and domain = $3 and (workspace_id = $1 or workspace_id is null))
           order by 2, 3 limit 1"#,
        workspace_id,
        hmac.as_slice(),
        domain,
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| Block {
        reason: r.reason,
        level: if r.instance {
            BlockLevel::Instance
        } else {
            BlockLevel::Workspace
        },
        scope: r.scope,
    }))
}

/// Serialises everything that writes or checks the erasure of one address, until the transaction
/// ends. Without it a contact create could read "no tombstone", an erase could commit, and the
/// create could then insert: a contact for an erased address.
///
/// SHORTCUT: one lock per address, held to the end of the transaction. Fine for one create or one
/// erase. A bulk path writing N addresses in ONE transaction would hold N locks in the shared lock
/// table (`out of shared memory ... increase max_locks_per_transaction`), and two concurrent bulk
/// writers taking them in different orders would deadlock. Such a path must batch into short
/// sub-transactions and acquire the locks in sorted key order.
async fn lock_address(db: &mut PgConnection, hmac: &[u8; 32]) -> sqlx::Result<()> {
    let key = i64::from_be_bytes(hmac[..8].try_into().expect("32 bytes"));
    sqlx::query!("select pg_advisory_xact_lock($1)", key)
        .execute(db)
        .await?;
    Ok(())
}

/// `true` if `email` was erased in `workspace_id` (or instance-wide). Narrower than
/// `is_suppressed`, on purpose: a bounced or unsubscribed address may still be a contact (the
/// eligibility view keeps it from being mailed), an erased one may not. Takes a lock held to the
/// end of the transaction, so the caller must be about to write (see `lock_address`). An address
/// that cannot be normalised counts as erased: refuse, never guess.
pub async fn is_erased(
    db: &mut PgConnection,
    secrets: &Secrets,
    workspace_id: Uuid,
    email: &str,
) -> sqlx::Result<bool> {
    let Ok(email) = normalize_email(email) else {
        return Ok(true);
    };
    let hmac = suppression_hmac(secrets, &email);
    lock_address(db, &hmac).await?;
    sqlx::query_scalar!(
        r#"select exists (select 1 from suppressions where reason = 'gdpr_erasure'
                            and email_hmac = $2 and (workspace_id = $1 or workspace_id is null))
           as "erased!""#,
        workspace_id,
        hmac.as_slice(),
    )
    .fetch_one(db)
    .await
}

/// Writes the erasure tombstone for an already-normalised `email`: the hash and no plaintext. Any
/// other row for the address in this workspace (live or lifted, with its plaintext) is deleted
/// first, so nothing readable survives and the tombstone cannot collide with a live block. The
/// caller passes the transaction that deletes the contact.
pub async fn erase_address(
    db: &mut PgConnection,
    secrets: &Secrets,
    workspace_id: Uuid,
    email: &str,
) -> sqlx::Result<()> {
    let hmac = suppression_hmac(secrets, email);
    lock_address(db, &hmac).await?;
    sqlx::query!(
        "delete from suppressions where workspace_id = $1 and scope = 'address' and email_hmac = $2",
        workspace_id,
        hmac.as_slice(),
    )
    .execute(&mut *db)
    .await?;
    sqlx::query!(
        "insert into suppressions (id, workspace_id, scope, email_hmac, reason) \
         values ($1, $2, 'address', $3, 'gdpr_erasure')",
        Uuid::now_v7(),
        workspace_id,
        hmac.as_slice(),
    )
    .execute(db)
    .await?;
    Ok(())
}

/// The subscriber's own workspace-wide opt-out, written by RFC 8058 one-click
/// (`public::unsubscribe`). `true` if a row was written, `false` if the address was already
/// blocked in this workspace.
///
/// **Why the one-click route writes here at all**, when its visible job is the membership rows
/// (the table is deliberately not named: `tests/consent.rs` keeps a census of the files that
/// mention it, and this one is not a writer of memberships, only a reader of this comment):
/// `docs/adr/0001-contacts-have-no-status-column.md` decided that the workspace-wide opt-out is a
/// row in this table and nowhere else, *because it outlives the contact row*. A membership sweep
/// alone is undone by an erasure followed by a re-import, or by an import that names the same
/// address on a list it was never on — and spec M1d invariant 1 only protects a membership that
/// already exists as `unsubscribed`. This is the row that makes "unsubscribe from everything"
/// mean everything, including lists that do not exist yet.
///
/// `created_by_user_id` is null and `source` is `one_click`: nobody signed in did this.
///
/// shortcut: `suppressions_live` is unique per (workspace, scope, address) **whatever the
/// reason**, so an address already blocked as `bounced` or `complained` keeps that row and gets
/// no `unsubscribed_all` one — the address is blocked either way, but lifting the bounce would
/// then unblock it. Give this table a per-reason live index if that case ever needs separating.
pub async fn unsubscribe_all(
    db: &mut PgConnection,
    secrets: &Secrets,
    workspace_id: Uuid,
    email: &str,
) -> ApiResult<bool> {
    // Fails closed, as `blocking_suppression` does: an address that will not normalise cannot be
    // blocked by hash, and silently not blocking it is the one outcome this route may not have.
    let Ok(email) = normalize_email(email) else {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "a stored address does not normalise, so a one-click opt-out cannot be recorded"
        )));
    };
    let hmac = suppression_hmac(secrets, &email);
    // The same lock `erase_address` and `is_erased` take, so a one-click arriving while a contact
    // is being erased cannot interleave into two live rows for one address.
    lock_address(db, &hmac).await?;
    let written = sqlx::query_scalar!(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason, source) \
         values ($1, $2, 'address', $3, $4, 'unsubscribed_all', 'one_click') \
         on conflict (workspace_id, scope, email_hmac, domain) where lifted_at is null \
         do nothing returning id",
        Uuid::now_v7(),
        workspace_id,
        email,
        hmac.as_slice(),
    )
    .fetch_optional(db)
    .await?;
    Ok(written.is_some())
}

/// Lifts the subscriber's own workspace-wide opt-out, and **only** that one. `true` if a row was
/// lifted. Called by the preference center when the subscriber asks for a list again: the opt-out
/// was theirs to set, so it is theirs to clear, and leaving it would show them boxes that tick
/// and change nothing a send would honour.
///
/// **`reason = 'unsubscribed_all'` is the whole of what this may touch, and that is the point.**
/// `blocking` is the one definition of "may this address be mailed" (`m1a-decisions.md`), and a
/// bounce, a complaint, an operator's manual block and an erasure tombstone are all facts about
/// the address that a subscriber ticking a checkbox has no standing to overturn — an erasure
/// tombstone is unliftable by anyone, including [`lift`]. Widening this predicate would turn an
/// unauthenticated page into a way to clear any block in the workspace.
///
/// `lifted_by_user_id` stays null, which already means "no signed-in user did this".
pub async fn lift_unsubscribed_all(
    db: &mut PgConnection,
    secrets: &Secrets,
    workspace_id: Uuid,
    email: &str,
) -> ApiResult<bool> {
    let Ok(email) = normalize_email(email) else {
        return Ok(false);
    };
    let hmac = suppression_hmac(secrets, &email);
    let lifted = sqlx::query_scalar!(
        "update suppressions set lifted_at = now() \
          where workspace_id = $1 and scope = 'address' and email_hmac = $2 \
            and reason = 'unsubscribed_all' and lifted_at is null \
         returning id",
        workspace_id,
        hmac.as_slice(),
    )
    .fetch_optional(db)
    .await?;
    Ok(lifted.is_some())
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
