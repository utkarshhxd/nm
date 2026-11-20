// SPDX-License-Identifier: AGPL-3.0-only

//! A selection is a **frozen set of contact ids**, never a predicate re-run when the action
//! executes. `POST /contacts/selection` evaluates the predicate once and writes the ids it matched
//! into `selection_snapshot_members`; the token that comes back carries nothing but
//! `{workspace_id, snapshot_id, issued}`, and every bulk action here narrows `contacts` by that
//! frozen set.
//!
//! Re-evaluating the predicate at action time is the compliance bug issue `04` records: a second
//! editor writing a tag inside the token's hour changes what the action hits. No timestamp bound
//! repairs it, because negation inverts — `not_has(tag)` bounded by `t` becomes *true* for a
//! contact whose tag row is deleted, so a contact that was never shown is acted on, and proving a
//! fact was *absent* at `t` is impossible once the row recording it is gone. See
//! `docs/adr/0004-selection-is-a-materialised-set.md` for both measured counterexamples.
//!
//! **A snapshot narrows `contacts`; it never supplies ids.** Reading ids *out of* the members
//! table and inserting them elsewhere hits a foreign-key violation the moment one selected contact
//! has been deleted since selection, which kills the whole action. Narrowing degrades correctly:
//! the deleted contact is simply absent and the `affected` count is short.
//!
//! Every bulk action here is one set-based statement. None loops over ids.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{AssertSqlSafe, PgConnection, PgPool, Postgres, QueryBuilder, SqlSafeStr};
use uuid::Uuid;

use super::{ContactFilter, push_filter};
use crate::{
    audit::Actor,
    consent,
    contacts::import::staging::SQL_UUID_V7,
    crypto::{secrets::Secrets, tokens},
    error::{ApiError, ApiResult, FieldError},
    lists::{MembershipSource, MembershipStatus},
    segments::{compile::push_rules, rules::Group},
};

/// Domain-separation label for the selection-token key: a token can never be replayed as any other
/// HMAC'd value, nor the reverse.
///
/// `v3` because the payload's *meaning* changed: it carried `{workspace_id, filter, as_of,
/// issued}` — a predicate to re-run — and now carries the id of a set that is already written
/// down. A `v2` token must fail the HMAC rather than deserialise into something this build would
/// misread: it has no `snapshot_id`, and nothing here may ever read a missing one as "no
/// narrowing", which on `bulk/delete` is the whole workspace. Bumping the label refuses every
/// outstanding token as `selection-invalid` — the path the UI already handles by asking the person
/// to select again — and at most one hour of tokens can be outstanding.
const SELECTION_DOMAIN: &str = "nm:selection:v3";

/// A token older than this is refused. An hour is long enough to read a confirmation dialog and
/// short enough that "everyone matching X, as of this morning" is not acted on tomorrow.
const TTL: Duration = Duration::from_secs(60 * 60);

/// The most ids a client may exclude from a selection: 10 000 is about 400 KB of JSON. Unbounded,
/// one request could carry hundreds of megabytes into a `<> all(...)`.
pub const MAX_EXCLUDED: usize = 10_000;

/// The most ids a client may name in an explicit id-list selection; the same bound, for the same
/// reason, as `MAX_EXCLUDED`.
pub const MAX_IDS: usize = 10_000;

/// What a signed token carries: which workspace, which frozen set, and how many contacts were in
/// it. The field names are part of the token format: renaming one invalidates tokens already
/// issued.
///
/// **No `serde(default)` on any field**, with `deny_unknown_fields` on top. A payload this build
/// did not write is refused rather than read: a `snapshot_id` defaulted to anything at all is a
/// selection with no frozen set behind it, and the one thing a selection must never do is resolve
/// to more contacts than the version that issued it meant. The HMAC refuses an outstanding `v2`
/// token first; this is the second lock, for the case where only the payload's shape differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub workspace_id: Uuid,
    /// The materialised set, by id. `Uuid::now_v7()`, so the issue time comes back out of the id
    /// itself and the TTL is checked before the database is touched (section 8.5).
    pub snapshot_id: Uuid,
    /// How many contacts were frozen: `selection_snapshots.size`, which is the materialising
    /// insert's own `rows_affected` and the number the person was shown before they confirmed.
    pub issued: u64,
}

/// What a bulk action acts on: a set of contacts that is already frozen, whichever way it was
/// named.
///
/// An enum, not a struct of optional parts, because the state it makes unrepresentable is the one
/// that empties a workspace: a selection carrying neither a snapshot nor ids narrows `contacts` by
/// nothing at all, and `bulk/delete` of "nothing at all" is every contact there is. The house rule
/// is unrepresentable, not merely unwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// A materialised snapshot, named by a signed token and found live in `selection_snapshots`.
    Snapshot {
        token: Token,
        /// The predicate the snapshot was frozen for, as it was sent, copied out of
        /// `selection_snapshots.filter` for the audit row the action writes (section 8.7). The
        /// token carries no predicate and the snapshot is deleted within the hour, so without this
        /// copy the log would read *"deleted 417,004 contacts, snapshot 0192…"* against a row that
        /// no longer exists. `q` is redacted by the audit copy, not here.
        filter: Value,
    },
    /// The ticked rows of a grid: these contacts and no others. An explicit id list is already a
    /// frozen set, so it carries no `snapshot_id` and nothing is materialised for it. It is still
    /// scoped by `workspace_id`, so an id from another workspace selects nothing.
    ///
    /// An **empty** `ids` is representable here and must stay refused at the door
    /// (`check_ids`): `id = any('{}')` is false for every row, so it acts on nothing today, but
    /// it is one careless edit away from meaning "no restriction". Nothing in the product builds
    /// one — only a route does, from a body `check_ids` has already seen.
    Ids { workspace_id: Uuid, ids: Vec<Uuid> },
}

