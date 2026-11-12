// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import type { Page } from "@playwright/test";

import { SAMPLE, expect, test } from "./support/app";
import { expectNoViolations } from "./support/axe";

type Theme = "light" | "dark";
type Frame = { theme: string | undefined; background: string };

/** Chooses the theme the way a returning visitor has: a stored choice, whatever the OS says. */
async function stored(page: Page, theme: Theme): Promise<void> {
  await page.addInitScript((t) => localStorage.setItem("theme", t), theme);
}

/**
 * Records `<html data-theme>` and the computed `<body>` background at the first animation frames
 * of the document, i.e. what the user can actually see before anything has had a chance to
 * correct it. A flash of the wrong theme is exactly a first frame that disagrees with the last.
 */
async function recordFrames(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const frames: Frame[] = [];
    (window as unknown as { __frames: Frame[] }).__frames = frames;
    const sample = () => {
      frames.push({
        theme: document.documentElement.dataset["theme"],
        background: document.body ? getComputedStyle(document.body).backgroundColor : "(no body)",
      });
      if (frames.length < 4) requestAnimationFrame(sample);
    };
    requestAnimationFrame(sample);
  });
}

async function frames(page: Page): Promise<Frame[]> {
  await page.waitForFunction(() => (window as unknown as { __frames: Frame[] }).__frames.length >= 4);
  return page.evaluate(() => (window as unknown as { __frames: Frame[] }).__frames);
}

test("dark_mode_applies_before_first_paint", async ({ browser, app }) => {
  const seen: Record<string, string> = {};
  const cases: { name: string; colorScheme: Theme; stored: Theme | null }[] = [
    // An explicit choice beats the operating system, both ways round.
    { name: "stored dark, OS light", colorScheme: "light", stored: "dark" },
    { name: "stored light, OS dark", colorScheme: "dark", stored: "light" },
    // No stored choice: the OS decides.
    { name: "no choice, OS dark", colorScheme: "dark", stored: null },
  ];
  for (const c of cases) {
    const context = await browser.newContext({ colorScheme: c.colorScheme });
    const page = await context.newPage();
    if (c.stored) await stored(page, c.stored);
    await recordFrames(page);
    // The real index.html: its own head decides the theme, not the test.
    await page.goto(`${app}/`);
    const wanted = c.stored ?? c.colorScheme;

    const first = await frames(page);
    await page.waitForLoadState("load");
    const settled = await page.evaluate(() => getComputedStyle(document.body).backgroundColor);

    // Every one of the first frames is already in the final theme: no flash.
    for (const [index, frame] of first.entries()) {
      expect(frame, `${c.name}, frame ${index}`).toEqual({ theme: wanted, background: settled });
    }
    seen[wanted] = settled;
    await context.close();
  }
  // And the two themes really are different backgrounds, so the assertion above has teeth.
  expect(seen["dark"]).not.toBe(seen["light"]);
});

/**
 * `@theme inline` over `:root` variables is what lets a theme be scoped to a subtree. With the
 * attribute on <html> a plain `@theme` behaves identically, which is exactly why this test is the
 * only thing that notices the difference. See the comment above `@theme inline` in tokens.css.
 */
test("a_theme_can_be_scoped_to_a_subtree", async ({ page, app }) => {
  await stored(page, "light");
  await page.goto(app + SAMPLE);
  const colours = await page.evaluate(() => {
    const el = document.createElement("div");
    el.className = "bg-surface";
    document.body.append(el);
    const bg = () => getComputedStyle(el).backgroundColor;
    const light = bg();
    el.dataset["theme"] = "dark";
    const nested = bg();
    delete el.dataset["theme"];
    document.documentElement.dataset["theme"] = "dark";
    const wholePage = bg();
    return { light, nested, wholePage };
  });
  expect(colours.nested).not.toBe(colours.light);
  expect(colours.nested).toBe(colours.wholePage);
});

test("both_themes_pass_axe_on_a_sample_page", async ({ browser, app }) => {
  for (const theme of ["light", "dark"] as const) {
    // The OS says the opposite, so only the stored choice can produce the right theme.
    const context = await browser.newContext({ colorScheme: theme === "dark" ? "light" : "dark" });
    const page = await context.newPage();
    await stored(page, theme);
    await page.goto(app + SAMPLE);
    await expect(page.locator("html")).toHaveAttribute("data-theme", theme);

    await expectNoViolations(new AxeBuilder({ page }), `${theme}, dialog closed`);

    await page.getByRole("button", { name: "Invite someone" }).click();
    await expect(page.getByRole("dialog", { name: "Invite someone" })).toBeVisible();
    await expectNoViolations(new AxeBuilder({ page }), `${theme}, dialog open`);
    await context.close();
  }
});
