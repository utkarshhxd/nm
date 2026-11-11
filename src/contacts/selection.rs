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
const SELECTION_DOMAIN: &str = "nm:selection:v1";

/// A token older than this is refused. An hour is long enough to read a confirmation dialog and
/// short enough that "everyone matching X, as of this morning" is not acted on tomorrow.
const TTL: Duration = Duration::from_secs(60 * 60);

/// The most ids a client may exclude from a selection: 10 000 is about 400 KB of JSON. Unbounded,
/// one request could carry hundreds of megabytes into a `<> all(...)`.
pub const MAX_EXCLUDED: usize = 10_000;

/// What a signed token carries. The field names are part of the token format: renaming one
/// invalidates tokens already issued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl Selection {
    /// A selection of `filter` as of now. Within one process `now_v7` is monotonic, so every
    /// contact created after this call has a greater id. Two processes inside the same
    /// millisecond have no such order: a contact created by the other one in that millisecond may
    /// fall on either side.
    pub fn now(workspace_id: Uuid, filter: ContactFilter) -> Selection {
        Selection {
            workspace_id,
            filter,
            as_of: Uuid::now_v7(),
        }
    }
}

fn mac(secrets: &Secrets, payload: &[u8]) -> [u8; 32] {
    tokens::hmac_sha256(&secrets.derive_key(SELECTION_DOMAIN), payload)
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

/// Appends the selection's conditions on `contacts`, after `... where `.
fn push_selected(q: &mut QueryBuilder<Postgres>, sel: &Selection, excluded: &[Uuid]) {
    q.push("workspace_id = ").push_bind(sel.workspace_id);
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
}
