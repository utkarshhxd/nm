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

/// Puts the tag on every selected contact that lacks it. Returns how many were newly tagged.
pub async fn tag(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
    tag_id: Uuid,
) -> ApiResult<u64> {
    let mut q = QueryBuilder::<Postgres>::new(
        "insert into contact_tags (workspace_id, contact_id, tag_id) select workspace_id, id, ",
    );
    q.push_bind(tag_id).push(" from contacts where ");
    push_selected(&mut q, sel, excluded);
    q.push(" on conflict (contact_id, tag_id) do nothing");
    Ok(q.build().execute(db).await?.rows_affected())
}

/// Takes the tag off every selected contact that has it. Returns how many had it.
pub async fn untag(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
    tag_id: Uuid,
) -> ApiResult<u64> {
    let mut q = QueryBuilder::<Postgres>::new("delete from contact_tags where workspace_id = ");
    q.push_bind(sel.workspace_id)
        .push(" and tag_id = ")
        .push_bind(tag_id)
        .push(" and contact_id in (select id from contacts where ");
    push_selected(&mut q, sel, excluded);
    q.push(")");
    Ok(q.build().execute(db).await?.rows_affected())
}

/// Adds every selected contact to the list, as `add_member` does one: `status` is what a new
/// member gets (`pending` on a double-opt-in list), an existing `pending` or `subscribed` row is
/// left alone, and an `unsubscribed` one is let back in. Returns how many rows were written.
pub async fn subscribe(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
    list_id: Uuid,
    status: MembershipStatus,
) -> ApiResult<u64> {
    let mut q = QueryBuilder::<Postgres>::new(
        "insert into list_memberships (workspace_id, contact_id, list_id, status, subscribed_at) \
         select workspace_id, id, ",
    );
    q.push_bind(list_id)
        .push(", ")
        .push_bind(status)
        .push(", case when ")
        .push_bind(status)
        .push(" = 'subscribed' then now() end from contacts where ");
    push_selected(&mut q, sel, excluded);
    q.push(
        " on conflict (contact_id, list_id) do update \
         set status = excluded.status, subscribed_at = excluded.subscribed_at, \
         unsubscribed_at = null where list_memberships.status = 'unsubscribed'",
    );
    Ok(q.build().execute(db).await?.rows_affected())
}

/// Marks every selected member of the list `unsubscribed`; a contact not on the list, or already
/// unsubscribed, is not counted and keeps its first `unsubscribed_at`.
pub async fn unsubscribe(
    db: &mut PgConnection,
    sel: &Selection,
    excluded: &[Uuid],
    list_id: Uuid,
) -> ApiResult<u64> {
    let mut q = QueryBuilder::<Postgres>::new(
        "update list_memberships set status = 'unsubscribed', unsubscribed_at = now() \
         where status <> 'unsubscribed' and workspace_id = ",
    );
    q.push_bind(sel.workspace_id)
        .push(" and list_id = ")
        .push_bind(list_id)
        .push(" and contact_id in (select id from contacts where ");
    push_selected(&mut q, sel, excluded);
    q.push(")");
    Ok(q.build().execute(db).await?.rows_affected())
}

/// Hard-deletes every selected contact, with its memberships and tags. Writes no suppression:
/// like `DELETE /contacts/{id}`, this is not erasure.
pub async fn delete(db: &mut PgConnection, sel: &Selection, excluded: &[Uuid]) -> ApiResult<u64> {
    let mut q = QueryBuilder::<Postgres>::new("delete from contacts where ");
    push_selected(&mut q, sel, excluded);
    Ok(q.build().execute(db).await?.rows_affected())
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
