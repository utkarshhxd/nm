// SPDX-License-Identifier: AGPL-3.0-only

//! The shell every public page is rendered into, and the escaper every value that reaches one
//! goes through.
//!
//! No templating crate. `Cargo.toml` here is curated and Task 13 runs `check-spdx.sh` and
//! `check:licenses` over the tree, so a dependency for five substitutions is a dependency with an
//! audit attached. What that costs is [`esc`]: a hand-written escaper, which is why it is tested
//! character by character rather than by inspection.
//!
//! These pages are server-rendered and work with JavaScript disabled, because they are reached
//! from an email client's browser and are the last chance a person has to leave a list. Nothing
//! here loads the SPA bundle.

use std::net::{IpAddr, Ipv4Addr};

use axum::{
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
};

/// Escapes the five characters that can end an attribute or open a tag. Every operator-supplied
/// value that reaches a page goes through this: the preference center Task 6 builds on this shell
/// renders list names an operator typed.
///
/// `'` is escaped as well as `"`, because a value interpolated into a single-quoted attribute is
/// as much of an opening as one in a double-quoted attribute, and nothing in a `format!` string
/// records which kind the author used. `&` is first by construction — it is a `match`, not a chain
/// of `replace` calls, so an entity this function writes can never be re-escaped by a later arm.
///
/// **What it is correct for: element text, and attribute values you quote.** Two contexts it does
/// not make safe, both of which the forms work in Task 9 will meet:
///
/// shortcut: an **unquoted** attribute value is still exploitable after this — space, tab,
/// newline, `/`, `=` and a backtick all end one, and none of the five is needed. Quote every
/// attribute (`title="{}"`, never `title={}`) and this holds; widen the escape set only if some
/// markup here ever stops quoting.
///
/// shortcut: this is **not a URL sanitiser**. `<a href="{}">` with `javascript:alert(1)` escapes
/// to itself and still runs on click. A value that becomes a URL — spec §7.3 gives a form an
/// operator-supplied `redirect_url` — needs its scheme checked against an allowlist before it
/// reaches a page; add that where the first such value appears, not here.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The tokens the pages use, copied from `web/src/styles/tokens.css` rather than imported: these
/// pages must render with no stylesheet request and no JavaScript, from an email client's browser
/// that may have neither.
///
/// The SPA chooses its theme with `data-theme`, written by `/theme.js` before first paint, and its
/// token file deliberately reads no `prefers-color-scheme` because two mechanisms would disagree
/// and flash. Here there is no script and so no second mechanism: the media query is the only one,
/// and it cannot disagree with anything.
const STYLE: &str = "\
:root{color-scheme:light;--canvas:oklch(0.985 0.003 265);--surface:oklch(1 0 0);\
--fg:oklch(0.23 0.018 265);--fg-muted:oklch(0.5 0.016 265);--border:oklch(0.905 0.006 265);\
--accent-text:oklch(0.45 0.19 295);--ring:oklch(0.53 0.195 295)}\
@media (prefers-color-scheme:dark){:root{color-scheme:dark;--canvas:oklch(0.165 0.01 265);\
--surface:oklch(0.205 0.011 265);--fg:oklch(0.95 0.004 265);--fg-muted:oklch(0.735 0.012 265);\
--border:oklch(0.305 0.013 265);--accent-text:oklch(0.8 0.105 295);--ring:oklch(0.72 0.155 295)}}\
body{margin:0;padding:2rem 1rem;background:var(--canvas);color:var(--fg);\
font:16px/1.6 system-ui,-apple-system,'Segoe UI',Roboto,sans-serif}\
main{max-width:34rem;margin:0 auto;padding:2rem;background:var(--surface);\
border:1px solid var(--border);border-radius:12px}\
h1{margin:0 0 1rem;font-size:1.5rem;line-height:1.3}\
p{margin:0 0 1rem}p:last-child{margin-bottom:0}\
.muted{color:var(--fg-muted);font-size:0.9375rem}\
a{color:var(--accent-text)}\
:focus-visible{outline:2px solid var(--ring);outline-offset:2px}\
button,input{font:inherit}";

