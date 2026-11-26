// SPDX-License-Identifier: AGPL-3.0-only

//! The subscriber's side of the product: the pages an email link leads to. Every route here is
//! unauthenticated and authenticated by its own HMAC token instead, is CSRF-exempt by
//! construction (`http::csrf` gates `/api/v1/` only), is rate-limited by client IP, and renders
//! server-side HTML that works with JavaScript disabled.

use axum::Router;

use crate::app::AppState;

pub mod confirm;
pub mod forms;
pub mod page;
pub mod token;
pub mod track;
pub mod unsubscribe;

/// The top-level path prefixes this module owns: `/c/` confirm, `/u/` unsubscribe and the
/// preference center, `/f/` the hosted signup forms, `/o/` the open pixel and `/l/` the click
/// redirect.
const PREFIXES: [&str; 5] = ["/c", "/u", "/f", "/o", "/l"];

/// True for those prefixes and everything under them, **including the bare prefix with no token**.
///
/// That bare case is the decision this function exists to record. `/u` and `/u/` match no route
/// here — axum 0.8 will not bind `{token}` to an empty segment — so without this they reached the
/// SPA fallback and were answered `index.html` with a `200`. A subscriber whose mail client
/// truncated the link at the last slash would get a single-page app asking them to sign in, which
/// is a worse answer than "not found" in every way that matters: it suggests an account exists, it
/// is a cacheable `200` naming nothing, and it gives a person trying to leave a list no signal at
/// all. These prefixes are server-rendered territory, there is no SPA route under any of them
/// (`web/src/router.tsx` claims `/`, `/login`, `/account/…`, `/invite/…`, `/workspaces/…`, `/w/…`
/// and `/admin/…`), so a `404` is both honest and free.
///
/// Decided once for all three in Task 5 so that `/f/` inherits it rather than repeating the
/// question. Both `http::web_ui::is_reserved` and `http::security_headers::uncacheable` call this
/// — the two are otherwise deliberately separate predicates, and a test pins that they agree, so
/// a prefix added to one and not the other would fail there. Here the two ideas genuinely are the
/// same idea: never the SPA, and never kept by a shared cache.
///
/// A near miss is not a match: `/contacts` and `/forms` are not under `/c` or `/f`, because the
/// character after the prefix must be a `/` or nothing at all.
pub fn owns_path(path: &str) -> bool {
    PREFIXES.iter().any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// Merged into `http::routes()`, where axum matches it ahead of the SPA fallback.
pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(confirm::routes())
        .merge(unsubscribe::routes())
        .merge(forms::routes())
        .merge(track::routes())
}

#[cfg(test)]
mod tests {
    use super::owns_path;

    #[test]
    fn the_public_prefixes_are_owned_and_their_near_misses_are_not() {
        for path in [
            "/c", "/c/", "/c/abc", "/u", "/u/", "/u/abc", "/f", "/f/", "/f/abc", "/o", "/o/x",
            "/l", "/l/x/0",
        ] {
            assert!(owns_path(path), "{path}");
        }
        for path in [
            "/",
            "/contacts",
            "/forms",
            "/customers",
            "/cc/x",
            "/api/v1/me",
            "/login",
            "/options",
        ] {
            assert!(!owns_path(path), "{path}");
        }
    }
}
