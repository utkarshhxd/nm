// SPDX-License-Identifier: AGPL-3.0-only

//! A selection is a predicate and a moment, never a set of ids. "All 2.3 million contacts matching
//! this filter" cannot be sent as ids, and re-running the filter when the action executes is a
//! compliance bug: a signup or import landing in between would silently join the selection, and an
//! unsubscribe or delete would act on people nobody selected. So the token carries the filter and
//! `as_of`, a UUIDv7 taken when the token was issued. Contact ids are `Uuid::now_v7()`, so
//! `id <= as_of` is exactly "existed at that moment", with no timestamp column.
//!
//! Every bulk action here is one set-based statement. None loops over ids.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, Postgres, QueryBuilder};
use uuid::Uuid;

use super::{ContactFilter, push_filter};
use crate::{
    crypto::{secrets::Secrets, tokens},
    error::{ApiError, ApiResult, FieldError},
    lists::MembershipStatus,
};

/// Domain-separation label for the selection-token key: a token can never be replayed as any other
/// HMAC'd value, nor the reverse.
///
/// `v2` because `Selection` gained a required `issued`, and a `v1` token has none. Bumping the
/// label refuses every outstanding token as `selection-invalid` — the path the UI already handles
/// by asking the user to select again — rather than letting an uncounted token through the one
/// check that stops `bulk/delete` widening. At most one hour of tokens can be outstanding.
const SELECTION_DOMAIN: &str = "nm:selection:v2";

/// A token older than this is refused. An hour is long enough to read a confirmation dialog and
/// short enough that "everyone matching X, as of this morning" is not acted on tomorrow.
const TTL: Duration = Duration::from_secs(60 * 60);

/// The most ids a client may exclude from a selection: 10 000 is about 400 KB of JSON. Unbounded,
/// one request could carry hundreds of megabytes into a `<> all(...)`.
pub const MAX_EXCLUDED: usize = 10_000;

/// The most ids a client may name in an explicit id-list selection; the same bound, for the same
/// reason, as `MAX_EXCLUDED`.
pub const MAX_IDS: usize = 10_000;

/// What a signed token carries. The field names are part of the token format: renaming one
/// invalidates tokens already issued.
///
/// `deny_unknown_fields` so the payload shape is fail-closed on its own, and not only because
/// someone remembered to bump `SELECTION_DOMAIN`. A signed payload carrying a key this build does
/// not know is a payload from a different version of this struct, and the one thing a selection
/// must never do is resolve to more contacts than the version that issued it meant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub workspace_id: Uuid,
    pub filter: ContactFilter,
    /// UUIDv7 upper bound: the contacts selected are those with `id <= as_of`.
    ///
    /// The snapshot is only as precise as UUIDv7's millisecond: a contact another process creates
    /// in the same millisecond as the token may fall on either side. And it holds only while
    /// every contact id is a `Uuid::now_v7()`: any write path that assigns a different id (a v4
    /// from an import, say) silently escapes the bound, and so can be acted on by a selection
    /// that was made before it existed. Any new way of creating contacts must keep minting v7 ids.
    pub as_of: Uuid,
    /// An explicit list of contacts instead of `filter`: the ticked rows of a table. Never in an
    /// issued token (a token is a predicate); only `BulkTarget` builds one, from `ids`. It is
    /// still scoped by `workspace_id`, so an id from another workspace selects nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<Uuid>>,
    /// How many contacts this selection stood for when it was made: the live count for a filter,
    /// the number of ids for an id list.
    ///
    /// This is what the person was shown before they confirmed, carried inside the signature so
    /// the server can compare it with what the predicate resolves to when the action runs. The
    /// `as_of` bound freezes creation time only — `push_filter` reads list memberships, tags and
    /// names as they are now — so a contact that existed at `as_of` and did not match then does
    /// match now if someone has since tagged it or added it to the list. Without this number
    /// nothing notices.
    ///
    /// Required, not optional: an uncounted token is one the widening check cannot be run on.
    pub issued: u64,
}

