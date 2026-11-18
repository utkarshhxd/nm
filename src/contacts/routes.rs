// SPDX-License-Identifier: AGPL-3.0-only

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{
    Contact, ContactFilter, NewContact, create,
    export::{self, Audit},
    get, list,
    selection::{self, Counts, Selection},
    validate_attributes_patch, validate_name,
};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    consent,
    custom_fields::definitions_for,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    lists::{self, MembershipStatus},
    pagination::{Page, sorted::SortedPageParams},
    segments::load_rules,
    suppressions::{Block, blocking_suppression, erase_address},
    tags,
    workspaces::{roles::Role, validate_timezone},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts",
            axum::routing::get(list_contacts).post(create_contact),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/{id}",
            axum::routing::get(get_contact)
                .patch(update_contact)
                .delete(delete_contact),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/{id}/eligibility",
            axum::routing::get(contact_eligibility),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/{id}/erase",
            axum::routing::post(erase_contact),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/selection",
            axum::routing::post(create_selection),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/bulk/tag",
            axum::routing::post(bulk_tag_contacts),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/bulk/untag",
            axum::routing::post(bulk_untag_contacts),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/bulk/subscribe",
            axum::routing::post(bulk_subscribe_contacts),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/bulk/unsubscribe",
            axum::routing::post(bulk_unsubscribe_contacts),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/bulk/delete",
            axum::routing::post(bulk_delete_contacts),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/export",
            axum::routing::post(export_contacts),
        )
}

/// A JSON Merge Patch (RFC 7386) of a contact: a field left out is left alone, and an explicit
/// `null` removes it. The address cannot change, so `email` is an unknown field here and is
/// rejected, not ignored.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateContact {
    /// `null` clears it to an empty string.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub first_name: Option<Option<String>>,
    /// `null` clears it to an empty string.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub last_name: Option<Option<String>>,
    /// Merged into the stored attributes key by key: a key left out keeps its value. A key set to
    /// `null` is removed, also for an archived field; any other value for an archived field is
    /// refused. `attributes` itself cannot be `null`.
    #[serde(default, deserialize_with = "present")]
    pub attributes: Option<Value>,
    /// An IANA time zone such as `Europe/London`. `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub timezone: Option<Option<String>>,
}

/// With `#[serde(default)]`, tells a key that is absent (`None`) from one that is present, even as
/// `null` (`Some(..)`). A plain `Option<T>` maps both to `None`, so a `null` would be silently
/// ignored.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

/// One page of contacts. `?snapshot_id=` narrows it to a selection snapshot's frozen set, which is
/// how the grid shows what a selection actually holds.
///
/// A snapshot that has expired, been swept, been superseded or belongs to another workspace is
/// `422 selection-expired`, **not** an empty page: "zero contacts matched" is a different fact
/// from "this selection is gone", and the grid cannot tell someone to select again if the server
/// will not say which it was.
#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/contacts",
    operation_id = "list_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), SortedPageParams),
    responses(
        (status = 200, description = "Newest first unless `sort` says otherwise", body = Page<Contact>),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a malformed cursor, an unknown `sort`, a `q` under 3 characters, a `list_status` without a `list_id`, or selection-expired: the `snapshot_id` names no live selection of this workspace"),
    ),
)]
pub async fn list_contacts(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiQuery(params): ApiQuery<SortedPageParams>,
) -> ApiResult<Json<Page<Contact>>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    if params.list_status.is_some() && params.list_id.is_none() {
        return Err(ApiError::Validation(vec![FieldError::new(
            "list_status",
            "needs a list_id to narrow",
        )]));
    }
    let filter = ContactFilter {
        list_id: params.list_id,
        list_status: params.list_status,
        tag_id: params.tag_id,
        ..ContactFilter::with_search(params.q.as_deref())?
    };
    // Refused here rather than answered with an empty page, and the page is *also* narrowed by the
    // same `expires_at` join inside `list`: the check is the honest answer, the join is what makes
    // a missed check harmless.
    if let Some(snapshot_id) = params.snapshot_id {
        selection::require_live(&mut conn, workspace_id, snapshot_id).await?;
    }
    Ok(Json(list(&mut conn, workspace_id, &filter, &params).await?))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}",
    operation_id = "get_contact",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
    ),
    responses(
        (status = 200, body = Contact),
        (status = 404, description = "not a member, or no such contact in this workspace"),
    ),
)]
pub async fn get_contact(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<Contact>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    Ok(Json(get(&mut conn, workspace_id, id).await?))
}

/// Whether a contact may be mailed, and if a standing block stops it, which one.
#[derive(Serialize, ToSchema)]
pub struct Eligibility {
    /// `false` exactly when `blocked_by` is set.
    pub mailable: bool,
    /// The block that stops this address, whether it is this workspace's or the whole instance's.
    /// It says nothing about any other address. A list membership that is `pending` or
    /// `unsubscribed` is a separate reason a contact is not mailed on that list, and is not here.
    pub blocked_by: Option<Block>,
}

