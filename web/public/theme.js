// SPDX-License-Identifier: AGPL-3.0-only

// Sets <html data-theme> before first paint. It is a blocking <script src> in index.html's <head>
// rather than an inline script because the CSP (src/http/security_headers.rs) has no 'unsafe-inline'
// for scripts. No import, no framework: it must run before anything else does.
(function () {
  var theme = null;
  try {
    theme = localStorage.getItem("theme");
  } catch (_) {
    // Storage blocked: fall through to the OS preference.
  }
  if (theme !== "light" && theme !== "dark") {
    theme = matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  document.documentElement.dataset.theme = theme;
})();
