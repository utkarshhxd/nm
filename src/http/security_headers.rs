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

/// Paths whose responses a shared cache must never keep a copy of.
///
/// The whole API: NM is meant to run behind a reverse proxy, and `GET /api/v1/me` is a 200
/// `application/json` with no `Set-Cookie` and no `Vary` — exactly what an nginx `proxy_cache`,
/// Varnish or a CDN will happily store and hand to the next caller. Its body carries the
/// signed-in user's email, `is_instance_admin`, workspaces and `csrf_token`, and that token is a
/// usable write credential. The audit log and the member list are the same shape.
///
/// The probes: a cached `/readyz` answers about some earlier instant, which is the one thing a
/// readiness probe must never do.
///
/// Deliberately NOT the SPA or `/assets/`: `web_ui` decides their caching, and the hashed bundle
/// is served `immutable`, which is what keeps the frontend fast. Widening this to every path
/// would silently throw that away.
fn uncacheable(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/") || path == "/healthz" || path == "/readyz"
}

/// Sets the baseline security headers on every response. Wrap the whole router
/// with `middleware::from_fn(security_headers)` so 404s and errors get them too.
pub async fn security_headers(req: Request, next: Next) -> Response {
    let no_store = uncacheable(req.uri().path());
    let mut res = next.run(req).await;
    for (name, value) in HEADERS {
        res.headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    if no_store {
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    res
}

#[cfg(test)]
mod tests {
    use super::uncacheable;

    #[test]
    fn no_store_covers_the_api_and_the_probes_but_not_the_assets() {
        for path in ["/api", "/api/v1/me", "/api/v1/docs/", "/healthz", "/readyz"] {
            assert!(uncacheable(path), "{path}");
        }
        for path in ["/assets/app-abc.js", "/", "/login", "/apiary"] {
            assert!(!uncacheable(path), "{path}");
        }
    }
}