/// Computed on the server because the answer can depend on an instance-wide block that a
/// workspace's own suppression list does not contain and its admins cannot read. It discloses only
/// the verdict for this one contact.
#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}/eligibility",
    operation_id = "contact_eligibility",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
    ),
    responses(
        (status = 200, body = Eligibility),
        (status = 404, description = "not a member, or no such contact in this workspace"),
    ),
)]
pub async fn contact_eligibility(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<Eligibility>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    let contact = get(&mut conn, workspace_id, id).await?;
    let blocked_by =
        blocking_suppression(&mut *conn, &state.secrets, workspace_id, &contact.email).await?;
    Ok(Json(Eligibility {
        mailable: blocked_by.is_none(),
        blocked_by,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts",
    operation_id = "create_contact",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewContact,
    responses(
        (status = 201, body = Contact),
        (status = 403, description = "insufficient-role: creating a contact takes editor"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 409, description = "email-taken; or address-erased: this address was erased and can never be added again"),
        (status = 422, description = "invalid address, name, time zone or attribute"),
    ),
)]
pub async fn create_contact(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewContact>,
) -> ApiResult<(StatusCode, Json<Contact>)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let id = create(&mut tx, &state.secrets, workspace_id, input).await?;
    // The address is personal data an erasure must be able to remove, and the audit log is not
    // erased: record the id only.
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::CONTACT_CREATED,
            target_type: Some("contact"),
            target_id: Some(id.to_string()),
            metadata: json!({}),
        },
    )
    .await?;
    let contact = get(&mut tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(contact)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}",
    operation_id = "update_contact",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
    ),
    request_body = UpdateContact,
    responses(
        (status = 200, body = Contact),
        (status = 403, description = "insufficient-role: editing a contact takes editor"),
        (status = 404, description = "not a member, or no such contact in this workspace"),
        (status = 422, description = "invalid name, time zone or attribute, or an unknown field such as `email`"),
    ),
)]
pub async fn update_contact(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<UpdateContact>,
) -> ApiResult<Json<Contact>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;

    // `Some(None)` is an explicit `null`; `None` is a key that was not sent.
    let name = |field: &'static str, v: &Option<Option<String>>| {
        v.as_ref()
            .map(|n| validate_name(field, n.as_deref().unwrap_or_default()))
            .transpose()
    };
    let first = name("first_name", &input.first_name);
    let last = name("last_name", &input.last_name);
    let tz = match &input.timezone {
        Some(Some(t)) => validate_timezone(t).map(Some),
        _ => Ok(None),
    };
    let attrs = match &input.attributes {
        Some(a) => {
            validate_attributes_patch(&definitions_for(&mut *tx, workspace_id).await?, a).map(Some)
        }
        None => Ok(None),
    };
    let (Ok(first), Ok(last), Ok(tz), Ok(attrs)) = (&first, &last, &tz, &attrs) else {
        let mut errors: Vec<FieldError> = [first.err(), last.err(), tz.err()]
            .into_iter()
            .flatten()
            .collect();
        errors.extend(attrs.err().into_iter().flatten());
        return Err(ApiError::Validation(errors));
    };
    let (sets, removed) = match attrs {
        Some((sets, removed)) => (Some(sets), removed.as_slice()),
        None => (None, [].as_slice()),
    };

    // A name left out is kept; `null` clears it to "". `timezone` needs its own flag, because
    // `null` there means NULL and so cannot also mean "unchanged". The attributes first lose the
    // removed keys, then merge the rest with `||`, which touches only the keys it names.
    let updated = sqlx::query!(
        "update contacts set first_name = coalesce($3, first_name), \
         last_name = coalesce($4, last_name), \
         timezone = case when $5 then $6 else timezone end, \
         attributes = (attributes - $7::text[]) || coalesce($8, '{}'::jsonb), updated_at = now() \
         where workspace_id = $1 and id = $2",
        workspace_id,
        id,
        first.as_deref(),
        last.as_deref(),
        input.timezone.is_some(),
        tz.as_deref(),
        removed,
        sets,
    )
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    // Which fields changed or were cleared, never their values: a name or an attribute can be
    // personal data, and the audit log outlives an erasure.
    let mut fields: Vec<String> = [
        ("first_name", input.first_name.is_some()),
        ("last_name", input.last_name.is_some()),
        ("timezone", input.timezone.is_some()),
    ]
    .into_iter()
    .filter(|(_, sent)| *sent)
    .map(|(name, _)| name.to_owned())
    .collect();
    let touched = sets
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|m| m.keys())
        .chain(removed);
    fields.extend(touched.map(|k| format!("attributes.{k}")));
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::CONTACT_UPDATED,
            target_type: Some("contact"),
            target_id: Some(id.to_string()),
            metadata: json!({ "fields": fields }),
        },
    )
    .await?;
    let contact = get(&mut tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok(Json(contact))
}