/// Wraps `body` in the page. `title` is escaped here; **`body` is not** — it is HTML the caller
/// built, and every value it interpolated is the caller's to put through [`esc`] first.
///
/// `Cache-Control: no-store` is set here rather than left to `http::security_headers`, which
/// applies it to `/api*`, `/healthz` and `/readyz` only. These paths get nothing from it, and NM
/// is documented as running behind a reverse proxy: a preference center page naming which lists a
/// named person is on is exactly the 200 `text/html` with no `Set-Cookie` and no `Vary` that an
/// nginx `proxy_cache` will keep and hand to the next caller. Setting it in the shell means a page
/// added by a later task cannot forget it.
pub fn shell(title: &str, body: String) -> Response {
    let title = esc(title);
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"robots\" content=\"noindex\">\n<title>{title}</title>\n\
         <style>{STYLE}</style>\n</head>\n<body>\n<main>\n<h1>{title}</h1>\n{body}\n\
         </main>\n</body>\n</html>\n"
    );
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Html(html),
    )
        .into_response()
}

/// The bucket a request whose client address we cannot see is counted in. `RequestMeta` has no IP
/// only when the server was built without `ConnectInfo`, which production never is; one shared
/// bucket for that case limits them together rather than letting them past the limiter.
pub const UNKNOWN_IP: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

/// A page with a status: every answer a public route gives a browser is HTML, including the
/// refusals. A subscriber who clicks a dead link gets a sentence, not an RFC 9457 document.
pub fn notice(status: StatusCode, title: &str, body: &str) -> Response {
    (status, shell(title, body.to_owned())).into_response()
}

/// Puts the `RateLimit` headers on an answer. Every answer carries them, refusals included, as the
/// authenticated limited routes do.
///
/// **This is the headers and nothing else.** There is deliberately no shared "too many requests"
/// page here: `GET /c/{token}` and `GET /u/{token}` each write their own, because spec §8.9 says
/// `POST /f/{slug}` must answer a rate-limited submission exactly as a successful one and
/// `POST /u/{token}` must unsubscribe anyway. A refusal helper in this file is a refusal those two
/// routes could inherit by reaching for the obvious name.
pub fn with(headers: HeaderMap, page: Response) -> Response {
    (headers, page).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_escapes_each_of_the_five_characters() {
        assert_eq!(esc("&"), "&amp;");
        assert_eq!(esc("<"), "&lt;");
        assert_eq!(esc(">"), "&gt;");
        assert_eq!(esc("\""), "&quot;");
        assert_eq!(esc("'"), "&#39;");
    }

    /// `esc` being correct is worth nothing if `shell` stops calling it. Every title today is a
    /// literal, so only this test stands between Task 6 — which renders operator-typed list names
    /// through this shell — and an injected tag. Deleting `let title = esc(title);` reds it.
    #[tokio::test]
    async fn shell_escapes_its_title_in_both_places_it_puts_it() {
        let res = shell("<script>alert(1)</script>", "<p>body</p>".to_owned());
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!html.contains("<script>"), "the title opened a tag: {html}");
        assert!(
            html.contains("<title>&lt;script&gt;alert(1)&lt;/script&gt;</title>"),
            "{html}"
        );
        assert!(
            html.contains("<h1>&lt;script&gt;alert(1)&lt;/script&gt;</h1>"),
            "{html}"
        );
        // The body is the caller's HTML and is deliberately not escaped here.
        assert!(html.contains("<p>body</p>"));
    }

    #[test]
    fn esc_escapes_a_string_containing_all_five() {
        assert_eq!(
            esc("<a href='x' title=\"Bob & Co\">"),
            "&lt;a href=&#39;x&#39; title=&quot;Bob &amp; Co&quot;&gt;"
        );
        // Text with none of the five is returned unchanged, accents and all.
        assert_eq!(esc("Zoë's"), "Zoë&#39;s");
    }
}
