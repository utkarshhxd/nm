// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Json, Router, extract::State, http::StatusCode};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{
    Contact, ContactFilter, NewContact, create, get, list,
    selection::{self, Selection},
    validate_attributes_patch, validate_name,
};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    custom_fields::definitions_for,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    lists::{self, MembershipStatus},
    pagination::{Page, sorted::SortedPageParams},
    suppressions::erase_address,
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

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/contacts",
    operation_id = "list_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), SortedPageParams),
    responses(
        (status = 200, description = "Newest first unless `sort` says otherwise", body = Page<Contact>),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a malformed cursor, an unknown `sort`, a `q` under 3 characters, or a `list_status` without a `list_id`"),
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

/// The signed token for a selection; see `selection`.
#[derive(Serialize, ToSchema)]
pub struct SelectionToken {
    /// Valid for one hour. Pass it to a `bulk/*` route.
    pub selection_token: String,
}

/// What a bulk route acts on: the selection, less the contacts the user ticked off.
#[derive(Deserialize, ToSchema)]
pub struct BulkTarget {
    pub selection_token: String,
    /// Contacts to leave out. At most 10 000.
    #[serde(default)]
    pub excluded_ids: Vec<Uuid>,
}

#[derive(Deserialize, ToSchema)]
pub struct BulkTag {
    #[serde(flatten)]
    pub target: BulkTarget,
    pub tag_id: Uuid,
}

#[derive(Deserialize, ToSchema)]
pub struct BulkList {
    #[serde(flatten)]
    pub target: BulkTarget,
    pub list_id: Uuid,
}

/// How many contacts a bulk action changed. This can be fewer than the selection held: contacts
/// already in the wanted state, excluded, or deleted since are not counted.
#[derive(Serialize, ToSchema)]
pub struct BulkResult {
    pub affected: u64,
}

/// Freezes "the contacts this filter matches" at this moment. The token holds the filter and a
/// time bound, not the contacts: contacts created afterwards are never part of it.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/selection",
    operation_id = "create_contact_selection",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = ContactFilter,
    responses(
        (status = 200, body = SelectionToken),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a `q` under 3 characters, or a `list_status` without a `list_id`"),
    ),
)]
pub async fn create_selection(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(filter): ApiJson<ContactFilter>,
) -> ApiResult<Json<SelectionToken>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Editor).await?;
    if filter.list_status.is_some() && filter.list_id.is_none() {
        return Err(ApiError::Validation(vec![FieldError::new(
            "list_status",
            "needs a list_id to narrow",
        )]));
    }
    let filter = ContactFilter {
        list_id: filter.list_id,
        list_status: filter.list_status,
        tag_id: filter.tag_id,
        ..ContactFilter::with_search(filter.q.as_deref())?
    };
    let selection = Selection::now(workspace_id, filter);
    Ok(Json(SelectionToken {
        selection_token: selection::issue(&state.secrets, &selection),
    }))
}

/// Opens the transaction, authorises, and resolves the token: in that order, so a non-member
/// learns nothing about whether a token was good. A token for another workspace is a `404`.
async fn begin_bulk(
    state: &AppState,
    principal: &Principal,
    workspace_id: Uuid,
    target: &BulkTarget,
) -> ApiResult<(Transaction<'static, Postgres>, Selection)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, principal, workspace_id, Role::Editor).await?;
    selection::check_excluded(&target.excluded_ids)?;
    let selection = selection::parse(&state.secrets, &target.selection_token)?;
    if selection.workspace_id != workspace_id {
        return Err(ApiError::NotFound);
    }
    Ok((tx, selection))
}