/// A hard delete. It writes no suppression: the address may be added again. Erasing a person for
/// GDPR is a different, admin-only action.
#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}",
    operation_id = "delete_contact",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
    ),
    responses(
        (status = 204, description = "Deleted. Nothing is suppressed"),
        (status = 403, description = "insufficient-role: deleting a contact takes editor"),
        (status = 404, description = "not a member, or no such contact in this workspace"),
    ),
)]
pub async fn delete_contact(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let deleted = sqlx::query!(
        "delete from contacts where workspace_id = $1 and id = $2",
        workspace_id,
        id,
    )
    .execute(&mut *tx)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::CONTACT_DELETED,
            target_type: Some("contact"),
            target_id: Some(id.to_string()),
            metadata: json!({}),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// GDPR erasure: deletes the contact, its list memberships and its tags, and leaves a block that
/// keeps the address from being mailed or added again. Admin only, and not reversible: no route
/// lifts it. A mistake is fixed with `DELETE`, which blocks nothing.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}/erase",
    operation_id = "erase_contact",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
    ),
    responses(
        (status = 204, description = "Erased. The address can never be added again"),
        (status = 403, description = "insufficient-role: erasing a contact takes admin"),
        (status = 404, description = "not a member, or no such contact in this workspace"),
    ),
)]
pub async fn erase_contact(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;
    // The address is read here only to hash it; the tombstone takes the lock before the delete,
    // so a concurrent create of the same address either sees the tombstone or is deleted by us.
    let email = get(&mut tx, workspace_id, id).await?.email;
    erase_address(&mut tx, &state.secrets, workspace_id, &email).await?;
    let deleted = sqlx::query!(
        "delete from contacts where workspace_id = $1 and id = $2",
        workspace_id,
        id,
    )
    .execute(&mut *tx)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    // The audit log outlives the contact and admins read it: the id and the action, never the
    // address, or the erasure would not have erased anything.
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::CONTACT_ERASED,
            target_type: Some("contact"),
            target_id: Some(id.to_string()),
            metadata: json!({}),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A materialised selection: the snapshot that was written, and the token for it.
#[derive(Serialize, ToSchema)]
pub struct SelectionToken {
    /// Valid for one hour. Pass it to a `bulk/*` route, with `confirmed_count`.
    ///
    /// It names `snapshot_id` and carries no predicate, so it is issued for every selection,
    /// `segment_id` included: there is no longer any predicate in it that could describe a wider
    /// set than the one that was frozen.
    pub selection_token: String,
    /// The frozen set, by id: exactly the contacts that matched, written down. It expires in an
    /// hour, and a new selection by the same person in this workspace supersedes it.
    pub snapshot_id: Uuid,
    /// How many contacts were frozen. Exact for free — it is the materialising insert's own
    /// `rows_affected` — and deliberately exempt from the 150 ms p95 target, like
    /// `GET /lists/{id}/mailable-count`: materialising a million ids is measured at 4,887 ms. The
    /// UI shows a skeleton until it lands. It is never kept as a counter on a table:
    /// hand-maintained counters drift.
    pub selected: u64,
}

/// The body of `POST /contacts/selection`: which contacts to freeze.
///
/// The grid's filter fields, spelled out rather than nested, for the reason `BulkTarget` gives:
/// unknown fields are refused, and a misspelt `tag_id_` that was quietly ignored here would
/// freeze every contact in the workspace.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewSelection {
    /// Case-insensitive substring of the address or name; at least 3 characters.
    pub q: Option<String>,
    /// Contacts with a membership row on this list, whatever its status.
    pub list_id: Option<Uuid>,
    /// Narrows `list_id` to the memberships with this status; refused without a `list_id`.
    pub list_status: Option<MembershipStatus>,
    /// Contacts with this tag.
    pub tag_id: Option<Uuid>,
    /// A saved segment, whose stored rules narrow the selection further. An id that is not a
    /// segment of this workspace is a `404`, never read as "no rules".
    pub segment_id: Option<Uuid>,
}

impl NewSelection {
    /// The grid's filter for this body; `422` for a short `q` or a `list_status` with no list.
    fn filter(&self) -> ApiResult<ContactFilter> {
        if self.list_status.is_some() && self.list_id.is_none() {
            return Err(ApiError::Validation(vec![FieldError::new(
                "list_status",
                "needs a list_id to narrow",
            )]));
        }
        Ok(ContactFilter {
            list_id: self.list_id,
            list_status: self.list_status,
            tag_id: self.tag_id,
            ..ContactFilter::with_search(self.q.as_deref())?
        })
    }
}

/// What a bulk route acts on: exactly one of a filter selection or an explicit list of contacts.
///
/// The three bulk bodies spell their fields out and deny unknown ones, on purpose: `excluded_ids`
/// defaults to empty, so a misspelt `excluded_id` that was quietly ignored would run the action
/// on exactly the contacts the user meant to spare. `serde(flatten)` cannot be combined with
/// `deny_unknown_fields`, so do not factor these back into one flattened struct, and give any
/// new optional field here a reason to exist.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BulkTarget {
    /// From `POST /contacts/selection`: the contacts it froze, less `excluded_ids`. Send this or
    /// `ids`, not both and not neither.
    pub selection_token: Option<String>,
    /// These contacts and no others, e.g. the ticked rows of a table: at most 10 000. Ids that are
    /// not in this workspace select nothing. Cannot be combined with `excluded_ids`.
    pub ids: Option<Vec<Uuid>>,
    /// Contacts to leave out of a `selection_token`. At most 10 000.
    #[serde(default)]
    pub excluded_ids: Vec<Uuid>,
    /// How many contacts the confirmation showed: the size of the frozen set **less**
    /// `excluded_ids` — the number on the button, not the number the token was issued for.
    /// Required with `selection_token`, refused with `ids`.
    ///
    /// The action is refused `409` if it would reach **more** contacts than this. Reaching fewer
    /// is not refused: a contact deleted between the selection and the action is simply absent,
    /// and the gap between `selected` and `affected` reports it. Sending the pre-exclusion size
    /// instead would send a number the server already holds signed into the token, which can
    /// never be below what the action reaches and so can never refuse anything.
    pub confirmed_count: Option<u64>,
}

/// `BulkTarget` and a tag.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BulkTag {
    /// See `BulkTarget`.
    pub selection_token: Option<String>,
    /// See `BulkTarget`.
    pub ids: Option<Vec<Uuid>>,
    /// See `BulkTarget`.
    #[serde(default)]
    pub excluded_ids: Vec<Uuid>,
    /// See `BulkTarget`.
    pub confirmed_count: Option<u64>,
    pub tag_id: Uuid,
}

impl BulkTag {
    fn split(self) -> (BulkTarget, Uuid) {
        let target = BulkTarget {
            selection_token: self.selection_token,
            ids: self.ids,
            excluded_ids: self.excluded_ids,
            confirmed_count: self.confirmed_count,
        };
        (target, self.tag_id)
    }
}