impl Selection {
    /// A selection of exactly these contacts. `issued` is how many were named; the resolved count
    /// can only be lower (an id from another workspace matches nothing), so an id selection can
    /// never widen.
    pub fn of_ids(workspace_id: Uuid, ids: Vec<Uuid>) -> Selection {
        Selection::Ids { workspace_id, ids }
    }

    /// The workspace this selection belongs to. Every statement built from it is scoped by this,
    /// whichever arm it came from.
    pub fn workspace_id(&self) -> Uuid {
        match self {
            Selection::Snapshot { token, .. } => token.workspace_id,
            Selection::Ids { workspace_id, .. } => *workspace_id,
        }
    }

    /// How many contacts this selection stood for when it was made — the number the confirmation
    /// dialog showed.
    pub fn issued(&self) -> u64 {
        match self {
            Selection::Snapshot { token, .. } => token.issued,
            Selection::Ids { ids, .. } => ids.len() as u64,
        }
    }

    /// True when the action resolved to more contacts than the selection was made for.
    ///
    /// **It cannot be true**, which is the point of materialising the set (section 8.6): nothing
    /// re-evaluates a predicate, so no write by anybody else can add a contact to a frozen set.
    /// `selected` comes out of the statement that performed the action, over the frozen set less
    /// the exclusions, so `selected ≤ |S| − |E ∩ S| ≤ |S| = issued` for a snapshot and
    /// `selected ≤ |ids| = issued` for an id list. It is kept as defence in depth — it costs
    /// nothing and it is the last line if the snapshot join is ever bypassed — and `tests/bulk.rs`
    /// asserts it never fires.
    ///
    /// **The exclusions are not added back.** M1a compared `selected + excluded.len() > issued`,
    /// which was right while `issued` was the whole match at issue time and `selected` the match
    /// *now* less the exclusions: two different quantities, and comparing them raw let a widening
    /// of up to `MAX_EXCLUDED` through. Here both sides count the same frozen set, so the
    /// exclusion term is not merely redundant but wrong: `issued = |S|` and
    /// `selected = |S| − |E ∩ S|`, so an excluded id that is *not* in the snapshot made the sum
    /// exceed `issued` and refused `409 selection-widened` on the most ordinary action the grid
    /// produces — unticking a row the live view shows and the frozen set does not. A tripwire that
    /// fires in normal operation is a tripwire somebody on call deletes.
    pub fn widened(&self, selected: u64) -> bool {
        selected > self.issued()
    }
}

fn mac(secrets: &Secrets, payload: &[u8]) -> [u8; 32] {
    tokens::hmac_sha256(secrets.derive_key(SELECTION_DOMAIN).as_slice(), payload)
}

/// `base64url(json).base64url(hmac)`. Signed, not encrypted: it carries no secret, but a client
/// cannot alter the workspace, the snapshot it names, or the count it was issued for.
pub fn issue(secrets: &Secrets, token: &Token) -> String {
    let payload = serde_json::to_vec(token).expect("a selection token always serialises");
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(mac(secrets, &payload)),
    )
}

/// `422 selection-invalid` for anything that is not a token this instance issued, and `422
/// selection-expired` once the snapshot id's own issue time is over an hour old. The caller
/// compares `workspace_id` with the workspace it authorised.
///
/// The age is checked here, **before the database is touched**, and again by the action's `sel`
/// CTE, which joins `selection_snapshots … and expires_at > now()`. Both, not either (section
/// 8.5): this one needs no query and refuses a Friday token on Monday, and the CTE's join is what
/// makes expiry independent of the reaper, which clears one snapshot a minute and can be hours
/// behind. `snapshot_id` is a UUIDv7 exactly so this check is possible at all — a v4 would leave
/// the TTL enforceable only by the row.
pub fn parse(secrets: &Secrets, token: &str) -> ApiResult<Token> {
    let invalid = || ApiError::Unprocessable("selection-invalid");
    let (payload, sig) = token.split_once('.').ok_or_else(invalid)?;
    let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| invalid())?;
    let sig = URL_SAFE_NO_PAD.decode(sig).map_err(|_| invalid())?;
    if !tokens::constant_time_eq(&mac(secrets, &payload), &sig) {
        return Err(invalid());
    }
    let token: Token = serde_json::from_slice(&payload).map_err(|_| invalid())?;
    let issued = token.snapshot_id.get_timestamp().ok_or_else(invalid)?;
    let (secs, nanos) = issued.to_unix();
    let issued = UNIX_EPOCH + Duration::new(secs, nanos);
    // A clock that went backwards makes the age an error: treat it as zero rather than refusing.
    if SystemTime::now().duration_since(issued).unwrap_or_default() > TTL {
        return Err(ApiError::Unprocessable("selection-expired"));
    }
    Ok(token)
}

