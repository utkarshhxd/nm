// SPDX-License-Identifier: AGPL-3.0-only

//! Workspaces: the tenant boundary. A workspace has members, each with a role; every
//! workspace-scoped handler calls `auth::authorize` before it reads or writes anything.

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::{PgExecutor, PgPool};
use utoipa::ToSchema;
use uuid::Uuid;

use self::roles::Role;
use crate::{
    app::AppState,
    audit::{self, Actor, AuditEvent, actions},
    auth::{CurrentUser, Principal, authorize},
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    pagination::{Page, PageParams},
};

pub mod invites;
pub mod members;
pub mod roles;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/workspaces", get(list).post(create))
        .route("/api/v1/workspaces/{id}", get(show).patch(update))
        .merge(members::routes())
        .merge(invites::routes())
}

/// A workspace as one caller sees it: `role` is the caller's own.
#[derive(Serialize, ToSchema)]
pub struct Workspace {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub timezone: String,
    pub role: Role,
}

/// The `/me` summary of a membership.
#[derive(Serialize, ToSchema)]
pub struct MeWorkspace {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub role: Role,
}

#[derive(Deserialize, ToSchema)]
pub struct NewWorkspace {
    pub slug: String,
    pub name: String,
    /// An IANA zone such as `Europe/London`; defaults to `UTC`.
    pub timezone: Option<String>,
}

/// The slug cannot change in M0, so an unknown field (`slug` included) is rejected, not ignored.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateWorkspace {
    pub name: Option<String>,
    pub timezone: Option<String>,
}

/// `^[a-z0-9](?:[a-z0-9-]{0,38}[a-z0-9])?$`, written out.
fn validate_slug(slug: &str) -> Result<(), FieldError> {
    let ok_char = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let b = slug.as_bytes();
    if (1..=40).contains(&b.len())
        && b.iter().all(|&c| ok_char(c) || c == b'-')
        && ok_char(b[0])
        && ok_char(b[b.len() - 1])
    {
        return Ok(());
    }
    Err(FieldError::new(
        "slug",
        "must be 1-40 characters: lowercase letters, digits and hyphens, not starting or ending with a hyphen",
    ))
}

/// Trimmed, 1..=100 characters, no control characters (NUL would also fail an audit insert).
pub(crate) fn validate_name(name: &str) -> Result<String, FieldError> {
    let name = name.trim();
    if (1..=100).contains(&name.chars().count()) && !name.chars().any(char::is_control) {
        return Ok(name.to_owned());
    }
    Err(FieldError::new(
        "name",
        "must be 1-100 characters with no control characters",
    ))
}

/// Returns the zone's canonical name.
pub(crate) fn validate_timezone(tz: &str) -> Result<&'static str, FieldError> {
    tz.parse::<Tz>().map(|t| t.name()).map_err(|_| {
        FieldError::new(
            "timezone",
            "must be an IANA time zone such as Europe/London",
        )
    })
}