/// `BulkTarget` and a list.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BulkList {
    /// See `BulkTarget`.
    pub selection_token: Option<String>,
    /// See `BulkTarget`.
    pub ids: Option<Vec<Uuid>>,
    /// See `BulkTarget`.
    #[serde(default)]
    pub excluded_ids: Vec<Uuid>,
    /// See `BulkTarget`.
    pub confirmed_count: Option<u64>,
    pub list_id: Uuid,
}

impl BulkList {
    fn split(self) -> (BulkTarget, Uuid) {
        let target = BulkTarget {
            selection_token: self.selection_token,
            ids: self.ids,
            excluded_ids: self.excluded_ids,
            confirmed_count: self.confirmed_count,
        };
        (target, self.list_id)
    }
}

/// What a bulk action did, as two numbers taken from the same statement. `affected` is how many
/// contacts it changed; `selected` is how much of the frozen set the action still found, less the
/// exclusions. `affected` is lower whenever some were already in the wanted state, so the UI can
/// say "N of M".
#[derive(Serialize, ToSchema)]
pub struct BulkResult {
    pub affected: u64,
    pub selected: u64,
    /// How many the selection stood for when it was made. `selected` below it means contacts in
    /// the frozen set were deleted between the selection and the action, or were excluded; it can
    /// never be above it, because the set was written down and nothing re-evaluates a predicate.
    pub issued: u64,
    /// Only for `subscribe`: the status the new memberships were written with. `pending` means
    /// the contacts are on a double-opt-in list and not mailable until they confirm, and NM has
    /// no confirmation flow yet: `affected` is not a count of people who can be mailed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<MembershipStatus>,
}

/// Freezes the contacts this predicate matches, by writing their ids down.
///
/// The set is decided here, once, and never re-derived: a predicate re-evaluated when the action
/// runs resolves to whatever the table says then, and a colleague's write inside the hour changes
/// what the action hits. A contact created afterwards is not in the set; a contact that no longer
/// matches still is, because the person confirmed *these contacts*, not *this query*; and a
/// contact deleted in between is simply absent.
///
/// Rate-limited per workspace, harder than preview, with `RateLimit` headers on the answer as well
/// as on the refusal: this is the most expensive call in the product, measured at 4,887 ms and
/// 158 MB for a million rows.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/selection",
    operation_id = "create_contact_selection",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewSelection,
    responses(
        (status = 200, body = SelectionToken),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 404, description = "not a member, no such workspace, or no such segment"),
        (status = 422, description = "a `q` under 3 characters, a `list_status` without a \
            `list_id`, a `segment_id` whose stored rules no longer resolve (the parser's own \
            code, such as unknown-field or invalid-value), or `selection-too-large`: the \
            predicate matches more than a million contacts"),
        (status = 429, description = "rate-limited: too many selections for this workspace"),
    ),
)]
pub async fn create_selection(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewSelection>,
) -> ApiResult<Response> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Editor).await?;
    // After `authorize`, so someone who is not in the workspace cannot spend its quota and deny
    // selections to the people who are.
    let quota = state.selection_limit.check(workspace_id);
    if let Some(retry_after_secs) = quota.over {
        return Ok((
            quota.headers,
            ApiError::TooManyRequests { retry_after_secs },
        )
            .into_response());
    }
    // Every answer from here carries the `RateLimit` headers, refusals included. The quota was
    // spent whatever the outcome, and `422 selection-too-large` is exactly the moment a client
    // needs to know how many attempts are left before it narrows the filter and tries again.
    Ok(
        match freeze(&state, &principal, workspace_id, input, conn).await {
            Ok(body) => (quota.headers, Json(body)).into_response(),
            Err(e) => (quota.headers, e).into_response(),
        },
    )
}

/// `create_selection` past the role check and the quota: validate, resolve, materialise, sign.
async fn freeze(
    state: &AppState,
    principal: &Principal,
    workspace_id: Uuid,
    input: NewSelection,
    mut conn: sqlx::pool::PoolConnection<Postgres>,
) -> ApiResult<SelectionToken> {
    let filter = input.filter()?;
    // A stored segment resolves with archived definitions **included**. Archiving a custom field
    // keeps every contact's value, so it must not break a saved segment that names the field;
    // preview and a save use the other resolver, because a *new* rule may not name an archived
    // field. Nothing builds a `Group` by hand — only `Group::parse` can refuse an empty group or
    // an unknown field, and a hand-built node can reach an `unreachable!` in the compiler.
    let rules = match input.segment_id {
        Some(id) => Some(load_rules(&mut conn, workspace_id, id).await?),
        None => None,
    };
    // The predicate as it was sent, kept for the audit row the bulk action writes: the token
    // carries none, so without this the log would hold a snapshot id that is deleted an hour
    // later. `q` is redacted by that copy, not here.
    let filter_doc = serde_json::to_value(&input).map_err(|e| ApiError::Internal(e.into()))?;
    // The pooled connection goes back before the long write takes its own.
    drop(conn);
    let snapshot = selection::materialise(
        state.pool.clone(),
        workspace_id,
        principal.actor(),
        filter_doc,
        filter,
        rules,
        selection::MAX_SELECTION,
    )
    .await?;
    // The token names the frozen set and nothing else, so it is issued whatever the predicate was
    // — a `segment_id` included. `issued` is the snapshot's own `size`: the number the person is
    // about to be shown, and what `confirmed_count` is checked against when they act on it.
    let token = selection::Token {
        workspace_id,
        snapshot_id: snapshot.id,
        issued: snapshot.size,
    };
    Ok(SelectionToken {
        selection_token: selection::issue(&state.secrets, &token),
        snapshot_id: snapshot.id,
        selected: snapshot.size,
    })
}

