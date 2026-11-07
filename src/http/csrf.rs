// SPDX-License-Identifier: AGPL-3.0-only

use axum::{
    extract::{Request, State},
    http::{HeaderMap, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use axum_extra::extract::CookieJar;

use crate::{app::AppState, auth::sessions, crypto::tokens::constant_time_eq, error::ApiError};

/// Sign-in routes: no session exists yet, so there is no token to present. The TOTP and passkey
/// ones arrive with Tasks 9 and 13.
const EXEMPT: [&str; 4] = [
    "/api/v1/auth/login",
    "/api/v1/auth/login/totp",
    "/api/v1/auth/passkey/login/start",
    "/api/v1/auth/passkey/login/finish",
];

/// Unsafe `/api/v1` requests carrying a cookie for a full session must send that session's
/// `X-CSRF-Token`. No cookie, or one that does not resolve, is not checked here: the request is
/// simply unauthenticated and the handler rejects it. Requests with an `Authorization` header
/// are skipped; Task 12 makes those ignore cookies altogether.
pub async fn require_csrf(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let unsafe_method = matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if !unsafe_method
        || !path.starts_with("/api/v1/")
        || EXEMPT.contains(&path)
        || req.headers().contains_key(header::AUTHORIZATION)
    {
        return next.run(req).await;
    }
    let Some(cookie) = CookieJar::from_headers(req.headers())
        .get(sessions::COOKIE_NAME)
        .map(|c| c.value().to_owned())
    else {
        return next.run(req).await;
    };
    // `resolve` is the one reader of `sessions`: it already refuses pending-MFA and expired ones.
    let session = match sessions::resolve(&state.pool, &cookie).await {
        Ok(Some(s)) => s,
        Ok(None) => return next.run(req).await,
        Err(e) => return ApiError::from(e).into_response(),
    };
    if presented(req.headers())
        .is_some_and(|t| constant_time_eq(t.as_bytes(), session.csrf_token.as_bytes()))
    {
        next.run(req).await
    } else {
        ApiError::Forbidden("csrf").into_response()
    }
}

fn presented(headers: &HeaderMap) -> Option<&str> {
    headers.get("x-csrf-token")?.to_str().ok()
}
