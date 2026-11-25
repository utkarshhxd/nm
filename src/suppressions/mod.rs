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
///
/// Named for what it blocks rather than `Block`, because `components.schemas` is ONE flat map
/// keyed by the type's name and `template::block::Block` is a different thing entirely. Two types
/// sharing a key do not collide loudly: the second insert replaces the first and every `$ref`
/// still resolves, so one of them ships with the other's shape (`schema_names_are_unique`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct SuppressionBlock {
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
) -> ApiResult<Option<SuppressionBlock>> {
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

/// `true` if **any** live suppression covering `email` has a reason other than `bounced`: this
/// workspace's or the instance's, on the address or on its domain.
///
/// **It is not [`blocking_suppression`] with a reason test on the answer, and the difference is
/// the whole point of it existing.** That function answers "what is the most specific block on
/// this address", `order by … limit 1`, workspace before instance and address before domain. A
/// workspace-scope `bounced` row therefore *masks* an instance-wide complaint and a domain block:
/// read the reason off the row it returns and an address that once bounced, on a domain the
/// operator blocked wholesale, reads as "only a bounce". The live-uniqueness index rules out two
/// rows in one slot, not two rows in different slots, so the masking combination is ordinary.
///
/// This asks the other question — "is there a block here that is not a bounce" — of every row at
/// once. A reason this build has never heard of counts as blocking, because the test is
/// `<> 'bounced'` and not a list of the five that refuse.
///
/// The caller that wants this is `public::forms`: a bounce is a deliverability fact with no door
/// a subscriber can knock on, and a double opt-in confirmation is itself the deliverability test,
/// while a complaint, a manual block, an erasure tombstone and a workspace-wide opt-out are all
/// consent decisions that a public form may not overrule. An address that cannot be normalised
/// counts as blocked: refuse, never guess.
pub async fn is_suppressed_excluding_bounce(
    db: impl PgExecutor<'_>,
    secrets: &Secrets,
    workspace_id: Uuid,
    email: &str,
) -> sqlx::Result<bool> {
    let Ok(email) = normalize_email(email) else {
        return Ok(true);
    };
    let hmac = suppression_hmac(secrets, &email);
    let domain = email.split_once('@').map_or("", |(_, d)| d);
    // Two `exists`, not one scan with an `or` across the scopes: each arm reaches its own partial
    // index, the same way `blocking` below is written.
    sqlx::query_scalar!(
        r#"select (exists (select 1 from suppressions
                            where lifted_at is null and scope = 'address' and email_hmac = $2
                              and reason <> 'bounced'
                              and (workspace_id = $1 or workspace_id is null))
                or exists (select 1 from suppressions
                            where lifted_at is null and scope = 'domain' and domain = $3
                              and reason <> 'bounced'
                              and (workspace_id = $1 or workspace_id is null)))
                  as "blocked!""#,
        workspace_id,
        hmac.as_slice(),
        domain,
    )
    .fetch_one(db)
    .await
}