/// Opens the transaction, authorises, and resolves the token: in that order, so a non-member
/// learns nothing about whether a token was good. A token for another workspace is a `404`.
///
/// The snapshot row is read here, in the action's own transaction, for two things the token cannot
/// carry: the predicate the audit row records, and the `size` the confirmed count is checked
/// against. A token whose snapshot is gone — expired, swept, or superseded by a newer selection of
/// the same person's — is `422 selection-expired`.
async fn begin_bulk(
    state: &AppState,
    principal: &Principal,
    workspace_id: Uuid,
    target: &BulkTarget,
) -> ApiResult<(Transaction<'static, Postgres>, Selection)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, principal, workspace_id, Role::Editor).await?;
    selection::check_excluded(&target.excluded_ids)?;
    let selection = match (&target.selection_token, &target.ids) {
        (Some(token), None) => {
            let token = selection::parse(&state.secrets, token)?;
            // Before the row is read, so a token for another workspace is a plain `404` and
            // learns nothing about whether its snapshot is still live.
            if token.workspace_id != workspace_id {
                return Err(ApiError::NotFound);
            }
            // Issue `03`: the number the person confirmed must reach the server. Required, not
            // optional — the whole gap was that nothing compared it, and a client that may omit
            // it is a client that is not checked. It is compared in `refuse_if_more_than_confirmed`,
            // after the action has resolved, because the quantity it has to be compared against is
            // what the action actually reached.
            if target.confirmed_count.is_none() {
                return Err(ApiError::Validation(vec![FieldError::new(
                    "confirmed_count",
                    "send the number of contacts the confirmation showed",
                )]));
            }
            // `resolve` refuses unless the token's `issued` is this snapshot's stored `size`,
            // which is a different property worth having: it ties the signature to the row.
            selection::resolve(&mut tx, token).await?
        }
        (None, Some(ids)) => {
            selection::check_ids(ids)?;
            if !target.excluded_ids.is_empty() {
                return Err(ApiError::Validation(vec![FieldError::new(
                    "excluded_ids",
                    "only applies to a selection_token",
                )]));
            }
            // The ids *are* the count, and there is no snapshot to compare one against: a number
            // accepted here would be a number nothing checks.
            if target.confirmed_count.is_some() {
                return Err(ApiError::Validation(vec![FieldError::new(
                    "confirmed_count",
                    "only applies to a selection_token",
                )]));
            }
            Selection::of_ids(workspace_id, ids.clone())
        }
        _ => {
            return Err(ApiError::Validation(vec![FieldError::new(
                "selection_token",
                "send exactly one of selection_token and ids",
            )]));
        }
    };
    Ok((tx, selection))
}

/// Refuses, inside the transaction, when the action resolved to more contacts than the selection
/// was made for. The action has already run; returning an error here rolls it back.
///
/// **This cannot happen** now that the set is written down rather than re-derived (spec section
/// 8.6): nothing re-evaluates a predicate, so no write by anybody else can add a contact to a
/// frozen set, and `tests/bulk.rs` asserts the refusal never fires — including with exclusions
/// that are not in the snapshot, which is the case M1a's arithmetic refused. It is kept because it
/// costs one comparison and it is the last line if the snapshot join is ever bypassed.
///
/// Applied to `bulk/delete` and `bulk/subscribe`, and the axis is **not** reversible versus
/// irreversible. It is whether the action manufactures consent that cannot be obtained
/// retroactively. A delete cannot be undone at all. A subscribe puts people on a list, and being
/// on a list is what makes someone mailable — unsubscribing removes the row, it does not un-send,
/// and the send is the thing the person did not consent to.
fn refuse_if_widened(
    sel: &Selection,
    counts: Counts,
    (nothing, doing): (&str, &str),
) -> ApiResult<()> {
    if !sel.widened(counts.selected) {
        return Ok(());
    }
    Err(ApiError::ConflictDetail {
        code: "selection-widened",
        detail: format!(
            "This selection was made for {} contacts and resolved to {}, so {nothing}. Select \
             again to see what you would be {doing}.",
            sel.issued(),
            counts.selected,
        ),
    })
}

/// Refuses, inside the transaction, when the action reached **more** contacts than the dialog
/// said it would. The action has already run; returning an error here rolls it back.
///
/// This is issue `03`'s actual close (section 9.2). `confirmed_count` is the number on the button
/// — the selection's size *less the rows the person unticked* — and `selected` is what the frozen
/// set resolved to less the exclusions the request carried. Comparing the two is the only check
/// that can catch a confirmation and a request that disagree: comparing `confirmed_count` with
/// `selection_snapshots.size` compares it with the token's own signed `issued`, which the server
/// already knew, so it verifies nothing a replayed token with fewer exclusions could not satisfy.
///
/// **One-sided, like `refuse_if_widened`.** Fewer than confirmed is not refused: section 8.2 says
/// a contact deleted between the selection and the action is simply absent and the `affected` /
/// `selected` gap reports it, and refusing there would turn somebody else's delete into this
/// person's error. More than confirmed is refused, because that is the direction in which a
/// dialog that said "Delete 1 contact" deletes two.
///
/// It fires today on an ordinary grid action: the grid shows the live filter, so unticking a row
/// that is *not* in the frozen set lowers the dialog's number without lowering what the action
/// reaches. That is a true refusal, not the spurious `409` section 8.9 removed — the dialog really
/// did understate — and `?snapshot_id=` is what removes the case at source once the grid shows the
/// frozen set (section 14).
fn refuse_if_more_than_confirmed(target: &BulkTarget, counts: Counts) -> ApiResult<()> {
    let Some(confirmed) = target.confirmed_count else {
        // An explicit id list carries no confirmed count: the ids *are* the number, and the route
        // refuses one alongside them.
        return Ok(());
    };
    if counts.selected <= confirmed {
        return Ok(());
    }
    Err(ApiError::ConflictDetail {
        code: "selection-count-mismatch",
        detail: format!(
            "This would have acted on {} contacts and the confirmation said {confirmed}, so \
             nothing was done. Select again to see what you would be acting on.",
            counts.selected,
        ),
    })
}

