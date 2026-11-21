// SPDX-License-Identifier: AGPL-3.0-only

import type AxeBuilder from "@axe-core/playwright";
import { expect } from "@playwright/test";

/**
 * Every axe scan in this suite goes through here, because the one thing that can go wrong with a
 * scan is that it checked nothing.
 *
 * `passes.length === 0` means axe ran no rules, and a run with no rules has no violations to
 * report: it passes every page and every dialog, silently, for as long as nobody looks. That is
 * not hypothetical — seven call sites across six files could do it before this helper existed,
 * one of them five lines from a call that was guarded. A vacuous assertion never fails, so
 * nothing ever tells you it stopped checking.
 *
 * To put a scan into that state deliberately — which is how you prove a test that uses this
 * still discriminates — restrict the builder with `withTags(["no-such-tag"])`: a valid `runOnly`
 * that matches no rule, so axe runs none and returns empty. `withRules([])` does NOT work; axe
 * rejects an empty `runOnly` outright with "runOnly.values must be a non-empty array".
 *
 * Takes a builder the caller has already configured, so a scan of another page or a narrowed
 * `include` needs nothing from here.
 */
export async function axeProblems(
  builder: AxeBuilder,
  label: string,
  options: AxeOptions = {},
): Promise<string[]> {
  const { violations, passes, incomplete } = await builder.analyze();
  const found = passes.length === 0 ? [`${label}: axe ran no rules`] : [];
  // THE THIRD BUCKET, and the one that reads as a pass. `incomplete` is where a rule lands when
  // it ran and **could not decide** — a colour it could not resolve, an element it could not
  // measure. It is neither a violation nor a pass, so a scan whose one rule you were counting on
  // landed here reports clean and tells you nothing. `passes.length === 0` above does not catch
  // it: other rules passed, so the scan looks alive.
  //
  // Printed rather than failed, because a caller cannot know in advance which of its pages has a
  // genuinely undecidable case, and a shared helper that turned every one of them red would be
  // turned off. `requireRules` is how a caller says "this rule had to have actually passed here".
  //
  // What this suite actually has, measured 2026-11-20 when the bucket was first looked at: the
  // public pages produce **none** across 20 scans, and `theme.spec.ts`'s sample page produces
  // `color-contrast×4` in each theme — four decorative `span[aria-hidden="true"]` badge glyphs,
  // every one of them "Element content contains only non-text characters". That is axe declining
  // to judge the contrast of something that is not text, which is correct, and not the failure
  // worth worrying about. The one worth worrying about would be a colour it could not resolve:
  // these pages are written entirely in `oklch()`, and `color-contrast` is in `passes` by name on
  // every one of the 20, so axe-core 4.14.0 resolves that colour space fine.
  if (incomplete.length > 0) {
    const what = incomplete.map((rule) => `${rule.id}×${rule.nodes.length}`).join(", ");
    console.log(`${label}: axe could not decide: ${what}`);
  }
  for (const id of options.requireRules ?? []) {
    if (passes.some((rule) => rule.id === id)) continue;
    const why = incomplete.some((rule) => rule.id === id)
      ? "it could not decide"
      : violations.some((rule) => rule.id === id)
        ? "it failed"
        : "the rule did not run, or matched no element";
    found.push(`${label}: axe did not pass ${id}: ${why}`);
  }
  for (const v of violations) {
    if (options.seriousOnly && v.impact !== "serious" && v.impact !== "critical") continue;
    const where = v.nodes.map((n) => n.target.join(" ")).join(", ");
    found.push(`${label}: axe ${v.id} (${v.impact}): ${where}`);
  }
  return found;
}

export type AxeOptions = {
  /**
   * Only `serious` and `critical`. The M0 screens were signed off with lesser ones outstanding;
   * everything since is held to all of them, so this is a statement about those screens' history
   * and not a default anyone should reach for.
   */
  seriousOnly?: boolean;
  /**
   * Rule ids that must appear in `passes` — not merely "not violated".
   *
   * For the rule a page's evidence actually rests on. `color-contrast` is the example: it is the
   * rule behind every "axe is clean" claim about colour, and it is also the rule most able to
   * land in `incomplete`, because it has to resolve a computed colour against whatever is behind
   * it. Naming it here is the difference between "axe found nothing" and "axe looked".
   */
  requireRules?: string[];
};

/** `axeProblems`, asserted. Use this unless the caller is collecting problems to report together. */
export async function expectNoViolations(
  builder: AxeBuilder,
  label: string,
  options: AxeOptions = {},
): Promise<void> {
  expect(await axeProblems(builder, label, options), label).toEqual([]);
}
