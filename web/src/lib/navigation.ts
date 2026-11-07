// SPDX-License-Identifier: AGPL-3.0-only

/**
 * Where `?next=…` of `href` should send someone once they have signed in, as a path on this
 * origin. Anything else — another origin, another scheme, or a value the URL parser rejects —
 * becomes `/`.
 *
 * **Ask the parser; do not reason about strings.** The candidate is resolved with the same URL
 * parser that `location.assign` will use, and the *result* is then resolved again and checked.
 * Two different bugs make both steps necessary:
 *
 * 1. The parser removes every ASCII tab, CR and LF before it resolves anything, so
 *    `/<tab>/evil.example` is a string that looks same-origin to any `startsWith` test and
 *    parses as `//evil.example`. Only comparing parsed origins catches that.
 * 2. A genuinely same-origin URL can carry a **pathname that is itself protocol-relative**.
 *    `https://nm.example//evil.example`, `//nm.example//evil.example` and `/..//evil.example`
 *    all pass an origin comparison honestly — the host really is this origin at that point —
 *    and leave a pathname of `//evil.example`, which `location.assign` then reads as
 *    protocol-relative and follows off-site. `/..//evil.example` names no host at all, so one
 *    crafted link works against every deployment.
 *
 * Hence the fixed point: whatever the first parse normalises the value into, the second parse
 * sees exactly what `location.assign` will see. That subsumes a `startsWith("//")` test and
 * covers normalisations nobody here has thought of — which is the point, because this has now
 * been got wrong twice by reasoning about which strings the parser rejects.
 *
 * A `javascript:` URL parses to the origin `"null"` and is refused by the same comparison.
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
    if (target.origin !== here.origin) return "/";
    const path = `${target.pathname}${target.search}${target.hash}`;
    return new URL(path, here.origin).origin === here.origin ? path : "/";
  } catch {
    return "/";
  }
}