/// `422` if more than `MAX_EXCLUDED` ids are excluded.
pub fn check_excluded(excluded: &[Uuid]) -> ApiResult<()> {
    if excluded.len() > MAX_EXCLUDED {
        return Err(ApiError::Validation(vec![FieldError::new(
            "excluded_ids",
            format!("at most {MAX_EXCLUDED} ids can be excluded"),
        )]));
    }
    Ok(())
}

/// `422` if no ids, or more than `MAX_IDS`, are named.
pub fn check_ids(ids: &[Uuid]) -> ApiResult<()> {
    if ids.is_empty() || ids.len() > MAX_IDS {
        return Err(ApiError::Validation(vec![FieldError::new(
            "ids",
            format!("name between 1 and {MAX_IDS} ids"),
        )]));
    }
    Ok(())
}

/// The most contacts one selection may hold. Applied as `limit MAX_SELECTION + 1` in the
/// materialising insert: if the extra row arrives the transaction rolls back and the refusal says
/// *more than a million* without naming a count, because naming one needs a second pass that
/// doubles the work and races with the insert.
pub const MAX_SELECTION: u64 = 1_000_000;

/// How long a snapshot lives, in seconds. The same hour as the token's [`TTL`], and derived from
/// it: a snapshot that outlived its token would be dead weight, and one that died first would
/// refuse a token this instance still considers valid.
const SNAPSHOT_TTL_SECS: f64 = TTL.as_secs() as f64;

/// The sweep runs this often, deleting at most one expired snapshot each time (section 8.10).
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// A materialised selection: the frozen set, by id, and how many contacts are in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// `Uuid::now_v7()`, so the issue time comes back out of the id and the TTL can be checked
    /// before the database is touched (section 8.5).
    pub id: Uuid,
    /// The materialising insert's own `rows_affected`, never a separate count: a count taken
    /// separately could disagree with what was written.
    pub size: u64,
}

/// Evaluates the predicate once and writes down the ids it matched, as one transaction.
///
/// This is what closes issue `04`. A predicate re-evaluated when the action runs resolves to
/// whatever the table says *then*: a second editor writing a tag inside the token's hour changes
/// what the action hits, and no timestamp bound fixes it, because proving a fact was *absent* at
/// `t` is impossible once the row recording it has been deleted (section 8.1). Identity recorded
/// has nothing to infer.
///
/// Three things here are requirements rather than style:
///
/// 1. **The workspace clause is written here.** Neither `push_filter` nor `push_rules` emits one —
///    the compiler's structural test proves its fragment is more than a workspace clause, and it
///    cannot prove the caller added one. Without the clause below this materialises every tenant's
///    contacts into one snapshot.
/// 2. **The parent row and the members commit together.** A parent that commits alone leaves a
///    snapshot whose `size` claims N and which holds no members.
/// 3. **The insert runs in a detached task.** It is measured at 4,887 ms for 1M rows, and a handler
///    future is dropped when the client disconnects. Detached, the statement finishes on its own
///    connection rather than being cancelled mid-protocol, which costs the pool a connection.
///
/// `filter_doc` is stored verbatim for the audit row the bulk action will write (section 8.7); `q`
/// is redacted by that copy, not here, so the snapshot stays self-describing while it lives.
///
/// `cap` is [`MAX_SELECTION`] for every caller in the product. It is a parameter rather than the
/// constant so that the refusal above has a test: a million contacts cannot be made in a test, and
/// a refusal nothing witnesses is the shape of assertion this project keeps finding passes against
/// broken code.
pub async fn materialise(
    pool: PgPool,
    workspace_id: Uuid,
    actor: Actor,
    filter_doc: Value,
    filter: ContactFilter,
    rules: Option<Group>,
    cap: u64,
) -> ApiResult<Snapshot> {
    tokio::spawn(async move {
        materialise_in(pool, workspace_id, actor, filter_doc, filter, rules, cap).await
    })
    .await
    .map_err(|e| ApiError::Internal(e.into()))?
}

async fn materialise_in(
    pool: PgPool,
    workspace_id: Uuid,
    actor: Actor,
    filter_doc: Value,
    filter: ContactFilter,
    rules: Option<Group>,
    cap: u64,
) -> ApiResult<Snapshot> {
    let (created_by, created_by_key) = columns(actor);
    let id = Uuid::now_v7();
    let mut tx = pool.begin().await?;
    // The actor's previous live selection is superseded: the UI has one active selection at a
    // time, so the old snapshot is dead weight, and dropping it bounds one person's footprint to
    // one snapshot of up to 158 MB.
    //
    // Matched on the actor's real identity, whichever class it is, and `is not distinct from` so
    // that the NULL half of the pair compares. Matching on `created_by` alone would supersede
    // nothing for an API key (NULL never equals NULL), and collapsing both to one nullable column
    // would give every key in the workspace a single shared slot, so two keys selecting at once
    // would destroy each other's snapshots — a correctness bug traded for a disk one.
    sqlx::query!(
        "delete from selection_snapshots where workspace_id = $1 \
         and created_by is not distinct from $2::uuid \
         and created_by_key is not distinct from $3::uuid",
        workspace_id,
        created_by,
        created_by_key,
    )
    .execute(&mut *tx)
    .await?;
    // `size` is written after the insert, which is the only thing that knows it.
    sqlx::query!(
        "insert into selection_snapshots \
           (id, workspace_id, created_by, created_by_key, filter, size, expires_at) \
         values ($1, $2, $3, $4, $5, 0, now() + make_interval(secs => $6))",
        id,
        workspace_id,
        created_by,
        created_by_key,
        filter_doc,
        SNAPSHOT_TTL_SECS,
    )
    .execute(&mut *tx)
    .await?;
    let mut q = members_insert(workspace_id, id, &filter, rules.as_ref(), cap);
    let size = q.build().execute(&mut *tx).await?.rows_affected();
    if size > cap {
        // Rolled back, so neither the parent nor the row over the cap survives.
        tx.rollback().await?;
        return Err(ApiError::UnprocessableDetail {
            code: "selection-too-large",
            // Built from `cap`, never from a count: naming an exact number needs a second pass
            // that doubles the work and races with the insert.
            detail: format!(
                "more than {} contacts match; narrow the selection and try again",
                with_separators(cap)
            ),
        });
    }
    sqlx::query!(
        "update selection_snapshots set size = $3 where workspace_id = $1 and id = $2",
        workspace_id,
        id,
        i64::try_from(size).unwrap_or(i64::MAX),
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Snapshot { id, size })
}

