// SPDX-License-Identifier: AGPL-3.0-only

//! Serves the Vite build that is compiled into the binary.

use std::borrow::Cow;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

use crate::error::ApiError;

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NO_CACHE: &str = "no-cache";
const NOT_BUILT: &str = "web UI not built; run pnpm --dir web build";

/// Where the built UI comes from. Tests substitute a map for the embedded build.
pub trait AssetSource: Send + Sync {
    fn get(&self, path: &str) -> Option<Cow<'static, [u8]>>;
}

/// `web/dist`, compiled in. `allow_missing` keeps a checkout that has never run
/// `pnpm --dir web build` compiling; `serve_web` then answers 503.
#[derive(RustEmbed)]
#[folder = "web/dist"]
#[allow_missing = true]
pub struct EmbeddedAssets;

impl AssetSource for EmbeddedAssets {
    fn get(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        <EmbeddedAssets as RustEmbed>::get(path).map(|file| file.data)
    }
}

/// Router fallback: every path the API, the health checks and the docs did not claim.
pub async fn fallback(uri: Uri, headers: HeaderMap) -> Response {
    serve_web(
        &EmbeddedAssets,
        uri.path(),
        headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok()),
    )
}

/// Serves `path` from `source`, falling back to `index.html` so the SPA can route it.
pub fn serve_web(source: &dyn AssetSource, path: &str, accept_encoding: Option<&str>) -> Response {
    // Exactly one leading slash. Stripping every slash instead would turn `//api/v1/nope`
    // into `api/v1/nope`, which misses `is_reserved` below and would be answered with
    // `index.html` and a 200.
    let Some(name) = path.strip_prefix('/').filter(|rest| !rest.starts_with('/')) else {
        return ApiError::NotFound.into_response();
    };
    if is_reserved(path) {
        return ApiError::NotFound.into_response();
    }
    let cache_control = if path.starts_with("/assets/") {
        IMMUTABLE
    } else {
        NO_CACHE
    };
    if !name.is_empty()
        && let Some(res) = respond(source, name, accept_encoding, cache_control)
    {
        return res;
    }
    respond(source, "index.html", accept_encoding, NO_CACHE)
        .unwrap_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, NOT_BUILT).into_response())
}

/// Paths the SPA must never answer for. Bare `/api` is here because it is not under `/api/`:
/// axum 0.8 wildcards need a character after the prefix, so `/api/{*rest}` misses it and it
/// would otherwise be served `index.html` with a 200. The health checks are real routes and
/// cannot reach this handler, but `serve_web` is public and must not contradict them.
///
/// `crate::public::owns_path` covers `/c`, `/u` and `/f` — the subscriber's server-rendered
/// pages, bare prefix included. That one is shared rather than spelled out twice, because for
/// those three the two ideas below really are one: see the reasoning on `owns_path`.
///
/// This happens to list the same paths as `security_headers::uncacheable`, which are two
/// different ideas that coincide — "never the SPA" and "never cached by a shared cache" — so
/// they are deliberately NOT one function. `security_headers`'s tests assert the two agree, so
/// a path added to only one of them fails loudly instead of drifting quietly.
pub(crate) fn is_reserved(path: &str) -> bool {
    path == "/api"
        || path.starts_with("/api/")
        || path == "/healthz"
        || path == "/readyz"
        || crate::public::owns_path(path)
}

/// The precompressed variant the client accepts, else the raw file; `None` if neither exists.
fn respond(
    source: &dyn AssetSource,
    name: &str,
    accept_encoding: Option<&str>,
    cache_control: &'static str,
) -> Option<Response> {
    // A precompressed variant is only ever reached through its base name, which is what
    // carries the Content-Encoding. Served directly it would be opaque bytes under
    // `application/octet-stream`, so the browser would save garbage instead of rendering it.
    if name.ends_with(".br") || name.ends_with(".gz") {
        return None;
    }
    let accept = accept_encoding.unwrap_or_default();
    let (data, encoding) = [("br", "br"), ("gzip", "gz")]
        .into_iter()
        .filter(|(coding, _)| accepts(accept, coding))
        .find_map(|(coding, extension)| {
            source
                .get(&format!("{name}.{extension}"))
                .map(|data| (data, Some(coding)))
        })
        .or_else(|| source.get(name).map(|data| (data, None)))?;

    // The type is the underlying file's: `app.js.br` is still JavaScript.
    let mut res = Response::builder()
        .header(
            header::CONTENT_TYPE,
            mime_guess::from_path(name).first_or_octet_stream().as_ref(),
        )
        .header(header::CACHE_CONTROL, cache_control)
        // Set even on the raw file: the same URL answers differently per Accept-Encoding,
        // and a shared cache that missed that would hand brotli to a client without it.
        .header(header::VARY, "accept-encoding");
    if let Some(coding) = encoding {
        res = res.header(header::CONTENT_ENCODING, coding);
    }
    let body = match data {
        Cow::Borrowed(bytes) => Bytes::from_static(bytes),
        Cow::Owned(bytes) => Bytes::from(bytes),
    };
    Some(res.body(Body::from(body)).expect("valid header values"))
}

/// True when `Accept-Encoding` offers `coding`. A token match, so `brotli` is not `br`.
fn accepts(accept_encoding: &str, coding: &str) -> bool {
    accept_encoding.split(',').any(|entry| {
        let mut parts = entry.split(';');
        let token = parts.next().unwrap_or_default().trim();
        token.eq_ignore_ascii_case(coding) && !parts.any(is_q_zero)
    })
}