/// The one audit row for a bulk action: what was asked (the predicate and its snapshot) and what
/// it did (the counts), never the contacts. Per-contact rows would outgrow the contacts table.
///
/// It is also where `refuse_if_more_than_confirmed` runs, so that every action goes through it:
/// this is the last thing inside the transaction, every bulk route ends here, and a sixth route
/// cannot be written that forgets the check and still commits.
async fn finish_bulk(
    mut tx: Transaction<'static, Postgres>,
    meta: &RequestMeta,
    principal: &Principal,
    action: &'static str,
    (selection, target): (&Selection, &BulkTarget),
    extra: Value,
    (counts, status): (Counts, Option<MembershipStatus>),
) -> ApiResult<Json<BulkResult>> {
    // Before the audit row, so a refused action leaves no log entry claiming it happened.
    refuse_if_more_than_confirmed(target, counts)?;
    // The predicate as the snapshot stored it (section 8.7). The token carries none and the
    // snapshot is deleted within the hour, so a log row holding only a snapshot id would read
    // "deleted 417,004 contacts, snapshot 0192…" against a row nobody can ever look up — worse
    // than M1a had, on the action that cannot be undone.
    //
    // An explicit id list is recorded as such and as a count, never as the ids: one row, not one
    // per contact, and ids are personal data the log would outlive.
    let (by, mut filter, snapshot_id, ids) = match selection {
        Selection::Snapshot { token, filter } => (
            "filter",
            filter.clone(),
            Some(token.snapshot_id),
            None::<usize>,
        ),
        Selection::Ids { ids, .. } => ("ids", Value::Null, None, Some(ids.len())),
    };
    // The search text can be an address or a name, and the audit log outlives an erasure: record
    // that a search was part of the selection, never what it said. Not a hash either: a hash of
    // an address falls to a dictionary. The snapshot keeps it unredacted while it lives, because
    // it is deleted within the hour and the person reading it is the one who typed it.
    redact_q(&mut filter);
    audit::record(
        &mut *tx,
        meta,
        AuditEvent {
            workspace_id: Some(selection.workspace_id()),
            actor: principal.actor(),
            action,
            target_type: None,
            target_id: None,
            metadata: json!({
                "by": by,
                "filter": filter,
                "snapshot_id": snapshot_id,
                "ids": ids,
                // A count, not the ids, by design (one row, not one per contact): replayed calls with
                // different exclusions cannot be told apart afterwards. Do not turn this into a list.
                "excluded": target.excluded_ids.len(),
                "issued": selection.issued(),
                // The number on the button, which `refuse_if_more_than_confirmed` compares and
                // then threw away — the one number issue `03` is named after, missing from the
                // record of the one action that cannot be undone. `null` for an id list, which
                // carries no confirmation because the ids are the number. With it, "you
                // confirmed 5,000 and we deleted 4,980" is answerable from the log while the
                // guard holds, which it is designed to do: fewer than confirmed is accepted
                // (section 8.2), so it is the gap the log has to be able to show.
                "confirmed": target.confirmed_count,
                "selected": counts.selected,
                "affected": counts.affected,
                "with": extra,
            }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(BulkResult {
        affected: counts.affected,
        selected: counts.selected,
        issued: selection.issued(),
        status,
    }))
}

/// Tags every contact in the selection.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/bulk/tag",
    operation_id = "bulk_tag_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = BulkTag,
    responses(
        (status = 200, description = "How many were newly tagged", body = BulkResult),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 409, description = "selection-count-mismatch: the action would have reached more contacts than confirmed_count; nothing was changed"),
        (status = 404, description = "not a member, no such tag, or a token for another workspace"),
        (status = 422, description = "selection-expired, selection-invalid, a missing \
            confirmed_count, a confirmed_count or excluded_ids sent alongside ids, an ids list \
            that is empty or over 10 000, over 10 000 excluded_ids, or not exactly one of \
            selection_token and ids"),
    ),
)]
pub async fn bulk_tag_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkTag>,
) -> ApiResult<Json<BulkResult>> {
    let (target, tag_id) = input.split();
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &target).await?;
    // Checked up front: with no contact to insert for, the foreign key would never object.
    tags::get(&mut tx, workspace_id, tag_id).await?;
    let n = selection::tag(&mut tx, &sel, &target.excluded_ids, tag_id).await?;
    let extra = json!({ "tag_id": tag_id });
    let action = actions::CONTACTS_BULK_TAGGED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &target),
        extra,
        (n, None),
    )
    .await
}

/// Takes a tag off every contact in the selection.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/bulk/untag",
    operation_id = "bulk_untag_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = BulkTag,
    responses(
        (status = 200, description = "How many had the tag", body = BulkResult),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 409, description = "selection-count-mismatch: the action would have reached more contacts than confirmed_count; nothing was changed"),
        (status = 404, description = "not a member, no such tag, or a token for another workspace"),
        (status = 422, description = "selection-expired, selection-invalid, a missing \
            confirmed_count, a confirmed_count or excluded_ids sent alongside ids, an ids list \
            that is empty or over 10 000, over 10 000 excluded_ids, or not exactly one of \
            selection_token and ids"),
    ),
)]
pub async fn bulk_untag_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkTag>,
) -> ApiResult<Json<BulkResult>> {
    let (target, tag_id) = input.split();
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &target).await?;
    tags::get(&mut tx, workspace_id, tag_id).await?;
    let n = selection::untag(&mut tx, &sel, &target.excluded_ids, tag_id).await?;
    let extra = json!({ "tag_id": tag_id });
    let action = actions::CONTACTS_BULK_UNTAGGED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &target),
        extra,
        (n, None),
    )
    .await
}

