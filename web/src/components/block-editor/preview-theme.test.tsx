// SPDX-License-Identifier: AGPL-3.0-only

// The dark rewrite and what the preview says when it cannot do it. The rule: replace the feature,
// never negate the query, and when a gap is left, say so next to the frame.

import { type Page, expect } from "@playwright/test";

import { EDITOR, test } from "../../../e2e/support/app";
import { themed } from "./preview-theme";

const FRAME = "Email preview";
const mode = (page: Page, name: string) => page.getByRole("button", { name, exact: true }).click();
const frame = (page: Page) => page.frameLocator(`iframe[title="${FRAME}"]`);
const open = async (page: Page, app: string, query = "") => {
  await page.goto(app + EDITOR + query);
  await expect(page.getByRole("textbox", { name: "Text block 2", exact: true })).toBeVisible();
};

const SPELLINGS = [
  "@media (prefers-color-scheme: dark)",
  "@media screen and (prefers-color-scheme: dark)",
  "@media only screen and ( prefers-color-scheme : DARK ) and (max-width:480px)",
  "@media (min-width:1px) and (prefers-color-scheme: dark)",
  "@media (prefers-color-scheme: dark) and (max-width:480px)",
  "@import url(x) (prefers-color-scheme: dark);",
  '<style media="(prefers-color-scheme: dark)">',
];
const FEATURE = /\(\s*prefers-color-scheme\s*:\s*dark\s*\)/i;

test("the_rewrite_replaces_the_feature_in_every_spelling_and_never_negates_a_query", () => {
  for (const css of SPELLINGS) {
    const always = themed(css, true);
    const never = themed(css, false);
    expect(always.html, css).toBe(css.replace(FEATURE, "(min-width:0)"));
    expect(never.html, css).toBe(css.replace(FEATURE, "(max-width:0)"));
    expect(`${always.html}${never.html}`, css).not.toMatch(/\bnot\b/);
    expect([always.partial, never.partial, always.dark], css).toEqual([false, false, 1]);
  }
});

test("light_rules_are_inverted_the_same_way", () => {
  const css = "@media screen and (prefers-color-scheme: light)";
  expect(themed(css, true).html).toBe("@media screen and (max-width:0)");
  expect(themed(css, false).html).toBe("@media screen and (min-width:0)");
});

test("a_query_it_cannot_rewrite_is_reported_as_partial_not_passed_over", () => {
  for (const css of ["@media (prefers-color-scheme)", "@media (prefers-color-scheme: dark), (prefers-color-scheme)"]) {
    expect(themed(css, true).partial, css).toBe(true);
    expect(themed(css, false).partial, css).toBe(true);
  }
  expect(themed("<p>no queries</p>", true)).toEqual({ html: "<p>no queries</p>", dark: 0, partial: false });
});

const shade = (page: Page, id: string) => frame(page).locator(`#${id}`).evaluate((e) => getComputedStyle(e).backgroundColor);
const WHITE = "rgb(255, 255, 255)";
const BLACK = "rgb(0, 0, 0)";
const IDS = ["a", "b", "c", "d", "e"];

for (const os of ["light", "dark"] as const) {
  test(`every_spelling_follows_the_mode_and_never_the_operators_own_${os}_theme`, async ({ page, app }) => {
    await page.emulateMedia({ colorScheme: os });
    await open(page, app, "?preview=spellings");
    // a: "screen and"; b: "(min-width) and"; c: combined with max-width, which is false in a 640px frame;
    // d: <style media>; e: black unless the light rule applies.
    await mode(page, "Dark");
    await expect(frame(page).locator("#e")).toBeVisible();
    expect(await Promise.all(IDS.map((i) => shade(page, i))), "dark").toEqual([BLACK, BLACK, WHITE, BLACK, BLACK]);
    for (const m of ["Desktop", "Mobile"]) {
      await mode(page, m);
      await expect(frame(page).locator("#e")).toBeVisible();
      expect(await Promise.all(IDS.map((i) => shade(page, i))), m).toEqual([WHITE, WHITE, WHITE, WHITE, WHITE]);
    }
  });
}

test("a_query_the_rewrite_cannot_handle_says_so_next_to_the_frame", async ({ page, app }) => {
  await open(page, app, "?preview=leftover");
  for (const m of ["Desktop", "Dark"]) {
    await mode(page, m);
    await expect(page.getByTestId("preview-partial")).toContainText("could not be previewed");
  }
  await open(page, app);
  await expect(page.getByTestId("preview-partial")).toHaveCount(0);
});

test("a_template_with_no_dark_styles_says_the_dark_preview_proves_nothing", async ({ page, app }) => {
  await open(page, app, "?preview=nodark");
  await mode(page, "Dark");
  await expect(page.getByTestId("dark-none")).toContainText("no dark styles");
  await expect(page.getByTestId("dark-none")).toContainText("looks the same as Light");
  // The coverage label is still there, directly above the frame.
  await expect(page.getByTestId("dark-coverage")).toBeVisible();
  await mode(page, "Desktop");
  await expect(page.getByTestId("dark-none")).toHaveCount(0);
  // A template that has some does not carry the warning.
  await open(page, app);
  await mode(page, "Dark");
  await expect(page.getByTestId("dark-none")).toHaveCount(0);
});

test("recovery_after_a_failed_check_is_announced_in_a_live_region_that_was_already_there", async ({ page, app }) => {
  await page.goto(app + EDITOR + "?preview=flaky");
  await expect(page.getByRole("alert").filter({ hasText: "unavailable" })).toBeVisible();
  const status = page.getByRole("status");
  await expect(status).toHaveCount(1);
  await expect(status).toHaveText("");
  await page.evaluate(() => {
    (window as unknown as { __heal: boolean }).__heal = true;
    document.querySelector("[role=status]")?.setAttribute("data-same", "yes");
  });
  await page.getByRole("button", { name: "Add divider", exact: true }).click();
  await expect(page.getByRole("alert").filter({ hasText: "unavailable" })).toHaveCount(0);
  await expect(page.locator("[role=status][data-same=yes]")).toHaveText("No findings.");
});

test("before_the_first_result_nothing_claims_there_are_no_findings", async ({ page, app }) => {
  await page.goto(app + EDITOR + "?preview=slow");
  await expect(page.getByTestId("findings-count")).toHaveText("Checking…");
  await expect(page.getByRole("status")).toHaveText("");
  await expect(page.getByText("No findings")).toHaveCount(0);
  await expect(page.getByRole("region", { name: "Preview and checks" })).toHaveAttribute("aria-busy", "true");
});