/// `(created_by, created_by_key)` for an actor: exactly one is set, so the pair is the actor's
/// identity and two actors of different classes can never collide.
///
/// `System` makes no selection — nothing in the product calls this without a request behind it —
/// and an unattributed snapshot is simply never superseded; it ages out with the TTL.
fn columns(actor: Actor) -> (Option<Uuid>, Option<Uuid>) {
    match actor {
        Actor::User(id) => (Some(id), None),
        Actor::ApiKey(id) => (None, Some(id)),
        Actor::System => (None, None),
    }
}

/// `1000000` as `1,000,000`. The refusal is a sentence a person reads.
fn with_separators(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.char_indices() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `insert into selection_snapshot_members … select … from contacts where … limit cap + 1`.
///
/// The limit is one past the cap, so one extra row is the whole evidence that the match is too
/// big — and the only evidence taken, because counting first doubles the work and races with the
/// insert. It is formatted rather than bound so that the SQL a test reads carries the number.
fn members_insert(
    workspace_id: Uuid,
    snapshot_id: Uuid,
    filter: &ContactFilter,
    rules: Option<&Group>,
    cap: u64,
) -> QueryBuilder<Postgres> {
    let mut q = QueryBuilder::<Postgres>::new(
        "insert into selection_snapshot_members (workspace_id, snapshot_id, contact_id) select ",
    );
    q.push_bind(workspace_id)
        .push(", ")
        .push_bind(snapshot_id)
        .push(", id from contacts where workspace_id = ")
        .push_bind(workspace_id);
    push_filter(&mut q, filter);
    if let Some(group) = rules {
        q.push(" and ");
        push_rules(&mut q, group);
    }
    // A `u64` constant: nothing a request carries reaches this.
    q.push(format!(" limit {}", cap + 1));
    q
}

/// Deletes **one** expired snapshot and its members, and reports whether there was one.
///
/// One, not every expired one: a snapshot holds up to 158 MB and its cascade is measured at
/// 850 ms, so a sweep of the whole backlog is an unbounded amount of work. M0's session sweep —
/// `delete from sessions where expires_at <= now()`, inline in a user's request — is sized for a
/// 200-byte row and is not the shape to copy here (section 8.10).
///
/// Nothing depends on this having run: a bulk action joins `expires_at > now()` itself, so an
/// un-reaped snapshot is already unusable. This only reclaims the space.
pub async fn reap_one_expired(pool: &PgPool) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "delete from selection_snapshots where id = \
           (select id from selection_snapshots where expires_at <= now() \
            order by expires_at limit 1)",
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// The sweep, as its own task: one expired snapshot a minute, for as long as the process runs.
///
/// Its own task, not a ride on an existing request, for the two reasons the session sweep cannot
/// serve (section 8.10): that one runs only while somebody is logged in, so an instance whose
/// editors go home on Friday would hold every Friday snapshot until Monday; and its cost would
/// land on the hot path of a stranger's authenticated request.
pub async fn sweep_expired_snapshots(pool: PgPool) {
    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        match reap_one_expired(&pool).await {
            Ok(true) => tracing::debug!("reaped an expired selection snapshot"),
            Ok(false) => {}
            // Best effort, like the other sweeps: the next pass tries again.
            Err(e) => tracing::warn!(error = ?e, "selection snapshot sweep failed"),
        }
    }
}

/// Resolves a token against the live snapshot it names, inside the action's own transaction.
///
/// `422 selection-expired` when there is no such live row: past `expires_at`, swept by the reaper,
/// or superseded by a newer selection of the same actor's. Those are one answer on purpose — they
/// are the same thing to the person holding the token, and telling them apart would disclose
/// whether a snapshot id exists.
///
/// Read in the action's transaction, so this `now()` and the `sel` CTE's `now()` are the one
/// transaction timestamp and cannot disagree about expiry.
pub async fn resolve(db: &mut PgConnection, token: Token) -> ApiResult<Selection> {
    let row = sqlx::query!(
        "select filter, size from selection_snapshots \
         where workspace_id = $1 and id = $2 and expires_at > now()",
        token.workspace_id,
        token.snapshot_id,
    )
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        return Err(ApiError::Unprocessable("selection-expired"));
    };
    // `issued` was signed from this row's `size` when the token was minted, so a disagreement is
    // this instance contradicting itself, not anything a client can cause. Refusing here is what
    // lets the caller compare the confirmed count against `issued` and have compared it against
    // `selection_snapshots.size` (section 9.2).
    if row.size != i64::try_from(token.issued).unwrap_or(i64::MAX) {
        return Err(ApiError::Unprocessable("selection-invalid"));
    }
    Ok(Selection::Snapshot {
        token,
        filter: row.filter,
    })
}