/// `br;q=0` is a refusal, not an offer.
fn is_q_zero(parameter: &str) -> bool {
    parameter
        .trim()
        .to_ascii_lowercase()
        .strip_prefix("q=")
        .and_then(|q| q.trim().parse::<f32>().ok())
        .is_some_and(|q| q <= 0.0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    struct Map(HashMap<&'static str, &'static [u8]>);

    impl Map {
        fn new(entries: &[(&'static str, &'static [u8])]) -> Map {
            Map(entries.iter().copied().collect())
        }
    }

    impl AssetSource for Map {
        fn get(&self, path: &str) -> Option<Cow<'static, [u8]>> {
            self.0.get(path).copied().map(Cow::Borrowed)
        }
    }

    const INDEX: &[u8] = b"<!doctype html><div id=\"root\"></div>";

    fn header_of<'a>(res: &'a Response, name: &str) -> Option<&'a str> {
        res.headers().get(name).map(|v| v.to_str().expect("ascii"))
    }

    fn body_of(res: Response) -> Vec<u8> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(async {
                axum::body::to_bytes(res.into_body(), usize::MAX)
                    .await
                    .expect("body")
                    .to_vec()
            })
    }

    #[test]
    fn hashed_assets_are_immutable() {
        let src = Map::new(&[("assets/app-abc.js", b"console.log(1)")]);
        let res = serve_web(&src, "/assets/app-abc.js", None);
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(header_of(&res, "cache-control"), Some(IMMUTABLE));
        assert!(
            header_of(&res, "content-type")
                .expect("content-type")
                .contains("javascript")
        );
        assert_eq!(body_of(res), b"console.log(1)");
    }

    #[test]
    fn brotli_served_when_accepted() {
        let src = Map::new(&[
            ("assets/app.js", b"raw bytes"),
            ("assets/app.js.br", b"brotli bytes"),
        ]);
        let res = serve_web(&src, "/assets/app.js", Some("gzip, br"));
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(header_of(&res, "content-encoding"), Some("br"));
        assert_eq!(header_of(&res, "vary"), Some("accept-encoding"));
        assert!(
            header_of(&res, "content-type")
                .expect("content-type")
                .contains("javascript"),
            "the type is the underlying file's, not the .br wrapper's"
        );
        assert_eq!(body_of(res), b"brotli bytes");
    }

    #[test]
    fn raw_when_no_variant_or_not_accepted() {
        let both = Map::new(&[
            ("assets/app.js", b"raw bytes"),
            ("assets/app.js.br", b"brotli bytes"),
        ]);
        let res = serve_web(&both, "/assets/app.js", None);
        assert_eq!(header_of(&res, "content-encoding"), None);
        assert_eq!(body_of(res), b"raw bytes");

        let raw_only = Map::new(&[("assets/app.js", b"raw bytes")]);
        let res = serve_web(&raw_only, "/assets/app.js", Some("br, gzip"));
        assert_eq!(header_of(&res, "content-encoding"), None);
        assert_eq!(body_of(res), b"raw bytes");

        // `br;q=0` is a refusal, not an offer.
        let res = serve_web(&both, "/assets/app.js", Some("br;q=0"));
        assert_eq!(header_of(&res, "content-encoding"), None);
        assert_eq!(body_of(res), b"raw bytes");
    }

    #[test]
    fn spa_routes_get_index_with_no_cache() {
        let src = Map::new(&[("index.html", INDEX)]);
        let res = serve_web(&src, "/w/acme/members", None);
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(header_of(&res, "cache-control"), Some(NO_CACHE));
        assert_eq!(header_of(&res, "content-type"), Some("text/html"));
        assert_eq!(body_of(res), INDEX);
    }

    #[test]
    fn missing_build_is_503() {
        let res = serve_web(&Map::new(&[]), "/", None);
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_of(res), NOT_BUILT.as_bytes());
    }

    /// A precompressed file is served only through its base name, which supplies the
    /// `Content-Encoding`. Asked for directly it falls through to the SPA.
    #[test]
    fn precompressed_variants_are_not_served_directly() {
        let src = Map::new(&[
            ("assets/app.js", b"raw bytes"),
            ("assets/app.js.br", b"brotli bytes"),
            ("assets/app.js.gz", b"gzip bytes"),
            ("index.html", INDEX),
        ]);
        for path in ["/assets/app.js.br", "/assets/app.js.gz"] {
            let res = serve_web(&src, path, Some("br, gzip"));
            assert_eq!(res.status(), StatusCode::OK, "{path}");
            assert_eq!(header_of(&res, "content-type"), Some("text/html"), "{path}");
            assert_eq!(header_of(&res, "cache-control"), Some(NO_CACHE), "{path}");
            assert_eq!(header_of(&res, "content-encoding"), None, "{path}");
            assert_eq!(body_of(res), INDEX, "{path}");
        }
    }

    /// Ruling: bare `/api` is not under `/api/`, so the API's wildcard 404 route misses it.
    /// It must not be answered with `index.html`. Nor may a doubled slash smuggle one past
    /// `is_reserved`.
    #[test]
    fn api_paths_are_never_the_spa() {
        let src = Map::new(&[("index.html", INDEX)]);
        for path in ["/api", "/api/", "/api/v1/nope", "//api", "//api/v1/nope"] {
            let res = serve_web(&src, path, None);
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(
                header_of(&res, "content-type"),
                Some("application/problem+json"),
                "{path}"
            );
        }
    }
}
