// SPDX-License-Identifier: AGPL-3.0-only

/**
 * Where `?next=…` of `href` should send someone once they have signed in, as a path on this
 * origin. Anything else — another origin, another scheme, or a value the URL parser rejects —
 * becomes `/`.
 *
 * The check runs the candidate through the **same parser that will perform the navigation**,
 * rather than pattern-matching the string. That is not a style preference: the WHATWG URL
 * parser removes every ASCII tab, CR and LF before it does anything else, so `/<tab>/evil.example`
 * is a string that looks same-origin to any `startsWith` test and then parses as
 * `//evil.example`. Comparing parsed origins is the only way to see what the browser will
 * actually do. A `javascript:` URL parses to the origin `"null"` and is rejected by the same
 * comparison.
 *
 * This is the one place the post-sign-in destination is decided, for the password form, the
 * second factor and both passkey paths alike. `web/e2e/next-path.spec.ts` holds its tests.
 */
export function nextPath(href: string): string {
  const here = new URL(href);
  const next = here.searchParams.get("next");
  if (!next) return "/";
  try {
    const target = new URL(next, here.origin);
    return target.origin === here.origin
      ? `${target.pathname}${target.search}${target.hash}`
      : "/";
  } catch {
    return "/";
  }
}
