// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Json, Router, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{
    ContactList, List, ListMember, MembershipSource, MembershipStatus, Visibility, add_member, get,
    unsubscribe_member, validate_description, validate_name,
};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    contacts,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    pagination::{Page, PageParams},
    workspaces::roles::Role,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/workspaces/{workspace_id}/lists",
            axum::routing::get(list_lists).post(create_list),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/lists/{id}",
            axum::routing::get(get_list)
                .patch(update_list)
                .delete(delete_list),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/lists/{id}/mailable-count",
            axum::routing::get(mailable_count),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/lists/{id}/members",
            axum::routing::get(list_list_members).post(add_list_member),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/lists/{id}/members/{contact_id}/unsubscribe",
            axum::routing::post(unsubscribe_list_member),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/{id}/lists",
            axum::routing::get(list_contact_lists).post(add_contact_to_list),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/{id}/lists/{list_id}/unsubscribe",
            axum::routing::post(unsubscribe_contact_from_list),
        )
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewList {
    /// Unique in the workspace.
    pub name: String,
    pub description: Option<String>,
    /// Default `true`.
    pub double_optin: Option<bool>,
    /// Default `private`.
    pub visibility: Option<Visibility>,
}

/// A field left out is left alone. `null` clears `description` to an empty string; on any other
/// field it is `422`, because there is nothing to clear it to and ignoring it would look like
/// success.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateList {
    /// Cannot be `null`.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = String, required = false)]
    pub name: Option<Option<String>>,
    /// `null` clears it to an empty string.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    /// Only decides the status of members added from now on. A member already `pending` stays
    /// `pending` (not mailable) when this is switched off. Cannot be `null`.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = bool, required = false)]
    pub double_optin: Option<Option<bool>>,
    /// Cannot be `null`.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Visibility, required = false)]
    pub visibility: Option<Option<Visibility>>,
}

fn null<T>(field: &'static str) -> Result<T, FieldError> {
    Err(FieldError::new(field, "cannot be null"))
}

/// Tells a key that is absent (`None`) from one present as `null` (`Some(None)`). A plain
/// `Option<T>` maps both to `None`, which would silently ignore a `"name": null`.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewMember {
    pub contact_id: Uuid,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewContactList {
    pub list_id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub struct MailableCount {
    pub count: u64,
}

#[derive(Serialize, ToSchema)]
pub struct Membership {
    pub contact_id: Uuid,
    pub list_id: Uuid,
    pub status: MembershipStatus,
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/lists",
    operation_id = "list_lists",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), PageParams),
    responses(
        (status = 200, description = "Newest first", body = Page<List>),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_lists(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<List>>> {
    authorize(&state.pool, &principal, workspace_id, Role::Viewer).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        List,
        r#"select id, name, description, double_optin, visibility as "visibility: Visibility",
                  created_at, updated_at
           from lists
           where workspace_id = $1 and ($2::uuid is null or id < $2)
           order by id desc limit $3"#,
        workspace_id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |l| l.id)))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/lists/{id}",
    operation_id = "get_list",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id"),
    ),
    responses(
        (status = 200, body = List),
        (status = 404, description = "not a member, or no such list in this workspace"),
    ),
)]
pub async fn get_list(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<List>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    Ok(Json(get(&mut conn, workspace_id, id).await?))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/lists",
    operation_id = "create_list",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewList,
    responses(
        (status = 201, body = List),
        (status = 403, description = "insufficient-role: creating a list takes editor"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 409, description = "list-name-taken"),
        (status = 422, description = "invalid name or description"),
    ),
)]
pub async fn create_list(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewList>,
) -> ApiResult<(StatusCode, Json<List>)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let name = validate_name(&input.name);
    let description = validate_description(input.description.as_deref().unwrap_or(""));
    let (Ok(name), Ok(description)) = (&name, &description) else {
        let errors = [name.err(), description.err()].into_iter().flatten();
        return Err(ApiError::Validation(errors.collect()));
    };
    let id = Uuid::now_v7();
    sqlx::query!(
        "insert into lists (id, workspace_id, name, description, double_optin, visibility) \
         values ($1, $2, $3, $4, coalesce($5, true), coalesce($6, 'private'))",
        id,
        workspace_id,
        name,
        description,
        input.double_optin,
        input.visibility as Option<Visibility>,
    )
    .execute(&mut *tx)
    .await
    // The only unique constraint a fresh `id` can hit is `(workspace_id, name)`; `source_ref` is
    // null here.
    .map_err(|e| match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("list-name-taken"),
        _ => e.into(),
    })?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::LIST_CREATED,
            target_type: Some("list"),
            target_id: Some(id.to_string()),
            metadata: json!({ "name": name }),
        },
    )
    .await?;
    let list = get(&mut tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(list)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{workspace_id}/lists/{id}",
    operation_id = "update_list",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id"),
    ),
    request_body = UpdateList,
    responses(
        (status = 200, body = List),
        (status = 403, description = "insufficient-role: editing a list takes editor"),
        (status = 404, description = "not a member, or no such list in this workspace"),
        (status = 409, description = "list-name-taken"),
        (status = 422, description = "invalid name or description, or an unknown field"),
    ),
)]
pub async fn update_list(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<UpdateList>,
) -> ApiResult<Json<List>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let name = match &input.name {
        Some(Some(n)) => validate_name(n).map(Some),
        Some(None) => null("name"),
        None => Ok(None),
    };
    let description = match &input.description {
        Some(d) => validate_description(d.as_deref().unwrap_or("")).map(Some),
        None => Ok(None),
    };
    let double_optin = match input.double_optin {
        Some(None) => null("double_optin"),
        d => Ok(d.flatten()),
    };
    let visibility = match input.visibility {
        Some(None) => null("visibility"),
        v => Ok(v.flatten()),
    };
    let (Ok(name), Ok(description), Ok(double_optin), Ok(visibility)) =
        (&name, &description, &double_optin, &visibility)
    else {
        let errors = [
            name.err(),
            description.err(),
            double_optin.err(),
            visibility.err(),
        ];
        return Err(ApiError::Validation(errors.into_iter().flatten().collect()));
    };
    // `pending` rows may outlive a `double_optin` change: one left on a list that is now
    // single opt-in still means "not confirmed, not mailable", and nothing else can resolve it
    // before the M1c confirmation flow. Only a NEW member's status follows the setting.
    let updated = sqlx::query!(
        "update lists set name = coalesce($3, name), description = coalesce($4, description), \
         double_optin = coalesce($5, double_optin), visibility = coalesce($6, visibility), \
         updated_at = now() where workspace_id = $1 and id = $2",
        workspace_id,
        id,
        name.as_deref(),
        description.as_deref(),
        *double_optin,
        *visibility as Option<Visibility>,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("list-name-taken"),
        _ => e.into(),
    })?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    let fields: Vec<&str> = [
        ("name", input.name.is_some()),
        ("description", input.description.is_some()),
        ("double_optin", double_optin.is_some()),
        ("visibility", visibility.is_some()),
    ]
    .into_iter()
    .filter(|(_, sent)| *sent)
    .map(|(field, _)| field)
    .collect();
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::LIST_UPDATED,
            target_type: Some("list"),
            target_id: Some(id.to_string()),
            metadata: json!({ "fields": fields }),
        },
    )
    .await?;
    let list = get(&mut tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok(Json(list))
}

