// SPDX-License-Identifier: AGPL-3.0-only

/**
 * What a dark preview can honestly reproduce: the author's own `prefers-color-scheme` CSS.
 *
 * Replace the FEATURE, never the query. `(prefers-color-scheme: dark)` becomes `(min-width:0)`
 * (always true) or `(max-width:0)` (never true), so whatever surrounds it (`screen and`, a
 * combined `and (max-width:480px)`, an `@import` prelude, a `<style media>` attribute) keeps its
 * own meaning. Prefixing `not` would negate the whole query instead, and the light preview would
 * then show dark styles. The frame then shows the same thing whatever the operator's own system
 * theme is. `light` is handled the same way, inverted.
 *
 * A regex over CSS has gaps (`(prefers-color-scheme)` with no value, a comment inside the query).
 * Rather than claim none, `partial` says whether the string is still there after the rewrite, and
 * the panel then says some dark styles could not be previewed.
 *
 * There is no colour inversion anywhere: a client that recolours the email itself cannot be shown
 * by a browser, and a guess would be a lie.
 */
const FEATURE = /\(\s*prefers-color-scheme\s*:\s*(dark|light)\s*\)/gi;

export function themed(html: string, dark: boolean): { html: string; dark: number; partial: boolean } {
  let rules = 0;
  const out = html.replace(FEATURE, (_, scheme: string) => {
    const isDark = scheme.toLowerCase() === "dark";
    if (isDark) rules++;
    return isDark === dark ? "(min-width:0)" : "(max-width:0)";
  });
  return { html: out, dark: rules, partial: /prefers-color-scheme/i.test(out) };
}