impl Selection {
    /// A selection of exactly these contacts, as of now. `issued` is how many were named; the
    /// resolved count can only be lower (an id from another workspace matches nothing), so an id
    /// selection can never widen.
    pub fn of_ids(workspace_id: Uuid, ids: Vec<Uuid>) -> Selection {
        Selection {
            issued: ids.len() as u64,
            ids: Some(ids),
            ..Selection::now(workspace_id, ContactFilter::default())
        }
    }

    /// A selection of `filter` as of now. Within one process `now_v7` is monotonic, so every
    /// contact created after this call has a greater id. Two processes inside the same
    /// millisecond have no such order: a contact created by the other one in that millisecond may
    /// fall on either side.
    pub fn now(workspace_id: Uuid, filter: ContactFilter) -> Selection {
        Selection {
            workspace_id,
            filter,
            as_of: Uuid::now_v7(),
            ids: None,
            // The count is not known until the filter has been run; the caller that issues a
            // token sets it from `count` before signing. A selection that is never signed (the
            // tests' own, and `of_ids`, which overrides it) is never compared against it.
            issued: 0,
        }
    }

    /// True when the predicate resolved to more contacts than the selection was made for.
    ///
    /// Only a filter can widen, and only because `push_filter` reads current state: the people
    /// who joined the match were already there at `as_of`, and someone tagged them, subscribed
    /// them or renamed them into the search afterwards.
    ///
    /// `selected` and `issued` are **not the same quantity**, which is why `excluded` is a
    /// parameter and not the caller's business to forget. `issued` is the whole match at issue
    /// time, counted with no exclusions (`count` passes `&[]`); `selected` is the match at run
    /// time *less* the excluded ids. Comparing them directly lets a widening of up to
    /// `excluded.len()` through, and `MAX_EXCLUDED` is 10 000 — ten thousand contacts deleted
    /// under a confirmation that read "Delete 1 contact".
    ///
    /// Writing `M0` for the match at issue time and `M1` for the match now:
    /// `|M1| = selected + |M1 ∩ E| ≤ selected + |E|`, so refusing when `selected + |E| > |M0|`
    /// guarantees `|M1| ≤ |M0|` whenever it does not refuse. It over-refuses only when an
    /// excluded id was **never** in the match — an id that was in the match and has since left
    /// is counted on both sides and cancels out. The UI only ever excludes rows drawn from the
    /// matching view, so it cannot be refused falsely; an API client passing arbitrary ids can
    /// be, and over-refusing is the safe direction on the one action that cannot be undone.
    ///
    /// What it still does not catch is a same-count swap: one contact leaving the match while
    /// another joins leaves `|M1|` unchanged and this returns false. The number that can be
    /// substituted that way is bounded by how many contacts left the match inside the one-hour
    /// window, which is not a small number by construction. Catching it needs the set of ids the
    /// filter matched at issue time, which is exactly what a predicate selection exists not to
    /// carry — see `docs/adr/0003-selection-is-a-snapshotted-predicate.md`.
    pub fn widened(&self, selected: u64, excluded: usize) -> bool {
        selected.saturating_add(excluded as u64) > self.issued
    }
}

fn mac(secrets: &Secrets, payload: &[u8]) -> [u8; 32] {
    tokens::hmac_sha256(secrets.derive_key(SELECTION_DOMAIN).as_slice(), payload)
}

/// `base64url(json).base64url(hmac)`. Signed, not encrypted: it carries no secret, but a client
/// cannot alter the filter, the workspace or the bound.
pub fn issue(secrets: &Secrets, selection: &Selection) -> String {
    let payload = serde_json::to_vec(selection).expect("a selection always serialises");
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(mac(secrets, &payload)),
    )
}

