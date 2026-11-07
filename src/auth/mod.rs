// SPDX-License-Identifier: AGPL-3.0-only

//! Who is calling: the session store, the sign-in handlers, and the `Principal` and
//! `CurrentUser` extractors that authenticated routes take.

use axum::{Router, extract::FromRequestParts, http::request::Parts};
use axum_extra::extract::CookieJar;
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::{
    app::AppState,
    audit::Actor,
    error::{ApiError, ApiResult},
    workspaces::roles::Role,
};

pub mod login;
pub mod rate_limit;
pub mod sessions;
pub mod totp;

pub fn routes() -> Router<AppState> {
    login::routes().merge(totp::routes())
}

/// An authenticated caller. Task 12 adds an API-key variant.
pub enum Principal {
    User {
        user_id: Uuid,
        session_hash: [u8; 32],
        csrf_token: String,
    },
}

impl Principal {
    /// Who to record in the audit log.
    pub fn actor(&self) -> Actor {
        let Principal::User { user_id, .. } = self;
        Actor::User(*user_id)
    }
}

/// The session cookie must name a full session that is unexpired and not idle. A pending-MFA
/// session, an unknown cookie and no cookie at all are the same `401 unauthenticated`.
impl FromRequestParts<AppState> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
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

/// A signed-in human (cookie session). Task 12 makes this reject API keys.
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
    // Task 12's API-key variant resolves its workspace and role here; until then this match
    // is irrefutable and adding a variant is a compile error.
    let Principal::User { user_id, .. } = principal;
    let role = sqlx::query_scalar!(
        r#"select role as "role: Role" from memberships where workspace_id = $1 and user_id = $2"#,
        workspace_id,
        user_id,
    )
    .fetch_optional(db)
    .await?
    .ok_or(ApiError::NotFound)?;
    if role < min {
        return Err(ApiError::Forbidden("insufficient-role"));
    }
    Ok(WorkspaceAccess { workspace_id, role })
}
