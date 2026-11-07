// SPDX-License-Identifier: AGPL-3.0-only

//! Who is calling: the session store, the sign-in handlers, and the `Principal` and
//! `CurrentUser` extractors that authenticated routes take.

use axum::{extract::FromRequestParts, http::request::Parts};
use axum_extra::extract::CookieJar;
use uuid::Uuid;

use crate::{app::AppState, error::ApiError};

pub mod login;
pub mod rate_limit;
pub mod sessions;

pub use login::routes;

/// An authenticated caller. Task 12 adds an API-key variant.
pub enum Principal {
    User {
        user_id: Uuid,
        session_hash: [u8; 32],
        csrf_token: String,
    },
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