/// `422 selection-invalid` for anything that is not a token this instance issued, and `422
/// selection-expired` once `as_of` is over an hour old. The caller compares `workspace_id` with
/// the workspace it authorised.
pub fn parse(secrets: &Secrets, token: &str) -> ApiResult<Selection> {
    let invalid = || ApiError::Unprocessable("selection-invalid");
    let (payload, sig) = token.split_once('.').ok_or_else(invalid)?;
    let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| invalid())?;
    let sig = URL_SAFE_NO_PAD.decode(sig).map_err(|_| invalid())?;
    if !tokens::constant_time_eq(&mac(secrets, &payload), &sig) {
        return Err(invalid());
    }
    let selection: Selection = serde_json::from_slice(&payload).map_err(|_| invalid())?;
    let issued = selection.as_of.get_timestamp().ok_or_else(invalid)?;
    let (secs, nanos) = issued.to_unix();
    let issued = UNIX_EPOCH + Duration::new(secs, nanos);
    // A clock that went backwards makes the age an error: treat it as zero rather than refusing.
    if SystemTime::now().duration_since(issued).unwrap_or_default() > TTL {
        return Err(ApiError::Unprocessable("selection-expired"));
    }
    Ok(selection)
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

/// How many contacts `sel` matches under its own `as_of` bound. A live count over the whole
/// match, so as slow as the match is big: the caller asks for it only when the user asks to select
/// everything.
pub async fn count(db: &mut PgConnection, sel: &Selection) -> ApiResult<u64> {
    let mut q = QueryBuilder::<Postgres>::new("select count(*)::bigint from contacts where ");
    push_selected(&mut q, sel, &[]);
    let n: i64 = q.build_query_scalar().fetch_one(db).await?;
    Ok(n.unsigned_abs())
}

/// Appends the selection's conditions on `contacts`, after `... where `.
fn push_selected(q: &mut QueryBuilder<Postgres>, sel: &Selection, excluded: &[Uuid]) {
    q.push("workspace_id = ").push_bind(sel.workspace_id);
    if let Some(ids) = &sel.ids {
        // Literal ids: no snapshot bound (the caller named these contacts) and no filter.
        q.push(" and id = any(").push_bind(ids.clone()).push(")");
        return;
    }
    // The snapshot: only what existed when the token was issued. Remove this and a signup
    // between the click and the action joins the selection.
    q.push(" and id <= ").push_bind(sel.as_of);
    q.push(" and id <> all(")
        .push_bind(excluded.to_vec())
        .push(")");
    push_filter(q, &sel.filter);
}

/// What a bulk statement did. Both numbers come out of the one statement, so they describe the same
/// rows: `selected` is what the predicate matched under the `as_of` bound (less the exclusions),
/// `affected` is how many of those the action actually changed. A count taken separately could
/// disagree with the action, which is worse than no count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub selected: u64,
    pub affected: u64,
}

/// `with sel as (<the selection>), done as (<action> returning 1) select count(sel), count(done)`.
/// `action` appends the data-changing statement, which reads `sel` for the contact ids.
async fn run(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
    action: impl FnOnce(&mut QueryBuilder<Postgres>),
) -> ApiResult<Counts> {
    let mut q = QueryBuilder::<Postgres>::new("with sel as (select id from contacts where ");
    push_selected(&mut q, sel, excluded);
    q.push("), done as (");
    action(&mut q);
    q.push(
        " returning 1) \
         select (select count(*) from sel)::bigint, (select count(*) from done)::bigint",
    );
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
            .push_bind(sel.workspace_id)
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
            .push_bind(sel.workspace_id)
            .push(" and tag_id = ")
            .push_bind(tag_id)
            .push(" and contact_id in (select id from sel)");
    })
    .await
}

