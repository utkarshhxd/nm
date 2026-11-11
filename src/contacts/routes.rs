// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde::Deserialize;
use serde_json::{Value, json};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{Contact, NewContact, create, get, validate_attributes, validate_name};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    custom_fields::definitions_for,
    error::{ApiError, ApiJson, ApiPath, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    workspaces::{roles::Role, validate_timezone},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts",
            post(create_contact),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/{id}",
            axum::routing::get(get_contact)
                .patch(update_contact)
                .delete(delete_contact),
        )
}

/// A partial update: a field left out is left alone. The address cannot change, so `email` is an
/// unknown field here and is rejected, not ignored.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateContact {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    /// Merged into the stored attributes key by key: a key left out keeps its value. A value
    /// cannot be set to `null`; purging the field is how values are removed.
    pub attributes: Option<Value>,
    /// An IANA time zone such as `Europe/London`.
    pub timezone: Option<String>,
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

    let first = input
        .first_name
        .as_deref()
        .map(|n| validate_name("first_name", n))
        .transpose();
    let last = input
        .last_name
        .as_deref()
        .map(|n| validate_name("last_name", n))
        .transpose();
    let tz = input.timezone.as_deref().map(validate_timezone).transpose();
    let attrs = match &input.attributes {
        Some(a) => {
            validate_attributes(&definitions_for(&mut *tx, workspace_id).await?, a).map(Some)
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

    // `||` on jsonb merges the top-level keys: a partial update erases nothing it did not name.
    let updated = sqlx::query!(
        "update contacts set first_name = coalesce($3, first_name), \
         last_name = coalesce($4, last_name), timezone = coalesce($5, timezone), \
         attributes = attributes || coalesce($6, '{}'::jsonb), updated_at = now() \
         where workspace_id = $1 and id = $2",
        workspace_id,
        id,
        first.as_ref(),
        last.as_ref(),
        tz.as_deref(),
        attrs.as_ref(),
    )
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    // Which fields changed, never their values: a name or an attribute can be personal data.
    let mut fields: Vec<String> = [
        ("first_name", first.is_some()),
        ("last_name", last.is_some()),
        ("timezone", tz.is_some()),
    ]
    .into_iter()
    .filter(|(_, sent)| *sent)
    .map(|(name, _)| name.to_owned())
    .collect();
    if let Some(Value::Object(a)) = attrs {
        fields.extend(a.keys().map(|k| format!("attributes.{k}")));
    }
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