/// A hard delete. It takes every membership of the list with it, unsubscribed ones included: the
/// list itself is gone, so there is nothing left to be unsubscribed from.
#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{workspace_id}/lists/{id}",
    operation_id = "delete_list",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id"),
    ),
    responses(
        (status = 204, description = "Deleted, with its memberships"),
        (status = 403, description = "insufficient-role: deleting a list takes editor"),
        (status = 404, description = "not a member, or no such list in this workspace"),
    ),
)]
pub async fn delete_list(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let deleted = sqlx::query!(
        "delete from lists where workspace_id = $1 and id = $2",
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
            action: actions::LIST_DELETED,
            target_type: Some("list"),
            target_id: Some(id.to_string()),
            metadata: json!({}),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/lists/{id}/members",
    operation_id = "list_list_members",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id"),
        PageParams,
    ),
    responses(
        (status = 200, description = "Every status, newest contact first", body = Page<ListMember>),
        (status = 404, description = "not a member, or no such list in this workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_list_members(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<ListMember>>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    get(&mut conn, workspace_id, id).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        ListMember,
        r#"select m.contact_id, c.email, m.status as "status: MembershipStatus",
                  m.subscribed_at, m.unsubscribed_at
           from list_memberships m
           join contacts c on c.workspace_id = m.workspace_id and c.id = m.contact_id
           where m.workspace_id = $1 and m.list_id = $2 and ($3::uuid is null or m.contact_id < $3)
           order by m.contact_id desc limit $4"#,
        workspace_id,
        id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |m| m.contact_id)))
}

/// How many members of the list may be emailed right now: subscribed and not suppressed, counted
/// over the `mailable_list_members` view, the one definition of eligibility. Deliberately exempt
/// from the 150 ms p95 target (1.4 s warm over 1.43M mailable contacts), so the UI fetches it on
/// its own. It is counted live, never kept as a counter on the list: hand-maintained counters
/// drift.
#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/lists/{id}/mailable-count",
    operation_id = "mailable_count",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id"),
    ),
    responses(
        (status = 200, body = MailableCount),
        (status = 404, description = "not a member, or no such list in this workspace"),
    ),
)]
pub async fn mailable_count(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<MailableCount>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    get(&mut conn, workspace_id, id).await?;
    let count = sqlx::query_scalar!(
        r#"select count(*) as "count!" from mailable_list_members
           where workspace_id = $1 and list_id = $2"#,
        workspace_id,
        id,
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(Json(MailableCount {
        count: count.unsigned_abs(),
    }))
}

/// Adding someone who is already `pending` or `subscribed` changes nothing; adding an
/// `unsubscribed` one subscribes them again.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/lists/{id}/members",
    operation_id = "add_list_member",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id"),
    ),
    request_body = NewMember,
    responses(
        (status = 200, description = "`subscribed` on a single-opt-in list, `pending` on a double-opt-in one", body = Membership),
        (status = 403, description = "insufficient-role: changing a list's members takes editor"),
        (status = 404, description = "not a member, or no such list or contact in this workspace"),
    ),
)]
pub async fn add_list_member(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<NewMember>,
) -> ApiResult<Json<Membership>> {
    let membership = add_audited(
        &state,
        &meta,
        &principal,
        workspace_id,
        id,
        input.contact_id,
    )
    .await?;
    Ok(Json(membership))
}