/// Creates a workspace with `owner` as its first owner, and audits it, in one transaction.
/// `409 slug-taken` if the slug exists; `422` per invalid field.
pub async fn create_workspace(
    db: &PgPool,
    meta: &RequestMeta,
    owner: Uuid,
    input: NewWorkspace,
) -> ApiResult<Workspace> {
    let slug = validate_slug(&input.slug);
    let name = validate_name(&input.name);
    let timezone = validate_timezone(input.timezone.as_deref().unwrap_or("UTC"));
    let (slug, name, timezone) = match (slug, name, timezone) {
        (Ok(()), Ok(name), Ok(tz)) => (input.slug, name, tz),
        (s, n, t) => {
            let errors = [s.err(), n.err(), t.err()].into_iter().flatten().collect();
            return Err(ApiError::Validation(errors));
        }
    };

    let id = Uuid::now_v7();
    let mut tx = db.begin().await?;
    sqlx::query!(
        "insert into workspaces (id, slug, name, timezone) values ($1, $2, $3, $4)",
        id,
        slug,
        name,
        timezone,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("slug-taken"),
        _ => e.into(),
    })?;
    sqlx::query!(
        "insert into memberships (workspace_id, user_id, role) values ($1, $2, $3)",
        id,
        owner,
        Role::Owner as Role,
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut *tx,
        meta,
        AuditEvent {
            workspace_id: Some(id),
            actor: Actor::User(owner),
            action: actions::WORKSPACE_CREATED,
            target_type: Some("workspace"),
            target_id: Some(id.to_string()),
            metadata: json!({ "slug": slug, "name": name }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Workspace {
        id,
        slug,
        name,
        timezone: timezone.to_owned(),
        role: Role::Owner,
    })
}

/// Every workspace the user belongs to, for `/me`, **by name**.
///
/// Deliberately NOT `list`'s `w.id desc`, and the two must not be "unified": this is a short,
/// unpaginated list a human reads in the workspace switcher, and `web/routes/home.tsx` redirects
/// into its first element. By name that redirect is stable and meaningful; by id it would be
/// whichever workspace happened to be created last, and it would move under the person every
/// time one is added. `w.id` only breaks ties, so the order is total and the list never
/// reshuffles between requests. `list` cannot do this — see the comment on its query.
pub async fn for_user(db: impl PgExecutor<'_>, user_id: Uuid) -> sqlx::Result<Vec<MeWorkspace>> {
    sqlx::query_as!(
        MeWorkspace,
        r#"select w.id, w.slug, w.name, m.role as "role: Role"
           from memberships m join workspaces w on w.id = m.workspace_id
           where m.user_id = $1 order by w.name, w.id"#,
        user_id,
    )
    .fetch_all(db)
    .await
}

/// The caller must already have been authorized; `role` is what `authorize` returned.
async fn load(db: impl PgExecutor<'_>, id: Uuid, role: Role) -> ApiResult<Workspace> {
    let w = sqlx::query!(
        "select slug, name, timezone from workspaces where id = $1",
        id
    )
    .fetch_one(db)
    .await?;
    Ok(Workspace {
        id,
        slug: w.slug,
        name: w.name,
        timezone: w.timezone,
        role,
    })
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces",
    operation_id = "list_workspaces",
    params(PageParams),
    responses((status = 200, description = "The caller's workspaces, newest first", body = Page<Workspace>)),
)]
pub async fn list(
    State(state): State<AppState>,
    principal: Principal,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<Workspace>>> {
    let limit = page.limit();
    let after = page.after()?;
    let user_id = match principal {
        Principal::User { user_id, .. } => user_id,
        // A key sees its own workspace and nothing else: a page of one, or none past its cursor.
        Principal::ApiKey {
            workspace_id, role, ..
        } => {
            let rows = if after.is_none_or(|a| workspace_id < a) {
                vec![load(&state.pool, workspace_id, role).await?]
            } else {
                vec![]
            };
            return Ok(Json(Page::from_rows(rows, limit, |w| w.id)));
        }
    };
    // `w.id desc` is forced by the cursor: the page filters on `w.id < $2`, so the ordering has
    // to be the same column, descending. That is why this differs from `for_user`, which orders
    // by name — do not "unify" the two, they are the same rows serving different jobs.
    let rows = sqlx::query_as!(
        Workspace,
        r#"select w.id, w.slug, w.name, w.timezone, m.role as "role: Role"
           from memberships m join workspaces w on w.id = m.workspace_id
           where m.user_id = $1 and ($2::uuid is null or w.id < $2)
           order by w.id desc limit $3"#,
        user_id,
        after,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |w| w.id)))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces",
    operation_id = "create_workspace",
    request_body = NewWorkspace,
    responses(
        (status = 201, body = Workspace),
        (status = 403, description = "instance-admin-required"),
        (status = 409, description = "slug-taken"),
        (status = 422, description = "invalid slug, name or timezone"),
    ),
)]
pub async fn create(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiJson(input): ApiJson<NewWorkspace>,
) -> ApiResult<(StatusCode, Json<Workspace>)> {
    let caller = crate::users::find_by_id(&state.pool, user.user_id)
        .await?
        .ok_or(ApiError::Unauthorized("unauthenticated"))?;
    if !caller.is_instance_admin {
        return Err(ApiError::Forbidden("instance-admin-required"));
    }
    let ws = create_workspace(&state.pool, &meta, user.user_id, input).await?;
    Ok((StatusCode::CREATED, Json(ws)))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{id}",
    params(("id" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, body = Workspace),
        (status = 404, description = "not a member, or no such workspace"),
    ),
)]
pub async fn show(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Workspace>> {
    let access = authorize(&state.pool, &principal, id, Role::Viewer).await?;
    Ok(Json(load(&state.pool, id, access.role).await?))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{id}",
    params(("id" = Uuid, Path, description = "Workspace id")),
    request_body = UpdateWorkspace,
    responses(
        (status = 200, body = Workspace),
        (status = 403, description = "insufficient-role"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "invalid name or timezone"),
    ),
)]
pub async fn update(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<UpdateWorkspace>,
) -> ApiResult<Json<Workspace>> {
    let mut tx = state.pool.begin().await?;
    let access = authorize(&mut *tx, &principal, id, Role::Admin).await?;

    let name = input.name.as_deref().map(validate_name).transpose();
    let timezone = input.timezone.as_deref().map(validate_timezone).transpose();
    let (name, timezone) = match (name, timezone) {
        (Ok(n), Ok(t)) => (n, t),
        (n, t) => {
            let errors = [n.err(), t.err()].into_iter().flatten().collect();
            return Err(ApiError::Validation(errors));
        }
    };

    let w = sqlx::query!(
        "update workspaces set name = coalesce($2, name), timezone = coalesce($3, timezone), \
         updated_at = now() where id = $1 returning slug, name, timezone",
        id,
        name,
        timezone,
    )
    .fetch_one(&mut *tx)
    .await?;
    // Only the fields that were sent, as their new values.
    let mut changed = Map::new();
    if let Some(n) = name {
        changed.insert("name".into(), Value::from(n));
    }
    if let Some(t) = timezone {
        changed.insert("timezone".into(), Value::from(t));
    }
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(id),
            actor: principal.actor(),
            action: actions::WORKSPACE_UPDATED,
            target_type: Some("workspace"),
            target_id: Some(id.to_string()),
            metadata: Value::Object(changed),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(Workspace {
        id,
        slug: w.slug,
        name: w.name,
        timezone: w.timezone,
        role: access.role,
    }))
}