/// Adds every contact in the selection who is not on a list yet: `pending` on a double-opt-in
/// list, `subscribed` otherwise. An existing membership is left exactly as it is: `pending` is not
/// promoted, and `unsubscribed` is never flipped back. A suppression is not lifted either.
///
/// This deliberately differs from `POST /lists/{id}/members`, which lets an unsubscribed contact
/// back in. One admin re-adding one named person is a considered act they answer for; a bulk
/// action over thousands involves no per-person judgement, and reviving opt-outs en masse is what
/// gets a sender blocklisted. Adding someone to a list is what makes them mailable, and an
/// unsubscribe afterwards does not un-send. Do not "fix" the asymmetry.
// Deliberately not a doc comment: this is for whoever edits the route, not for a client reading
// the published document. The `409 selection-widened` this route declares cannot happen over a
// frozen set; the guard is kept, as `bulk/delete` keeps it, because it costs one comparison and
// is the last line if the snapshot join is ever bypassed.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/bulk/subscribe",
    operation_id = "bulk_subscribe_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = BulkList,
    responses(
        (status = 200, description = "How many memberships were added; contacts already on the list, whatever their status, are not counted", body = BulkResult),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 404, description = "not a member, no such list, or a token for another workspace"),
        (status = 409, description = "selection-count-mismatch: the action would have reached more contacts than confirmed_count; or selection-widened, which a frozen set cannot produce; nobody was added"),
        (status = 422, description = "selection-expired, selection-invalid, a missing \
            confirmed_count, a confirmed_count or excluded_ids sent alongside ids, an ids list \
            that is empty or over 10 000, over 10 000 excluded_ids, or not exactly one of \
            selection_token and ids"),
    ),
)]
pub async fn bulk_subscribe_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkList>,
) -> ApiResult<Json<BulkResult>> {
    let (target, list_id) = input.split();
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &target).await?;
    let list = lists::get(&mut tx, workspace_id, list_id).await?;
    let status = if list.double_optin {
        MembershipStatus::Pending
    } else {
        MembershipStatus::Subscribed
    };
    let excluded = &target.excluded_ids;
    let cx = consent::Context::of_principal(&principal);
    let n = selection::subscribe(&mut tx, &sel, excluded, list_id, status, &cx).await?;
    refuse_if_widened(&sel, n, ("nobody was added to the list", "adding"))?;
    let extra = json!({ "list_id": list_id });
    let action = actions::CONTACTS_BULK_SUBSCRIBED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &target),
        extra,
        (n, Some(status)),
    )
    .await
}

/// Unsubscribes every member of the selection from a list.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/bulk/unsubscribe",
    operation_id = "bulk_unsubscribe_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = BulkList,
    responses(
        (status = 200, description = "How many were unsubscribed now; those already unsubscribed or not on the list are not counted", body = BulkResult),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 409, description = "selection-count-mismatch: the action would have reached more contacts than confirmed_count; nothing was changed"),
        (status = 404, description = "not a member, no such list, or a token for another workspace"),
        (status = 422, description = "selection-expired, selection-invalid, a missing \
            confirmed_count, a confirmed_count or excluded_ids sent alongside ids, an ids list \
            that is empty or over 10 000, over 10 000 excluded_ids, or not exactly one of \
            selection_token and ids"),
    ),
)]
pub async fn bulk_unsubscribe_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkList>,
) -> ApiResult<Json<BulkResult>> {
    let (target, list_id) = input.split();
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &target).await?;
    lists::get(&mut tx, workspace_id, list_id).await?;
    let excluded = &target.excluded_ids;
    let cx = consent::Context::of_principal(&principal);
    let n = selection::unsubscribe(&mut tx, &sel, excluded, list_id, &cx).await?;
    let extra = json!({ "list_id": list_id });
    let action = actions::CONTACTS_BULK_UNSUBSCRIBED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &target),
        extra,
        (n, None),
    )
    .await
}

/// A hard delete of every contact in the selection. Like `DELETE /contacts/{id}` it writes no
/// suppression: the addresses may be added again. It is not erasure.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/bulk/delete",
    operation_id = "bulk_delete_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = BulkTarget,
    responses(
        (status = 200, description = "How many were deleted", body = BulkResult),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 404, description = "not a member, or a token for another workspace"),
        (status = 409, description = "selection-count-mismatch: the action would have reached more contacts than confirmed_count; or selection-widened, which a frozen set cannot produce; nothing was deleted"),
        (status = 422, description = "selection-expired, selection-invalid, a missing \
            confirmed_count, a confirmed_count or excluded_ids sent alongside ids, an ids list \
            that is empty or over 10 000, over 10 000 excluded_ids, or not exactly one of \
            selection_token and ids"),
    ),
)]
pub async fn bulk_delete_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkTarget>,
) -> ApiResult<Json<BulkResult>> {
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &input).await?;
    let n = selection::delete(&mut tx, &sel, &input.excluded_ids).await?;
    // Inside the transaction and after the statement, so the number compared is the one the
    // delete itself resolved rather than a second count that could disagree with it. Returning
    // here drops `tx` and the delete rolls back.
    refuse_if_widened(&sel, n, ("nothing was deleted", "deleting"))?;
    let action = actions::CONTACTS_BULK_DELETED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &input),
        json!({}),
        (n, None),
    )
    .await
}