/// Unsubscribes; it does not delete the membership.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/lists/{id}/members/{contact_id}/unsubscribe",
    operation_id = "unsubscribe_list_member",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id"),
        ("contact_id" = Uuid, Path, description = "Contact id"),
    ),
    responses(
        (status = 204, description = "Unsubscribed. The membership stays, as `unsubscribed`"),
        (status = 403, description = "insufficient-role: changing a list's members takes editor"),
        (status = 404, description = "not a member, or the contact was never on this list"),
    ),
)]
pub async fn unsubscribe_list_member(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id, contact_id)): ApiPath<(Uuid, Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    unsubscribe_audited(&state, &meta, &principal, workspace_id, id, contact_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The one place a membership is added, for the list-side and the contact-side route alike: it
/// authorises first, writes, and audits in the same transaction. The audit row records ids and
/// the status, never an address.
async fn add_audited(
    state: &AppState,
    meta: &RequestMeta,
    principal: &Principal,
    workspace_id: Uuid,
    list_id: Uuid,
    contact_id: Uuid,
) -> ApiResult<Membership> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, principal, workspace_id, Role::Editor).await?;
    let status = add_member(
        &mut tx,
        workspace_id,
        list_id,
        contact_id,
        MembershipSource::of_single_add(principal),
    )
    .await?;
    audit::record(
        &mut *tx,
        meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::LIST_MEMBER_ADDED,
            target_type: Some("list"),
            target_id: Some(list_id.to_string()),
            metadata: json!({ "contact_id": contact_id, "status": status }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Membership {
        contact_id,
        list_id,
        status,
    })
}

/// The one place a membership is unsubscribed; see `add_audited`.
async fn unsubscribe_audited(
    state: &AppState,
    meta: &RequestMeta,
    principal: &Principal,
    workspace_id: Uuid,
    list_id: Uuid,
    contact_id: Uuid,
) -> ApiResult<()> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, principal, workspace_id, Role::Editor).await?;
    unsubscribe_member(&mut tx, workspace_id, list_id, contact_id).await?;
    audit::record(
        &mut *tx,
        meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::LIST_MEMBER_UNSUBSCRIBED,
            target_type: Some("list"),
            target_id: Some(list_id.to_string()),
            metadata: json!({ "contact_id": contact_id }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}/lists",
    operation_id = "list_contact_lists",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
        PageParams,
    ),
    responses(
        (status = 200, description = "Every list the contact has a membership on, whatever its status, newest list first", body = Page<ContactList>),
        (status = 404, description = "not a member, or no such contact in this workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_contact_lists(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<ContactList>>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    contacts::get(&mut conn, workspace_id, id).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        ContactList,
        r#"select m.list_id, l.name, m.status as "status: MembershipStatus",
                  m.subscribed_at, m.unsubscribed_at
           from list_memberships m
           join lists l on l.workspace_id = m.workspace_id and l.id = m.list_id
           where m.workspace_id = $1 and m.contact_id = $2 and ($3::uuid is null or m.list_id < $3)
           order by m.list_id desc limit $4"#,
        workspace_id,
        id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |l| l.list_id)))
}

/// The same as `POST /lists/{id}/members`, from the contact's side.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}/lists",
    operation_id = "add_contact_to_list",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
    ),
    request_body = NewContactList,
    responses(
        (status = 200, description = "`subscribed` on a single-opt-in list, `pending` on a double-opt-in one", body = Membership),
        (status = 403, description = "insufficient-role: changing a contact's lists takes editor"),
        (status = 404, description = "not a member, or no such contact or list in this workspace"),
    ),
)]
pub async fn add_contact_to_list(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<NewContactList>,
) -> ApiResult<Json<Membership>> {
    let membership =
        add_audited(&state, &meta, &principal, workspace_id, input.list_id, id).await?;
    Ok(Json(membership))
}

/// Unsubscribes a contact from a list, and keeps the membership.
///
/// The same as `POST /lists/{id}/members/{contact_id}/unsubscribe`, from the contact's side.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}/lists/{list_id}/unsubscribe",
    operation_id = "unsubscribe_contact_from_list",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
        ("list_id" = Uuid, Path, description = "List id"),
    ),
    responses(
        (status = 204, description = "Unsubscribed. The membership stays, as `unsubscribed`"),
        (status = 403, description = "insufficient-role: changing a contact's lists takes editor"),
        (status = 404, description = "not a member, or the contact was never on this list"),
    ),
)]
pub async fn unsubscribe_contact_from_list(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id, list_id)): ApiPath<(Uuid, Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    unsubscribe_audited(&state, &meta, &principal, workspace_id, list_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
