// SPDX-License-Identifier: AGPL-3.0-only

use axum::{
    extract::Request,
    http::{HeaderName, HeaderValue, header},
    middleware::Next,
    response::Response,
};

const HEADERS: [(HeaderName, &str); 4] = [
    (
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; \
         frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
    ),
    (header::X_FRAME_OPTIONS, "DENY"),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::REFERRER_POLICY, "strict-origin-when-cross-origin"),
];

/// Sets the baseline security headers on every response. Wrap the whole router
/// with `middleware::from_fn(security_headers)` so 404s and errors get them too.
pub async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    for (name, value) in HEADERS {
        res.headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    res
}