/// `422 selection-expired` unless this workspace has a live snapshot with this id.
///
/// For the read path, which has no token to parse: `GET /contacts?snapshot_id=…`. An empty page
/// would be the wrong answer — "zero contacts matched" is a different fact from "this selection is
/// gone", and the grid cannot tell someone to select again if the server will not say which. Past
/// `expires_at`, swept, superseded and belonging to another workspace are one answer, as they are
/// for a bulk action: the caller is already authorised in this workspace, so the refusal is no
/// oracle, and a foreign id is indistinguishable from one that never existed.
pub async fn require_live(
    db: &mut PgConnection,
    workspace_id: Uuid,
    snapshot_id: Uuid,
) -> ApiResult<()> {
    let live = sqlx::query_scalar!(
        "select exists (select 1 from selection_snapshots \
         where workspace_id = $1 and id = $2 and expires_at > now())",
        workspace_id,
        snapshot_id,
    )
    .fetch_one(db)
    .await?;
    if live == Some(true) {
        return Ok(());
    }
    Err(ApiError::Unprocessable("selection-expired"))
}

/// Appends ` and id in (<the frozen set>)` to a query that has already constrained
/// `contacts.workspace_id`. The grid's `?snapshot_id=` and every bulk action share it, so "the
/// contacts this snapshot shows" and "the contacts this snapshot acts on" cannot drift apart.
///
/// Two properties here are requirements, not style:
///
/// 1. **It narrows `contacts`; it never supplies ids.** `insert … select contact_id from
///    selection_snapshot_members` would hit a foreign-key violation the moment a single selected
///    contact had been deleted since selection — the members table deliberately carries no foreign
///    key to `contacts` (section 8.6) — and that violation kills the whole action, not one row.
///    Narrowed, a deleted contact is simply absent.
/// 2. **It joins the parent with `expires_at > now()`.** Expiry then does not depend on the reaper
///    having run (section 8.5), and a snapshot whose row is gone narrows to the empty set rather
///    than to everything.
pub(crate) fn push_snapshot(q: &mut QueryBuilder<Postgres>, workspace_id: Uuid, snapshot_id: Uuid) {
    q.push(
        " and id in (select m.contact_id from selection_snapshot_members m \
         join selection_snapshots s on s.workspace_id = m.workspace_id and s.id = m.snapshot_id \
         where m.workspace_id = ",
    )
    .push_bind(workspace_id)
    .push(" and m.snapshot_id = ")
    .push_bind(snapshot_id)
    .push(" and s.expires_at > now())");
}

/// Appends the selection's conditions on `contacts`, after `... where `.
fn push_selected(q: &mut QueryBuilder<Postgres>, sel: &Selection, excluded: &[Uuid]) {
    q.push("workspace_id = ").push_bind(sel.workspace_id());
    match sel {
        // Literal ids: the caller named these contacts, and a route refuses `excluded_ids`
        // alongside them.
        Selection::Ids { ids, .. } => {
            q.push(" and id = any(").push_bind(ids.clone()).push(")");
        }
        Selection::Snapshot { token, .. } => {
            push_snapshot(q, token.workspace_id, token.snapshot_id);
            q.push(" and id <> all(")
                .push_bind(excluded.to_vec())
                .push(")");
        }
    }
}

/// What a bulk statement did. Both numbers come out of the one statement, so they describe the same
/// rows: `selected` is how much of the frozen set still exists, less the exclusions, and `affected`
/// is how many of those the action actually changed. A count taken separately could disagree with
/// the action, which is worse than no count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub selected: u64,
    pub affected: u64,
}

/// The whole statement a bulk action runs:
/// `with sel as (<the selection>), done as (<action> returning 1) select count(sel), count(done)`.
/// `action` appends the data-changing statement, which reads `sel` for the contact ids.
///
/// Built here rather than inside [`run`] so that a test can read it. `run` executes, so the one
/// thing a test could otherwise never see is the opening — `select id from **contacts**` — which
/// is the half of section 8.2 that matters: the ids are never taken *out of* the members table.
fn statement(
    sel: &Selection,
    excluded: &[Uuid],
    action: impl FnOnce(&mut QueryBuilder<Postgres>),
) -> QueryBuilder<Postgres> {
    statement_logging(sel, excluded, action, "1", |_| {})
}

/// [`statement`] with the action's `returning` column named, and `log` free to append further
/// CTEs that read `done` — for an action that has to write something else for every row it
/// changed, in the one statement that changed them.
fn statement_logging(
    sel: &Selection,
    excluded: &[Uuid],
    action: impl FnOnce(&mut QueryBuilder<Postgres>),
    returning: &str,
    log: impl FnOnce(&mut QueryBuilder<Postgres>),
) -> QueryBuilder<Postgres> {
    let mut q = QueryBuilder::<Postgres>::new("with sel as (select id from contacts where ");
    push_selected(&mut q, sel, excluded);
    q.push("), done as (");
    action(&mut q);
    // `AssertSqlSafe`: `returning` is one of the two literals this private function is called
    // with, and nothing a client sent reaches it. `QueryBuilder::push` takes any `Display` and
    // would not have stopped it either way, which is why the audit is written down here.
    let tail = AssertSqlSafe(format!(" returning {returning})")).into_sql_str();
    q.push(tail.as_str());
    log(&mut q);
    q.push(" select (select count(*) from sel)::bigint, (select count(*) from done)::bigint");
    q
}