/// The one audit row for a bulk action: what was asked (the filter and its time bound) and what
/// it did (the count), never the contacts. Per-contact rows would outgrow the contacts table.
async fn finish_bulk(
    mut tx: Transaction<'static, Postgres>,
    meta: &RequestMeta,
    principal: &Principal,
    action: &'static str,
    (selection, target): (&Selection, &BulkTarget),
    extra: Value,
    affected: u64,
) -> ApiResult<Json<BulkResult>> {
    audit::record(
        &mut *tx,
        meta,
        AuditEvent {
            workspace_id: Some(selection.workspace_id),
            actor: principal.actor(),
            action,
            target_type: None,
            target_id: None,
            metadata: json!({
                "filter": selection.filter,
                "as_of": selection.as_of,
                "excluded": target.excluded_ids.len(),
                "affected": affected,
                "with": extra,
            }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(BulkResult { affected }))
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
        (status = 404, description = "not a member, no such tag, or a token for another workspace"),
        (status = 422, description = "selection-expired, selection-invalid, or too many excluded_ids"),
    ),
)]
pub async fn bulk_tag_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkTag>,
) -> ApiResult<Json<BulkResult>> {
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &input.target).await?;
    // Checked up front: with no contact to insert for, the foreign key would never object.
    tags::get(&mut tx, workspace_id, input.tag_id).await?;
    let n = selection::tag(&mut tx, &sel, &input.target.excluded_ids, input.tag_id).await?;
    let extra = json!({ "tag_id": input.tag_id });
    let action = actions::CONTACTS_BULK_TAGGED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &input.target),
        extra,
        n,
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
        (status = 404, description = "not a member, no such tag, or a token for another workspace"),
        (status = 422, description = "selection-expired, selection-invalid, or too many excluded_ids"),
    ),
)]
pub async fn bulk_untag_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkTag>,
) -> ApiResult<Json<BulkResult>> {
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &input.target).await?;
    tags::get(&mut tx, workspace_id, input.tag_id).await?;
    let n = selection::untag(&mut tx, &sel, &input.target.excluded_ids, input.tag_id).await?;
    let extra = json!({ "tag_id": input.tag_id });
    let action = actions::CONTACTS_BULK_UNTAGGED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &input.target),
        extra,
        n,
    )
    .await
}

/// Puts every contact in the selection on a list: `pending` on a double-opt-in list, `subscribed`
/// otherwise. Someone who unsubscribed is let back in, as `POST /contacts/{id}/lists` does.
/// A suppression is not lifted: a suppressed address stays unmailable.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/bulk/subscribe",
    operation_id = "bulk_subscribe_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = BulkList,
    responses(
        (status = 200, description = "How many memberships were written", body = BulkResult),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 404, description = "not a member, no such list, or a token for another workspace"),
        (status = 422, description = "selection-expired, selection-invalid, or too many excluded_ids"),
    ),
)]
pub async fn bulk_subscribe_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkList>,
) -> ApiResult<Json<BulkResult>> {
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &input.target).await?;
    let list = lists::get(&mut tx, workspace_id, input.list_id).await?;
    let status = if list.double_optin {
        MembershipStatus::Pending
    } else {
        MembershipStatus::Subscribed
    };
    let excluded = &input.target.excluded_ids;
    let n = selection::subscribe(&mut tx, &sel, excluded, input.list_id, status).await?;
    let extra = json!({ "list_id": input.list_id });
    let action = actions::CONTACTS_BULK_SUBSCRIBED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &input.target),
        extra,
        n,
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
        (status = 404, description = "not a member, no such list, or a token for another workspace"),
        (status = 422, description = "selection-expired, selection-invalid, or too many excluded_ids"),
    ),
)]
pub async fn bulk_unsubscribe_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkList>,
) -> ApiResult<Json<BulkResult>> {
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &input.target).await?;
    lists::get(&mut tx, workspace_id, input.list_id).await?;
    let excluded = &input.target.excluded_ids;
    let n = selection::unsubscribe(&mut tx, &sel, excluded, input.list_id).await?;
    let extra = json!({ "list_id": input.list_id });
    let action = actions::CONTACTS_BULK_UNSUBSCRIBED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &input.target),
        extra,
        n,
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
        (status = 422, description = "selection-expired, selection-invalid, or too many excluded_ids"),
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
    let action = actions::CONTACTS_BULK_DELETED;
    finish_bulk(tx, &meta, &principal, action, (&sel, &input), json!({}), n).await
}
