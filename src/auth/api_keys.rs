// SPDX-License-Identifier: AGPL-3.0-only

//! Workspace-scoped API keys, a second kind of `Principal` beside the cookie session.
//!
//! A key is `nm_{prefix}_{secret}`. The table keeps the `prefix` (to find the row) and the sha256
//! of the secret half; the whole key is shown once, in the creation response, and never stored,
//! listed, logged or audited. Keys are sent as `Authorization: Bearer nm_…`.
//!
//! Creating and revoking keys takes a signed-in human (`CurrentUser`), never a key: a leaked admin
//! key must not be able to mint successors that outlive its own revocation. The cost is that key
//! rotation cannot be fully automated; that is deliberate.

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    routing::{delete, get},
};
use chrono::{DateTime, Utc};
use rand::{TryRng, rngs::SysRng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{CurrentUser, Principal, authorize};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    crypto::tokens::{constant_time_eq, random_token, sha256},
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    pagination::{Page, PageParams},
    workspaces::{
        roles::{Role, can_assign},
        validate_name,
    },
};

const PREFIX_LEN: usize = 8;
const PREFIX_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/workspaces/{id}/api-keys", get(list).post(create))
        .route("/api/v1/workspaces/{id}/api-keys/{key_id}", delete(revoke))
}

/// A verified key's identity, as `Principal::ApiKey` carries it.
pub struct ResolvedKey {
    pub key_id: Uuid,
    pub workspace_id: Uuid,
    pub role: Role,
}

/// 8 lowercase alphanumerics from the OS CSPRNG. The prefix is an identifier, not a secret; the
/// modulo bias (256 % 36) is irrelevant for that.
fn random_prefix() -> String {
    let mut buf = [0u8; PREFIX_LEN];
    SysRng.try_fill_bytes(&mut buf).expect("OS RNG unavailable");
    buf.iter()
        .map(|b| char::from(PREFIX_ALPHABET[usize::from(*b) % PREFIX_ALPHABET.len()]))
        .collect()
}