async fn run(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
    action: impl FnOnce(&mut QueryBuilder<Postgres>),
) -> ApiResult<Counts> {
    let mut q = statement(sel, excluded, action);
    let (selected, affected): (i64, i64) = q.build_query_as().fetch_one(db).await?;
    // Counts are never negative.
    Ok(Counts {
        selected: selected.unsigned_abs(),
        affected: affected.unsigned_abs(),
    })
}

/// Puts the tag on every selected contact that lacks it; `affected` is how many were newly tagged.
pub async fn tag(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
    tag_id: Uuid,
) -> ApiResult<Counts> {
    run(db, sel, excluded, |q| {
        q.push("insert into contact_tags (workspace_id, contact_id, tag_id) select ")
            .push_bind(sel.workspace_id())
            .push(", id, ")
            .push_bind(tag_id)
            .push(" from sel on conflict (contact_id, tag_id) do nothing");
    })
    .await
}

/// Takes the tag off every selected contact that has it; `affected` is how many had it.
pub async fn untag(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
    tag_id: Uuid,
) -> ApiResult<Counts> {
    run(db, sel, excluded, |q| {
        q.push("delete from contact_tags where workspace_id = ")
            .push_bind(sel.workspace_id())
            .push(" and tag_id = ")
            .push_bind(tag_id)
            .push(" and contact_id in (select id from sel)");
    })
    .await
}

/// Runs a membership-changing action and records `action` in the consent log for every contact it
/// changed — in the same statement, as a `logged` CTE over the rows the change returned. The log
/// names exactly the rows that moved, is in the caller's transaction by construction rather than
/// by convention, and a bulk action over `MAX_SELECTION` contacts sends no id back to Rust and
/// none out again.
///
/// **The `logged` insert binds no `ip` and no `user_agent` — and must never be made to.** An
/// request address and browser are not the subscriber's evidence (`Context::of_principal` makes
/// the same choice for the single-row writers), so both columns are left null. That is also why
/// [`consent::scrub`] has no work here: with no evidence bound, a failing row's DETAIL echoes no
/// personal data. Do not "fix" either half.
///
/// Which is why this takes a `source_ref` and not a whole [`consent::Context`]: a parameter that
/// accepts evidence it will silently drop is a rule held by this comment alone, and a comment is
/// not an enforcement. The type now says exactly what reaches the row.
async fn run_recorded(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
    list_id: Uuid,
    (action, source_ref): (consent::Action, Option<&str>),
    change: impl FnOnce(&mut QueryBuilder<Postgres>),
) -> ApiResult<Counts> {
    let mut q = statement_logging(sel, excluded, change, "contact_id", |q| {
        // `Source::Api` is the code path, not the actor: a session-authenticated operator using
        // bulk subscribe is logged `api` while the same operator adding one member is logged
        // `operator`. `consent_log.source` says how a change arrived, never who made it.
        q.push(
            ", logged as (insert into consent_log \
               (id, workspace_id, contact_id, list_id, action, source, source_ref) select ",
        )
        .push(SQL_UUID_V7)
        .push(", ")
        .push_bind(sel.workspace_id())
        .push(", contact_id, ")
        .push_bind(list_id)
        .push(", ")
        .push_bind(action)
        .push(", ")
        .push_bind(consent::Source::Api)
        .push(", ")
        .push_bind(source_ref.map(str::to_owned))
        .push(" from done)");
    });
    let (selected, affected): (i64, i64) = q.build_query_as().fetch_one(&mut *db).await?;
    Ok(Counts {
        selected: selected.unsigned_abs(),
        affected: affected.unsigned_abs(),
    })
}

