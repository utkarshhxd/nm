// SPDX-License-Identifier: AGPL-3.0-only

//! Who is calling: the session store, the sign-in handlers, and the `Principal` and
//! `CurrentUser` extractors that authenticated routes take.

use axum::{
    Router,
    extract::FromRequestParts,
    http::{HeaderMap, header::AUTHORIZATION, request::Parts},
};
use axum_extra::extract::CookieJar;
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::{
    app::AppState,
    audit::Actor,
    error::{ApiError, ApiResult},
    workspaces::roles::Role,
};

pub mod api_keys;
pub mod login;
pub mod rate_limit;
pub mod sessions;
pub mod totp;

pub fn routes() -> Router<AppState> {
    login::routes()
        .merge(totp::routes())
        .merge(api_keys::routes())
}

/// An authenticated caller: a signed-in user (cookie) or a workspace API key (Bearer).
pub enum Principal {
    User {
        user_id: Uuid,
        session_hash: [u8; 32],
        csrf_token: String,
    },
    /// Confined to one workspace, at the role the key was created with.
    ApiKey {
        key_id: Uuid,
        workspace_id: Uuid,
        role: Role,
    },
}

impl Principal {
    /// Who to record in the audit log.
    pub fn actor(&self) -> Actor {
        match self {
            Principal::User { user_id, .. } => Actor::User(*user_id),
            Principal::ApiKey { key_id, .. } => Actor::ApiKey(*key_id),
        }
    }
}

/// The credential of an `Authorization: Bearer` header (scheme matched case-insensitively), which
/// may be empty; `None` if there is no such header. Any other scheme, such as a proxy's `Basic`, is
/// not ours and leaves cookie authentication alone. A header that is not valid text counts as a
/// bearer attempt with an empty credential, so it fails closed rather than falling back to the
/// cookie.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get_all(AUTHORIZATION).iter().find_map(|v| {
        let Ok(v) = v.to_str() else { return Some("") };
        let (scheme, credential) = v.split_once(' ').unwrap_or((v, ""));
        scheme
            .eq_ignore_ascii_case("bearer")
            .then(|| credential.trim())
    })
}

/// With an `Authorization: Bearer` header the request is an API-key request and the cookie is not
/// read at all: a bad or revoked key is `401 invalid-api-key`, never a fall back to a valid
/// session (that would be a confused deputy). Otherwise the session cookie must name a full session
/// that is unexpired and not idle; a pending-MFA session, an unknown cookie and no cookie at all
/// are the same `401 unauthenticated`.
impl FromRequestParts<AppState> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if let Some(key) = bearer(&parts.headers) {
            return match api_keys::resolve(&state.pool, key).await? {
                Some(k) => Ok(Principal::ApiKey {
                    key_id: k.key_id,
                    workspace_id: k.workspace_id,
                    role: k.role,
                }),
                None => Err(ApiError::Unauthorized("invalid-api-key")),
            };
        }
        let jar = CookieJar::from_headers(&parts.headers);
        let Some(cookie) = jar.get(sessions::COOKIE_NAME) else {
            return Err(ApiError::Unauthorized("unauthenticated"));
        };
        match sessions::resolve(&state.pool, cookie.value()).await? {
            Some(s) => Ok(Principal::User {
                user_id: s.user_id,
                session_hash: s.session_hash,
                csrf_token: s.csrf_token,
            }),
            None => Err(ApiError::Unauthorized("unauthenticated")),
        }
    }
}

/// A signed-in human (cookie session). An API key is refused with `403 user-session-required`:
/// account security, and anything that grants access to the workspace, belong to people.
pub struct CurrentUser {
    pub user_id: Uuid,
    pub session_hash: [u8; 32],
    pub csrf_token: String,
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        match Principal::from_request_parts(parts, state).await? {
            Principal::User {
                user_id,
                session_hash,
                csrf_token,
            } => Ok(CurrentUser {
                user_id,
                session_hash,
                csrf_token,
            }),
            Principal::ApiKey { .. } => Err(ApiError::Forbidden("user-session-required")),
        }
    }
}

impl CurrentUser {
    /// This user as a `Principal`, for `authorize`.
    pub fn principal(&self) -> Principal {
        Principal::User {
            user_id: self.user_id,
            session_hash: self.session_hash,
            csrf_token: self.csrf_token.clone(),
        }
    }
}

/// What a caller holds in a workspace.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceAccess {
    pub workspace_id: Uuid,
    pub role: Role,
}

/// The gate every workspace-scoped handler goes through before it touches workspace data.
/// A caller who is not a member gets `404`, the same as for a workspace that does not exist: a
/// `403` would confirm it is there. A member below `min` gets `403 insufficient-role`, since they
/// already know it exists.
///
/// Takes an executor so a handler that goes on to write can authorize in its own transaction.
pub async fn authorize(
    db: impl PgExecutor<'_>,
    principal: &Principal,
    workspace_id: Uuid,
    min: Role,
) -> ApiResult<WorkspaceAccess> {
    let role = match principal {
        Principal::User { user_id, .. } => sqlx::query_scalar!(
            r#"select role as "role: Role" from memberships where workspace_id = $1 and user_id = $2"#,
            workspace_id,
            user_id,
        )
        .fetch_optional(db)
        .await?
        .ok_or(ApiError::NotFound)?,
        // A key belongs to one workspace; any other is a 404, exactly as for a non-member.
        Principal::ApiKey {
            workspace_id: key_workspace,
            role,
            ..
        } => {
            if *key_workspace != workspace_id {
                return Err(ApiError::NotFound);
            }
            *role
        }
    };
    if role < min {
        return Err(ApiError::Forbidden("insufficient-role"));
    }
    Ok(WorkspaceAccess { workspace_id, role })
}