/// The unrevoked key behind `nm_{prefix}_{secret}`, or `None` for anything else: malformed,
/// unknown prefix, wrong secret and revoked are indistinguishable to the caller. The one read of
/// `api_keys` for authentication. The secret half is compared by its hash in constant time.
pub async fn resolve(db: &PgPool, presented: &str) -> sqlx::Result<Option<ResolvedKey>> {
    let Some((prefix, secret)) = presented
        .strip_prefix("nm_")
        .and_then(|rest| rest.split_once('_'))
    else {
        return Ok(None);
    };
    if prefix.len() != PREFIX_LEN {
        return Ok(None);
    }
    let Some(row) = sqlx::query!(
        r#"select id, workspace_id, role as "role: Role", secret_hash,
                  (last_used_at is null or last_used_at < now() - interval '1 minute') as "stale!"
           from api_keys where prefix = $1 and revoked_at is null"#,
        prefix,
    )
    .fetch_optional(db)
    .await?
    else {
        return Ok(None);
    };
    if !constant_time_eq(&sha256(secret.as_bytes()), &row.secret_hash) {
        return Ok(None);
    }
    // shortcut: last_used_at is written at most once a minute, so it is only accurate to about a
    // minute; write on every request if exact last-use times ever matter.
    // Bookkeeping must never fail the request it rides on.
    if row.stale
        && let Err(e) = sqlx::query!(
            "update api_keys set last_used_at = now() where id = $1",
            row.id
        )
        .execute(db)
        .await
    {
        tracing::warn!(key_id = %row.id, error = %e, "could not record api key last_used_at");
    }
    Ok(Some(ResolvedKey {
        key_id: row.id,
        workspace_id: row.workspace_id,
        role: row.role,
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct NewApiKey {
    pub name: String,
    /// `viewer`, `editor` or `admin`, and no higher than the caller's own role.
    pub role: Role,
}

/// The one response that carries the key.
#[derive(Serialize, ToSchema)]
pub struct CreatedApiKey {
    pub id: Uuid,
    pub name: String,
    pub role: Role,
    pub key: String,
    pub created_at: DateTime<Utc>,
}

/// A key as listed: its prefix identifies it, nothing in it can authenticate.
#[derive(Serialize, ToSchema)]
pub struct ApiKeySummary {
    pub id: Uuid,
    pub name: String,
    pub role: Role,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{id}/api-keys",
    operation_id = "create_api_key",
    params(("id" = Uuid, Path, description = "Workspace id")),
    request_body = NewApiKey,
    responses(
        (status = 201, description = "Created; `key` is returned here and never again", body = CreatedApiKey),
        (status = 403, description = "user-session-required (a key cannot create keys) or insufficient-role: below admin, or the role is above the caller's"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "invalid name, or role `owner`"),
    ),
)]
pub async fn create(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<NewApiKey>,
) -> ApiResult<(
    StatusCode,
    [(header::HeaderName, &'static str); 1],
    Json<CreatedApiKey>,
)> {
    let principal = user.principal();
    let mut tx = state.pool.begin().await?;
    let access = authorize(&mut *tx, &principal, id, Role::Admin).await?;
    // `owner` is checked here so the table's check constraint never has to answer for it.
    let role = if body.role == Role::Owner {
        Err(FieldError::new("role", "must be viewer, editor or admin"))
    } else {
        Ok(body.role)
    };
    let (name, role) = match (validate_name(&body.name), role) {
        (Ok(n), Ok(r)) => (n, r),
        (n, r) => {
            let errors = [n.err(), r.err()].into_iter().flatten().collect();
            return Err(ApiError::Validation(errors));
        }
    };
    if !can_assign(access.role, None, role) {
        return Err(ApiError::Forbidden("insufficient-role"));
    }

    let key_id = Uuid::now_v7();
    // A prefix collision (36^8 values) would surface as a 500; the caller can simply retry.
    let prefix = random_prefix();
    let secret = random_token(32);
    let created_at = sqlx::query_scalar!(
        "insert into api_keys (id, workspace_id, name, prefix, secret_hash, role, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7) returning created_at",
        key_id,
        id,
        name,
        prefix,
        &sha256(secret.as_bytes())[..],
        role as Role,
        user.user_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(id),
            actor: principal.actor(),
            action: actions::API_KEY_CREATED,
            target_type: Some("api_key"),
            target_id: Some(key_id.to_string()),
            // The prefix identifies the key; the secret is never recorded.
            metadata: json!({ "name": name, "role": role, "prefix": prefix }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(CreatedApiKey {
            id: key_id,
            name,
            role,
            key: format!("nm_{prefix}_{secret}"),
            created_at,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{id}/api-keys",
    operation_id = "list_api_keys",
    params(("id" = Uuid, Path, description = "Workspace id"), PageParams),
    responses(
        (status = 200, description = "Unrevoked keys, newest first", body = Page<ApiKeySummary>),
        (status = 403, description = "insufficient-role: below admin"),
        (status = 404, description = "not a member, or no such workspace"),
    ),
)]
pub async fn list(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<ApiKeySummary>>> {
    authorize(&state.pool, &principal, id, Role::Admin).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        ApiKeySummary,
        r#"select id, name, role as "role: Role", prefix, created_at, last_used_at
           from api_keys
           where workspace_id = $1 and revoked_at is null and ($2::uuid is null or id < $2)
           order by id desc limit $3"#,
        id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |k| k.id)))
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{id}/api-keys/{key_id}",
    operation_id = "revoke_api_key",
    params(
        ("id" = Uuid, Path, description = "Workspace id"),
        ("key_id" = Uuid, Path, description = "Key id"),
    ),
    responses(
        (status = 204, description = "Revoked"),
        (status = 403, description = "user-session-required (a key cannot revoke keys) or insufficient-role: below admin"),
        (status = 404, description = "not a member, or no such unrevoked key in this workspace"),
    ),
)]
pub async fn revoke(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiPath((id, key_id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let principal = user.principal();
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, id, Role::Admin).await?;
    let key = sqlx::query!(
        r#"update api_keys set revoked_at = now()
           where id = $1 and workspace_id = $2 and revoked_at is null
           returning name, prefix, role as "role: Role""#,
        key_id,
        id,
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(id),
            actor: principal.actor(),
            action: actions::API_KEY_REVOKED,
            target_type: Some("api_key"),
            target_id: Some(key_id.to_string()),
            metadata: json!({ "name": key.name, "role": key.role, "prefix": key.prefix }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