/// Adds every selected contact who is not on the list yet, with `status` (`pending` on a
/// double-opt-in list). A membership that exists is never touched: `pending` is not promoted past
/// the confirmation the list requires, and `unsubscribed` is not reversed, because nobody has
/// asked to be mailed again. `affected` is how many rows were written, so a count below
/// `selected` tells the user something was skipped.
///
/// **Freezing the set is safe here only because this is `on conflict do nothing`** (section 8.3).
/// Somebody who unsubscribes between the selection and the action is still in the frozen set, so
/// this statement does run against them; doing nothing to the row they already have is the whole
/// reason that is harmless. `lists::add_member` is `do update … where status = 'unsubscribed'` —
/// it revives an unsubscribed membership deliberately, for one named person an admin answers for.
/// Anyone who later "fixes the asymmetry" by aligning this to that turns freezing into a bulk
/// re-subscription of people who opted out, with nothing in the tests or the types to object.
/// Section 15.3 carries the same question to the importer.
///
/// Each contact it adds gets a consent row, `Source::Api`, in the same transaction — `requested`
/// when `status` is `pending`, `subscribed` otherwise; a contact it skips gets none.
///
/// `source_ref` and not a [`consent::Context`], for the reason [`run_recorded`] gives: this writes
/// one row per contact in a selection of up to `MAX_SELECTION`, and no request's IP, user agent or
/// token nonce is evidence about all of them. A `Context` parameter here would accept all three
/// and drop them. A subscriber recording their own evidence does it one contact at a time, through
/// `lists::add_member` and `lists::unsubscribe_member`, which do take a whole `Context`.
pub async fn subscribe(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
    list_id: Uuid,
    status: MembershipStatus,
    source_ref: Option<&str>,
) -> ApiResult<Counts> {
    // One `status` for the whole statement, so one action for the whole statement — read off what
    // is about to be written, never off the list's `double_optin` flag, which is mutable.
    // Exhaustive on purpose: a status added later must be given an action here before this
    // compiles, rather than falling into a wildcard and being logged as something it is not.
    let action = match status {
        MembershipStatus::Pending => consent::Action::Requested,
        MembershipStatus::Subscribed => consent::Action::Subscribed,
        MembershipStatus::Unsubscribed => consent::Action::Unsubscribed,
    };
    run_recorded(db, sel, excluded, list_id, (action, source_ref), |q| {
        q.push("insert into list_memberships (workspace_id, contact_id, list_id, status, subscribed_at, source) \
                select ")
            .push_bind(sel.workspace_id())
            .push(", id, ")
            .push_bind(list_id)
            .push(", ")
            .push_bind(status)
            .push(", case when ")
            .push_bind(status)
            .push(" = 'subscribed' then now() end, ")
            .push_bind(MembershipSource::Bulk)
            .push(" from sel on conflict (contact_id, list_id) do nothing");
    })
    .await
}

/// Marks every selected member of the list `unsubscribed`; a contact not on the list, or already
/// unsubscribed, is not counted in `affected` and keeps its first `unsubscribed_at`. Each one it
/// changes gets an `unsubscribed` consent row, `Source::Api`, in the same transaction.
///
/// `source_ref` and not a [`consent::Context`], for the reason [`subscribe`] gives.
pub async fn unsubscribe(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
    list_id: Uuid,
    source_ref: Option<&str>,
) -> ApiResult<Counts> {
    let recorded = (consent::Action::Unsubscribed, source_ref);
    run_recorded(db, sel, excluded, list_id, recorded, |q| {
        q.push(
            "update list_memberships set status = 'unsubscribed', unsubscribed_at = now() \
             where status <> 'unsubscribed' and workspace_id = ",
        )
        .push_bind(sel.workspace_id())
        .push(" and list_id = ")
        .push_bind(list_id)
        .push(" and contact_id in (select id from sel)");
    })
    .await
}

