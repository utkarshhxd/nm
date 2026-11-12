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
  const { violations, passes } = await builder.analyze();
  const found = passes.length === 0 ? [`${label}: axe ran no rules`] : [];
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
};

/** `axeProblems`, asserted. Use this unless the caller is collecting problems to report together. */
export async function expectNoViolations(
  builder: AxeBuilder,
  label: string,
  options: AxeOptions = {},
): Promise<void> {
  expect(await axeProblems(builder, label, options), label).toEqual([]);
}