/// The search text can be an address or a name, and the audit log outlives an erasure: record
/// that a search was part of the filter, never what it said. The filter is the flat document
/// `serde_json::to_value(NewSelection)` gives, so `q` is a top-level key.
fn redact_q(filter: &mut Value) {
    if !filter["q"].is_null() {
        filter["q"] = json!("<redacted>");
    }
}

/// Streams every contact the filter matches as CSV: the grid's filter, `segment_id` included.
///
/// Rows are streamed as they are read and never buffered, formula cells are prefixed with `'`,
/// and the audit row records the actor, the filter with `q` redacted, and the row count. Admin
/// only. That does not stop exfiltration, since an editor can already page the book out through
/// `GET /contacts` at up to 200 rows a time; it raises the cost and makes one place where there
/// was none.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/export",
    operation_id = "export_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewSelection,
    responses(
        (status = 200, description = "text/csv, streamed", content_type = "text/csv", body = String),
        (status = 403, description = "insufficient-role: export takes admin"),
        (status = 404, description = "not a member, no such workspace, or no such segment"),
        (status = 422, description = "a `q` under 3 characters, a `list_status` without a `list_id`, \
            or a `segment_id` whose stored rules no longer resolve (the parser's own code)"),
        (status = 429, description = "rate-limited: too many exports for this workspace"),
        (status = 503, description = "export-capacity: this instance is already streaming as many \
            exports as it may at once, whatever workspace they belong to; try again (Retry-After)"),
    ),
)]
pub async fn export_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewSelection>,
) -> ApiResult<Response> {
    // Short-lived, and given straight back: the connection an export holds for the whole of its
    // stream is acquired below, **after** the permit. Authorising on the streaming connection
    // instead would mean twenty concurrent requests took twenty pooled connections before a
    // single permit was asked for, and the semaphore would bound nothing. The cost is one extra
    // pool round trip per export, against a full CSV stream.
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Admin).await?;
    drop(conn);
    // After `authorize`, so a stranger cannot spend the workspace's quota.
    let quota = state.export_limit.check(workspace_id);
    if let Some(retry_after_secs) = quota.over {
        return Ok((
            quota.headers,
            ApiError::TooManyRequests { retry_after_secs },
        )
            .into_response());
    }
    // Also after `authorize`, so a stranger cannot sit on one of the instance's few export slots.
    // `try_acquire` and not `acquire`: queueing here would only move the pool's hang into the
    // semaphore, and the whole point is to answer now.
    //
    // **503, not 429.** The client refused here may have sent exactly one export and be well
    // inside its own quota — "too many requests" would be a label asserting something untrue
    // about it. What is true is that the instance has no export capacity at this moment, which is
    // what 503 says, and `Retry-After` says to come back shortly rather than to give up. The
    // quota headers go on it like every other answer from this handler: they report that
    // workspace's remaining allowance, which is true and is what the client needs next.
    let Ok(permit) = state.export_slots.clone().try_acquire_owned() else {
        return Ok((
            quota.headers,
            [("retry-after", "5")],
            ApiError::ServiceUnavailable("export-capacity"),
        )
            .into_response());
    };
    // The streaming connection, held until the client has the last chunk.
    let conn = state.pool.acquire().await?;
    // Every answer from here carries the `RateLimit` headers, refusals included, as
    // `create_selection`'s and `preview_segment`'s do. The quota was spent whatever the outcome,
    // and this is the smallest of the three at five a minute: a client refused `422` for a short
    // `q` or `404` for a segment that is not its own is exactly the one that needs to know how
    // many of those five are left before it fixes the filter and tries again.
    Ok(
        match stream_export(&state, meta, &principal, workspace_id, input, conn, permit).await {
            Ok(body) => (
                quota.headers,
                [
                    ("content-type", "text/csv; charset=utf-8"),
                    (
                        "content-disposition",
                        "attachment; filename=\"contacts.csv\"",
                    ),
                    ("cache-control", "no-store"),
                ],
                body,
            )
                .into_response(),
            Err(e) => (quota.headers, e).into_response(),
        },
    )
}

/// `export_contacts` past the role check and the quota: validate, resolve the segment, start the
/// stream. Every refusal in here leaves through `?` and so through the caller's `Err` arm, which
/// is what puts the headers on it.
async fn stream_export(
    state: &AppState,
    meta: RequestMeta,
    principal: &Principal,
    workspace_id: Uuid,
    input: NewSelection,
    mut conn: sqlx::pool::PoolConnection<Postgres>,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> ApiResult<Body> {
    let filter = input.filter()?;
    // `404` for a segment that is not this workspace's, decided in `load_rules`: read as "no
    // rules" it would export the whole workspace. Nothing here builds a `Group` by hand.
    let rules = match input.segment_id {
        Some(id) => Some(load_rules(&mut conn, workspace_id, id).await?),
        None => None,
    };
    let live = definitions_for(&mut *conn, workspace_id)
        .await?
        .into_iter()
        .filter(|d| d.archived_at.is_none())
        .collect();
    let mut doc = serde_json::to_value(&input).map_err(|e| ApiError::Internal(e.into()))?;
    redact_q(&mut doc);
    let audit = Audit {
        meta,
        actor: principal.actor(),
        filter: doc,
    };
    let job = export::Export {
        workspace_id,
        filter,
        rules,
        live,
    };
    // Every `?` above leaves with the permit still a local here, so a refusal releases the slot
    // on the spot; from here it belongs to the producer task.
    Ok(export::start(
        state.pool.clone(),
        conn,
        job,
        audit,
        export::SEND_TIMEOUT,
        permit,
    ))
}