/// Hard-deletes every selected contact, with its memberships and tags. Writes no suppression:
/// like `DELETE /contacts/{id}`, this is not erasure.
pub async fn delete(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
) -> ApiResult<Counts> {
    run(db, sel, excluded, |q| {
        q.push("delete from contacts where workspace_id = ")
            .push_bind(sel.workspace_id())
            .push(" and id in (select id from sel)");
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecretKey;

    fn secrets() -> Secrets {
        Secrets::new(&SecretKey([7u8; 32]))
    }

    fn token() -> Token {
        Token {
            workspace_id: Uuid::now_v7(),
            snapshot_id: Uuid::now_v7(),
            issued: 7,
        }
    }

    #[test]
    fn a_token_round_trips() {
        let t = token();
        assert_eq!(parse(&secrets(), &issue(&secrets(), &t)).unwrap(), t);
    }

    /// The **whole** statement a bulk action runs, which only its SQL can witness.
    ///
    /// Three properties, in one string. **`contacts` is the driving table**: the `sel` CTE opens
    /// `select id from contacts`, and the members table appears only inside `id in (…)`. Selecting
    /// the ids *out of* the members table and inserting them elsewhere would hit a foreign-key
    /// violation the moment one selected contact had been deleted, which kills the whole action
    /// rather than one row — and that substitution lives in this statement's opening, not in
    /// `push_selected`, which is why the assertion covers the opening too. The subquery **joins
    /// the parent on `expires_at > now()`**, so an unswept expired snapshot narrows to nothing;
    /// nothing reachable through a handler can witness that, because the handler refuses an
    /// expired snapshot first, and `tests/bulk.rs` drives it through this module instead. And the
    /// **exclusions** are applied to `contacts`, not to the members table.
    #[test]
    fn the_action_narrows_contacts_by_a_live_snapshot() {
        let sel = Selection::Snapshot {
            token: token(),
            filter: serde_json::json!({}),
        };
        let sql = statement(&sel, &[], |q| {
            q.push("delete from contacts where id in (select id from sel)");
        })
        .into_sql();
        assert_eq!(
            sql,
            "with sel as (select id from contacts where workspace_id = $1 and id in (select \
             m.contact_id from selection_snapshot_members m join selection_snapshots s on \
             s.workspace_id = m.workspace_id and s.id = m.snapshot_id where m.workspace_id = $2 \
             and m.snapshot_id = $3 and s.expires_at > now()) and id <> all($4)), done as (delete \
             from contacts where id in (select id from sel) returning 1) select (select count(*) \
             from sel)::bigint, (select count(*) from done)::bigint"
        );
    }

    /// The guard is unreachable by construction and kept as defence in depth, so the only thing
    /// left to pin is the arithmetic. It takes no exclusion count: M1a added one back, and under a
    /// frozen set that refused the most ordinary action on the grid — see `Selection::widened`.
    #[test]
    fn a_frozen_set_never_widens() {
        let sel = Selection::Snapshot {
            token: Token {
                issued: 3,
                ..token()
            },
            filter: Value::Null,
        };
        for resolved in 0..=3 {
            assert!(!sel.widened(resolved), "{resolved} of 3");
        }
        assert!(sel.widened(4), "the tripwire still works if the set grew");

        let ids = Selection::of_ids(Uuid::now_v7(), vec![Uuid::now_v7(), Uuid::now_v7()]);
        assert_eq!(ids.issued(), 2);
        assert!(!ids.widened(2));
    }

    #[test]
    fn a_refusal_names_the_cap_in_digits_a_person_reads() {
        assert_eq!(with_separators(MAX_SELECTION), "1,000,000");
        assert_eq!(with_separators(2), "2");
        assert_eq!(with_separators(1_000), "1,000");
    }

    /// Two properties of the materialising insert that only its SQL can witness.
    ///
    /// The **workspace clause**: `push_filter` and `push_rules` both emit fragments that assume
    /// the caller restricted `contacts.workspace_id`, and a filter with every field `None` adds
    /// nothing at all. Drop it and the insert materialises every tenant's contacts; the integration
    /// test for that catches it too, but this names the clause.
    ///
    /// The **`limit`**: the cap is enforced by taking one row past it and no more. Removing the
    /// limit leaves the refusal working — the extra rows still arrive and the transaction still
    /// rolls back — so nothing observable changes, and a 1M-row selection quietly writes the whole
    /// match before refusing. This is the only witness there is.
    #[test]
    fn the_materialising_insert_scopes_by_workspace_and_takes_one_row_past_the_cap() {
        let sql = members_insert(
            Uuid::now_v7(),
            Uuid::now_v7(),
            &ContactFilter::default(),
            None,
            7,
        )
        .into_sql();
        let sql = sql.as_str();
        assert!(
            sql.contains("from contacts where workspace_id = $3"),
            "{sql}"
        );
        assert!(sql.ends_with(" limit 8"), "{sql}");
    }

    #[test]
    fn a_token_from_another_key_is_invalid() {
        let other = Secrets::new(&SecretKey([8u8; 32]));
        assert!(matches!(
            parse(&other, &issue(&secrets(), &token())),
            Err(ApiError::Unprocessable("selection-invalid"))
        ));
    }

    #[test]
    fn garbage_is_invalid_not_a_panic() {
        for t in ["", ".", "a.b", "!!!.???", "e30.e30"] {
            assert!(matches!(
                parse(&secrets(), t),
                Err(ApiError::Unprocessable("selection-invalid"))
            ));
        }
    }

    fn signed(payload: &serde_json::Value) -> String {
        let payload = serde_json::to_vec(payload).unwrap();
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&payload),
            URL_SAFE_NO_PAD.encode(mac(&secrets(), &payload))
        )
    }

    /// The payload this struct writes round-trips, and nothing else does.
    ///
    /// **No field has `serde(default)`**, so each of the three is required. A payload missing
    /// `snapshot_id` is a token from the predicate era; read with a default it would be a
    /// selection narrowed by nothing, which on `bulk/delete` is the workspace. A real v2 token
    /// already fails the HMAC under the `v3` label; this covers the case where only the payload's
    /// shape differs, which is the half a label bump cannot catch.
    #[test]
    fn only_a_payload_this_build_writes_is_read() {
        let (ws, snapshot) = (Uuid::now_v7(), Uuid::now_v7());
        let full = serde_json::json!({
            "workspace_id": ws,
            "snapshot_id": snapshot,
            "issued": 7,
        });
        let t = parse(&secrets(), &signed(&full)).unwrap();
        assert_eq!(t.snapshot_id, snapshot);
        assert_eq!(t.issued, 7);

        // Each field removed in turn: refused, never defaulted.
        for field in ["workspace_id", "snapshot_id", "issued"] {
            let mut short = full.clone();
            short.as_object_mut().unwrap().remove(field);
            assert!(
                matches!(
                    parse(&secrets(), &signed(&short)),
                    Err(ApiError::Unprocessable("selection-invalid"))
                ),
                "a payload with no {field} was read"
            );
        }

        // The v2 payload, signed by this instance under *this* label: the shape alone refuses it,
        // so a rolling deploy that somehow kept the old key still cannot act on one.
        let v2 = serde_json::json!({
            "workspace_id": ws,
            "filter": {"q": null, "list_id": null, "list_status": null, "tag_id": null},
            "as_of": Uuid::now_v7(),
            "issued": 7,
        });
        assert!(matches!(
            parse(&secrets(), &signed(&v2)),
            Err(ApiError::Unprocessable("selection-invalid"))
        ));

        // An unknown key at the top level, which is this struct's own `deny_unknown_fields`.
        // Without it a payload from another version of this struct parses, and whatever that
        // version meant by its extra key is silently dropped.
        let mut extra = full.clone();
        extra
            .as_object_mut()
            .unwrap()
            .insert("ids".into(), serde_json::json!([Uuid::now_v7()]));
        assert!(matches!(
            parse(&secrets(), &signed(&extra)),
            Err(ApiError::Unprocessable("selection-invalid"))
        ));
    }
}
