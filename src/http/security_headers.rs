// SPDX-License-Identifier: AGPL-3.0-only

use axum::{
    extract::Request,
    http::{HeaderName, HeaderValue, header},
    middleware::Next,
    response::Response,
};

// No `script-src`, so scripts fall back to `default-src 'self'`: an inline script is blocked.
// That is why web/public/theme.js, which must run before first paint, is an external blocking
// <script src> and not inline in index.html. Do not inline it, and do not add a script hash here:
// a hash ties this header to the script's exact bytes, so a later edit would silently break the
// theme in production. web/e2e/support/app.ts serves this same CSP to catch an inlined script.
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
///
/// The list is the same as `web_ui::is_reserved`'s, and the two stay separate on purpose: "never
/// cached" and "never the SPA" are different ideas that happen to cover the same paths today.
/// `uncacheable_and_is_reserved_agree` below pins that they still coincide, so adding a path to
/// one and not the other fails rather than drifting.
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
    use crate::http::web_ui::is_reserved;

    /// Every path either predicate has an opinion about, plus near misses.
    const PATHS: [&str; 11] = [
        "/api",
        "/api/",
        "/api/v1/me",
        "/api/v1/docs/",
        "/healthz",
        "/readyz",
        "/assets/app-abc.js",
        "/",
        "/login",
        "/apiary",
        "/healthzz",
    ];

    #[test]
    fn no_store_covers_the_api_and_the_probes_but_not_the_assets() {
        for path in ["/api", "/api/v1/me", "/api/v1/docs/", "/healthz", "/readyz"] {
            assert!(uncacheable(path), "{path}");
        }
        for path in ["/assets/app-abc.js", "/", "/login", "/apiary", "/healthzz"] {
            assert!(!uncacheable(path), "{path}");
        }
    }

    /// The two predicates are separate ideas, but they must stay in step: a path that is reserved
    /// from the SPA is server-generated, and a server-generated response is one a shared cache
    /// must not keep. Adding one to only one of them should fail here, not in production.
    #[test]
    fn uncacheable_and_is_reserved_agree() {
        for path in PATHS {
            assert_eq!(
                uncacheable(path),
                is_reserved(path),
                "{path}: security_headers::uncacheable and web_ui::is_reserved disagree"
            );
        }
    }
}