/// The one predicate behind both; `email` is already normalised.
async fn blocking(
    db: impl PgExecutor<'_>,
    secrets: &Secrets,
    workspace_id: Uuid,
    email: &str,
) -> sqlx::Result<Option<SuppressionBlock>> {
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
    Ok(row.map(|r| SuppressionBlock {
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

/// Takes [`lock_address`] for a caller that is about to branch on what it finds about an address
/// and must not let the branch be timed.
///
/// `is_erased` and `unsubscribe_all` take this lock as part of doing their own work, so a caller
/// that reaches neither — a submission for an address that is already a contact, or one that is
/// suppressed — took no lock at all and ran in parallel with every other request naming that
/// address, while a submission for a *new* address serialised with them. That difference is
/// visible in one concurrent burst with no statistics at all, and it answers spec §4's third
/// anti-goal: whether an address exists in a workspace. Taking the lock first, on every path,
/// makes every submission for one address serialise whatever is true about it.
///
/// An address that does not normalise takes no lock and is not an error: there is nothing to
/// collide on, and every writer past this point refuses it anyway (`is_erased` counts it as
/// erased). The lock is held to the end of the transaction, like every other caller's, so the
/// SHORTCUT on `lock_address` applies here too — this is for a path writing one address.
pub async fn lock_address_for(
    db: &mut PgConnection,
    secrets: &Secrets,
    email: &str,
) -> sqlx::Result<()> {
    let Ok(email) = normalize_email(email) else {
        return Ok(());
    };
    lock_address(db, &suppression_hmac(secrets, &email)).await
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
        // `lifted_at is null` is **not** a narrowing: nothing can lift an erasure. `lift` refuses a
        // `gdpr_erasure` row with `erasure-not-liftable`, and the only other writer of the column
        // (`lift_unsubscribed_all`) filters on `reason = 'unsubscribed_all'`, so no row this
        // predicate matches has ever had a `lifted_at`. It is here because it is what makes the
        // partial index `suppressions_lookup_hmac (email_hmac) where lifted_at is null` usable.
        // Without it this is a `Seq Scan` of a table that only grows, taken on every contact
        // insert — including every submission of the unauthenticated public signup form, which
        // spec §9 budgets at under 500 ms p95. Measured at 20,000 rows: `Seq Scan`, 328 buffers,
        // 1.5 ms, against `Index Scan`, 4 buffers, 0.04 ms. Recorded in M1c and acted on in M1d
        // Task 9, the first caller reachable from the open internet.
        r#"select exists (select 1 from suppressions where reason = 'gdpr_erasure'
                            and lifted_at is null
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
        // No `scope` filter, so a `list` row goes with the rest: it carries the plaintext address
        // too, and erasure means nothing readable about the address survives. A domain row has a
        // null `email_hmac` and matches nothing here, so dropping the filter cannot widen this to
        // one.
        "delete from suppressions where workspace_id = $1 and email_hmac = $2",
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
/// **Why the one-click route writes here at all**, when its visible job is `list_memberships`:
/// `docs/adr/0001-contacts-have-no-status-column.md` decided that the workspace-wide opt-out is a
/// row in this table and nowhere else, *because it outlives the contact row*. A membership sweep
/// alone is undone by an erasure followed by a re-import, or by an import that names the same
/// address on a list it was never on — and spec M1d invariant 1 only protects a membership that
/// already exists as `unsubscribed`. This is the row that makes "unsubscribe from everything"
/// mean everything, including lists that do not exist yet.
///
/// `created_by_user_id` is null and `source` is `one_click`: nobody signed in did this.
///
/// **Call this before writing any `list_memberships` row in the same transaction.** It takes
/// [`lock_address`], and `contacts::routes::erase_contact` takes that lock *first* and then
/// deletes the contact, which cascades to the membership rows. A caller that locked membership
/// rows first would form an ABBA cycle with the erase path, and Postgres breaks one by aborting a
/// side with `40P01` — for the one-click route that is a `500` and a rolled-back unsubscribe.
/// The lock being *shared* with the erase path is what keeps two live rows from appearing; the
/// *order* is what keeps the two transactions from deadlocking, and only the caller can get that
/// right.
///
/// shortcut: `suppressions_live` is unique per (workspace, scope, address) **whatever the
/// reason**, so an address already blocked as `bounced` or `complained` keeps that row and gets
/// no `unsubscribed_all` one — the address is blocked either way, but lifting the bounce would
/// then unblock it. Give this table a per-reason live index if that case ever needs separating;
/// `issues/27-lifting-a-bounce-can-lose-a-one-click-unsubscribe.md` carries the argument.
/// `tests/public_pages.rs::one_click_on_an_already_blocked_address_keeps_the_block_it_has` pins
/// the behaviour so that change cannot happen unnoticed.
pub async fn unsubscribe_all(
    db: &mut PgConnection,
    secrets: &Secrets,
    workspace_id: Uuid,
    email: &str,
) -> ApiResult<bool> {
    // **Not fail-closed, unlike `blocking_suppression`, and the difference is which way "closed"
    // points.** There, refusing to answer means refusing to mail, which is safe. Here, refusing
    // would abort the transaction and take the caller's membership sweep with it — an RFC 8058
    // one-click that did not stick, which is spec M1d §4's second anti-goal. So the opt-out is
    // skipped, loudly, and the per-list unsubscribes still apply. `contacts::create` normalises,
    // so a stored address that will not normalise should be unreachable; if it is reachable the
    // log is where it surfaces. The address is not logged.
    let Ok(email) = normalize_email(email) else {
        tracing::error!(
            "a stored address does not normalise, so a one-click workspace-wide opt-out could \
             not be recorded; the per-list unsubscribes still applied"
        );
        return Ok(false);
    };
    let hmac = suppression_hmac(secrets, &email);
    lock_address(db, &hmac).await?;
    let written = sqlx::query_scalar!(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason, source) \
         values ($1, $2, 'address', $3, $4, 'unsubscribed_all', 'one_click') \
         on conflict (workspace_id, scope, email_hmac, domain) \
         where lifted_at is null and scope <> 'list' do nothing returning id",
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
///
/// **Call this before writing any `list_memberships` row in the same transaction**, for the reason
/// [`unsubscribe_all`] gives: `erase_address` deletes this row and only then deletes the contact,
/// so a caller that locked the membership rows first would deadlock with an erase of the same
/// address.
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

/// The subscriber's own refusal of **one list**, written by the preference center when a box they
/// had ticked is unticked. The durable half of that unsubscribe: the `list_memberships` row is the
/// other half, and it dies with the contact or with the list, which is the defect this exists to
/// close (spec §8 invariant 1, amended 2026-11-20; `0027_list_scope_suppressions.sql`).
///
/// It is **not** a block on the address. Nothing address-level may read it: `blocking`,
/// `is_suppressed`, `is_suppressed_excluding_bounce` and the four address arms of
/// `mailable_list_members` all filter on `scope`, the admin API reads `address_suppressions`, and
/// `Scope` and `Reason` have no variant that would decode one. The reader that honours it is the
/// view's fifth clause, the importer, and `public::forms`.
///
/// The `list_name` snapshot is read from `lists` here rather than taken from the caller, so a
/// rename between rendering the page and saving it cannot store a name the list never had.
///
/// **`for key share` on that read, and it is not decoration.** Without it the plain `select` takes
/// the statement's READ COMMITTED snapshot, so a rename committing in that window is invisible: the
/// row stores the OLD name, and the rename's trigger has already run and will not come back for it.
/// The foreign key's own `FOR KEY SHARE` is taken *after* the row is built, which is too late to
/// change what was read. Nothing looks wrong while the list lives, because every reader matches on
/// `list_id` — and then deleting that list and recreating it under its current name finds no
/// correctly-named orphan to adopt, and the next import resurrects the person. That is the defect
/// this whole scope exists to close, reintroduced by one statement. `name` belongs to
/// `lists_workspace_id_name_key`, so renaming is a key update: `for key share` conflicts with it,
/// the insert blocks and re-reads, and the stored name is the committed one. Measured against
/// Postgres 16: plain `select` stores the stale name and does not block; `for key share` blocks
/// for the writer's lifetime and stores the new one.
///
/// **The conflict target is the LIST, and it does not adopt anything.** It used to conflict on
/// `list_name` and `do update set list_id`, which could not see a live row already on this list
/// under a different name — the state the rename trigger used to manufacture — so it inserted a
/// second refusal on one list, after which every later rename of that list failed. Worse, the
/// `do update` silently re-pointed the ORPHAN it had conflicted with, so the address's refusal of
/// the *deleted* list of that name was gone and recreating one no longer restored it: the
/// delete-and-recreate resurrection this whole scope exists to close. Adopting an orphan is the
/// trigger's job, where the guard that keeps `suppressions_live_list` satisfiable lives; this
/// statement's only question is whether this address already refuses this list, and `do nothing`
/// is the whole of the answer.
///
/// **Call this before writing any `list_memberships` row in the same transaction**, for exactly
/// the reason [`unsubscribe_all`] gives: it takes [`lock_address`], and the erase path takes that
/// lock first and only then deletes the contact, which cascades to the membership rows.
///
/// **The lock it takes is for the name it reads, not for the lock order.** It happens to put this
/// statement in order too, and for one round that was the whole argument — but ordering is the
/// path's job now, taken once in `save` through `lists::lock_lists`, because arguing it per call
/// is what produced four deadlocks in three rounds. See
/// `docs/adr/0005-one-lock-order-for-every-path.md`. What this line is load-bearing for is the
/// name, and `tests/suppressions.rs::a_rename_that_commits_mid_save_does_not_leave_the_old_name_on_the_refusal`
/// is what holds it.
///
/// **What that wait costs, measured in what holds the row.** A rename holds it for one `UPDATE`
/// and its trigger. A **list deletion** holds it for its whole cascade — `list_memberships`,
/// `jobs`, `form_lists`, and the `set null` sweeps over `suppressions` and `consent_log` — because
/// deleting the row conflicts with `FOR KEY SHARE` exactly as a key update does. Every untick of
/// that list now queues behind that, where before this line an untick-only save took no `lists`
/// lock at all. It is a short wait on a small workspace and an unbounded one on a large list, and
/// it is the price of the row not storing a name the list has already lost.
///
/// **And if the list is deleted while the untick waits, the refusal is dropped.** The select then
/// matches nothing, the insert writes nothing, and this returns `Ok(())`. Mostly that is right —
/// there is no list left to refuse, and the membership rows went with it — but if a list of that
/// name is later recreated, nothing refuses it, which is the one case this scope exists for. It is
/// logged rather than fixed: writing the refusal anyway needs a name, and the only name left is
/// the caller's, which is the stale read this `for key share` was added to stop trusting.
/// `issues/37`.
pub async fn unsubscribe_list(
    db: &mut PgConnection,
    secrets: &Secrets,
    workspace_id: Uuid,
    list_id: Uuid,
    email: &str,
) -> ApiResult<()> {
    // Not fail-closed, for the reason [`unsubscribe_all`] records: refusing would abort the
    // transaction and take the subscriber's membership changes with it. The address is not logged.
    let Ok(email) = normalize_email(email) else {
        tracing::error!(
            "a stored address does not normalise, so a per-list unsubscribe could not be made \
             durable; the membership row still records it"
        );
        return Ok(());
    };
    let hmac = suppression_hmac(secrets, &email);
    lock_address(db, &hmac).await?;
    // The `lists` row is read once, in a materialised CTE, so that "was there a list at all" is
    // answered by the same locked read the row is built from. `rows_affected` cannot answer it:
    // the insert also writes nothing when this address already refuses this list, which is the
    // ordinary idempotent case and not a deletion.
    let list_exists = sqlx::query_scalar!(
        r#"with l as materialized (
               select id, name from lists where workspace_id = $2 and id = $5 for key share
           ), written as (
               insert into suppressions (id, workspace_id, scope, email, email_hmac, reason,
                                         source, list_id, list_name)
               select $1, $2, 'list', $3, $4, 'unsubscribed_list', 'preference_center', l.id,
                      l.name
                 from l
               on conflict (workspace_id, email_hmac, list_id)
                 where lifted_at is null and scope = 'list' and list_id is not null
               do nothing
               returning id
           )
           select exists (select 1 from l) as "list_exists!""#,
        Uuid::now_v7(),
        workspace_id,
        email,
        hmac.as_slice(),
        list_id,
    )
    .fetch_one(db)
    .await?;
    // No `lists` row means it was gone by the time the lock was granted: an operator deleted the
    // list while this untick waited behind the deletion's own lock on that row. There is nothing
    // left to refuse and nothing was written, which is why this is a warning and not an error —
    // but a list of that name recreated later is refused by nothing, so it is said out loud rather
    // than discovered at send time. No address is logged. `issues/37`.
    if !list_exists {
        tracing::warn!(
            %list_id,
            "a per-list unsubscribe was not recorded because the list was deleted while the \
             request waited for it; nothing refuses a list of that name if one is recreated"
        );
    }
    Ok(())
}

/// Lifts the subscriber's own refusal of one list, and **only** that one. The per-list mirror of
/// [`lift_unsubscribed_all`], called where that is called and nowhere else. Ticking a box in the
/// preference center is a genuine re-subscribe, and leaving the refusal standing would show them a
/// box that ticks and changes nothing a send would honour.
///
/// `scope = 'list'` and `list_id = $3` are the whole of what this may touch. An import must never
/// reach it: `tests/import.rs` pins that direction, because getting it backwards turns a consent
/// feature into a trap.
///
/// **It takes no `lists` row of its own; its caller has taken them all.** This used to read the
/// row `for key share` to put itself in the lock order, which worked and then turned out to be the
/// wrong altitude: `save` re-ticks one box and unticks another, and the only reason a per-call
/// lock was enough is that those two sets are disjoint. Three review rounds of that reasoning
/// produced four deadlocks, so position 2 is now the path's job and is taken once, for every
/// candidate list, in `save`. `docs/adr/0005-one-lock-order-for-every-path.md`.
///
/// `tests/lock_order.rs::a_re_tick_and_a_rename_do_not_deadlock` is what holds it, and it holds
/// `save`'s `lock_lists` rather than anything here.
pub async fn lift_unsubscribed_list(
    db: &mut PgConnection,
    secrets: &Secrets,
    workspace_id: Uuid,
    list_id: Uuid,
    email: &str,
) -> ApiResult<()> {
    let Ok(email) = normalize_email(email) else {
        return Ok(());
    };
    let hmac = suppression_hmac(secrets, &email);
    sqlx::query!(
        // The CTE is here for the list's NAME, which the orphan arm below needs, and no longer to
        // order anything: `docs/adr/0005` puts position 2 on the path, and `save` — this
        // function's only caller — takes every candidate list through `lists::lock_lists` before
        // it gets here. A second `for key share` on the same row would be a guard no test can see
        // fail, which is the shape this branch has learnt to distrust. A future caller that takes
        // no list rows of its own is the ADR's problem, not this statement's.
        // **Every refusal of this list BY NAME goes too, not only the one carrying its id.** An
        // orphan naming this list is a refusal of "the list called X", and the subscriber has just
        // asked for the list called X; leaving it would let the next delete-and-recreate adopt it
        // and refuse the list they explicitly asked for. That is reachable — two orphans of one
        // name, one adopted, this lifting only the adopted one, then a delete and a recreate — and
        // it is why `0027` no longer calls a surviving duplicate one nobody reads.
        //
        // It cannot make a list deletion fail: lifting only ever takes rows OUT of the live
        // partial indexes, so nothing the `on delete set null` sweep writes can collide because of
        // it.
        "with locked as materialized ( \
             select id, name from lists where workspace_id = $1 and id = $3 \
         ) \
         update suppressions s set lifted_at = now() \
           from locked l \
          where s.workspace_id = $1 and s.scope = 'list' and s.email_hmac = $2 \
            and s.lifted_at is null \
            and (s.list_id = l.id or (s.list_id is null and s.list_name = l.name))",
        workspace_id,
        hmac.as_slice(),
        list_id,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Which of `list_ids` this address has refused: the subset `public::forms` must leave out of a
/// submission. Matches on `list_id` alone, which `0027`'s two triggers keep filled for every list
/// that exists.
///
/// An address that cannot be normalised refuses every list: fail closed, never guess. That is
/// unreachable from the one caller, which normalised before it got here.
pub async fn suppressed_lists(
    db: impl PgExecutor<'_>,
    workspace_id: Uuid,
    email: &str,
    list_ids: &[Uuid],
) -> sqlx::Result<Vec<Uuid>> {
    let Ok(email) = normalize_email(email) else {
        return Ok(list_ids.to_vec());
    };
    sqlx::query_scalar!(
        r#"select l as "l!" from unnest($3::uuid[]) l
            where exists (select 1 from suppressions s where s.lifted_at is null
                            and s.workspace_id = $1 and s.scope = 'list'
                            and s.email = $2 and s.list_id = l)"#,
        workspace_id,
        email,
        list_ids,
    )
    .fetch_all(db)
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
///
/// `address_suppressions` and not `suppressions`, here and in [`list`] and [`lift`]: a `list`-scope
/// row is a subscriber's refusal of one list and is no part of the workspace blocklist this API
/// shows, creates and lifts. The view is what makes that so rather than a predicate repeated three
/// times — and if one of these were ever pointed back at the table, `Scope` and `Reason` have no
/// variant to decode a list row into, so it would fail loudly instead of showing a blank block an
/// operator could lift.
pub async fn get(
    db: &mut PgConnection,
    workspace_id: Option<Uuid>,
    id: Uuid,
) -> ApiResult<Suppression> {
    sqlx::query_as!(
        Suppression,
        r#"select id as "id!", workspace_id, scope as "scope!: Scope", email, domain,
                  reason as "reason!: Reason", note, created_at as "created_at!",
                  created_by_user_id, lifted_at, lifted_by_user_id
           from address_suppressions where id = $2 and workspace_id is not distinct from $1"#,
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
        r#"select id as "id!", workspace_id, scope as "scope!: Scope", email, domain,
                  reason as "reason!: Reason", note, created_at as "created_at!",
                  created_by_user_id, lifted_at, lifted_by_user_id
           from address_suppressions
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
        r#"select reason as "reason!: Reason", lifted_at from address_suppressions
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