/// Adds every selected contact who is not on the list yet, with `status` (`pending` on a
/// double-opt-in list). A membership that exists is never touched: `pending` is not promoted past
/// the confirmation the list requires, and `unsubscribed` is not reversed, because nobody has
/// asked to be mailed again. `affected` is how many rows were written, so a count below
/// `selected` tells the user something was skipped.
pub async fn subscribe(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
    list_id: Uuid,
    status: MembershipStatus,
) -> ApiResult<Counts> {
    run(db, sel, excluded, |q| {
        q.push("insert into list_memberships (workspace_id, contact_id, list_id, status, subscribed_at) \
                select ")
            .push_bind(sel.workspace_id)
            .push(", id, ")
            .push_bind(list_id)
            .push(", ")
            .push_bind(status)
            .push(", case when ")
            .push_bind(status)
            .push(" = 'subscribed' then now() end from sel on conflict (contact_id, list_id) do nothing");
    })
    .await
}

/// Marks every selected member of the list `unsubscribed`; a contact not on the list, or already
/// unsubscribed, is not counted in `affected` and keeps its first `unsubscribed_at`.
pub async fn unsubscribe(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
    list_id: Uuid,
) -> ApiResult<Counts> {
    run(db, sel, excluded, |q| {
        q.push(
            "update list_memberships set status = 'unsubscribed', unsubscribed_at = now() \
             where status <> 'unsubscribed' and workspace_id = ",
        )
        .push_bind(sel.workspace_id)
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
            .push_bind(sel.workspace_id)
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

    fn full_filter() -> ContactFilter {
        ContactFilter {
            q: Some("acme".into()),
            list_id: Some(Uuid::now_v7()),
            list_status: Some(MembershipStatus::Subscribed),
            tag_id: Some(Uuid::now_v7()),
        }
    }

    #[test]
    fn all_four_filter_fields_round_trip() {
        let s = Selection::now(Uuid::now_v7(), full_filter());
        assert_eq!(parse(&secrets(), &issue(&secrets(), &s)).unwrap(), s);
    }

    #[test]
    fn a_token_from_another_key_is_invalid() {
        let s = Selection::now(Uuid::now_v7(), ContactFilter::default());
        let other = Secrets::new(&SecretKey([8u8; 32]));
        assert!(matches!(
            parse(&other, &issue(&secrets(), &s)),
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
    /// `issued` has no `serde(default)` on purpose: a payload without it is a token from before
    /// the widening check existed, and it is refused rather than read as some default. That is
    /// the fail-closed half of the `v1` to `v2` domain bump — a real old token already fails the
    /// HMAC, and this covers the case where only the payload shape differs.
    #[test]
    fn only_a_payload_this_build_writes_is_read() {
        let (ws, list) = (Uuid::now_v7(), Uuid::now_v7());
        let full = serde_json::json!({
            "workspace_id": ws,
            "filter": {"q": null, "list_id": list, "list_status": "subscribed", "tag_id": null},
            "as_of": Uuid::now_v7(),
            "issued": 7,
        });
        let s = parse(&secrets(), &signed(&full)).unwrap();
        assert_eq!(s.filter.list_id, Some(list));
        assert_eq!(s.issued, 7);

        // The pre-`issued` shape, signed by this instance: still refused.
        let mut old = full.clone();
        old.as_object_mut().unwrap().remove("issued");
        assert!(matches!(
            parse(&secrets(), &signed(&old)),
            Err(ApiError::Unprocessable("selection-invalid"))
        ));

        // A signed payload with a field this build does not know is refused, not widened —
        // inside `filter`, which `ContactFilter` guards …
        let token = signed(&serde_json::json!({
            "workspace_id": ws,
            "filter": {"tag_id_": list},
            "as_of": Uuid::now_v7(),
            "issued": 0,
        }));
        assert!(matches!(
            parse(&secrets(), &token),
            Err(ApiError::Unprocessable("selection-invalid"))
        ));

        // … and at the top level, which is this struct's own `deny_unknown_fields`. Without it a
        // payload from another version of this struct parses, and whatever that version meant by
        // its extra key is silently dropped.
        let mut extra = full.clone();
        extra
            .as_object_mut()
            .unwrap()
            .insert("limit".into(), serde_json::json!(1));
        assert!(matches!(
            parse(&secrets(), &signed(&extra)),
            Err(ApiError::Unprocessable("selection-invalid"))
        ));
    }
}
