// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Json, Router, extract::State, http::StatusCode};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{
    Contact, ContactFilter, NewContact, create, get, list, validate_attributes_patch, validate_name,
};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    custom_fields::definitions_for,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    pagination::{Page, sorted::SortedPageParams},
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
        (status = 422, description = "a malformed cursor, an unknown `sort`, or a `q` under 3 characters"),
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
    let filter = ContactFilter::with_search(params.q.as_deref())?;
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
        (status = 409, description = "email-taken"),
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
    let id = create(&mut tx, workspace_id, input).await?;
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
