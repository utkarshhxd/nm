// SPDX-License-Identifier: AGPL-3.0-only

// Runs in a real browser, not jsdom: the focus trap, `inert` and Escape handling this asserts
// are what the browser does for a modal `<dialog>`, and jsdom implements none of them.

import { type Page, expect } from "@playwright/test";

import { SAMPLE, test } from "../../e2e/support/app";

async function openDialog(page: Page, app: string) {
  await page.goto(app + SAMPLE);
  const trigger = page.getByRole("button", { name: "Invite someone" });
  await trigger.focus();
  await trigger.click();
  const dialog = page.getByRole("dialog", { name: "Invite someone" });
  await expect(dialog).toBeVisible();
  return { trigger, dialog };
}

test("closes_on_escape", async ({ page, app }) => {
  const { dialog } = await openDialog(page, app);
  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
  expect(await page.evaluate(() => document.querySelector("dialog")?.open)).toBe(false);
});

test("traps_focus_while_open", async ({ page, app }) => {
  const { dialog } = await openDialog(page, app);
  // The page behind has its own focusable controls; none may ever be reached.
  const outside = () =>
    page.evaluate(() => {
      const active = document.activeElement;
      return active !== document.body && !document.querySelector("dialog")?.contains(active);
    });
  for (let press = 0; press < 12; press++) {
    await page.keyboard.press("Tab");
    expect(await outside(), `Tab #${press + 1} left the dialog`).toBe(false);
  }
  for (let press = 0; press < 12; press++) {
    await page.keyboard.press("Shift+Tab");
    expect(await outside(), `Shift+Tab #${press + 1} left the dialog`).toBe(false);
  }
  // And a script cannot drag focus out either: the rest of the page is inert.
  await page.evaluate(() => document.querySelector<HTMLElement>("#page-search")?.focus());
  expect(await outside()).toBe(false);
  await expect(dialog).toBeVisible();
});

test("returns_focus_to_the_trigger_on_close", async ({ page, app }) => {
  const { trigger, dialog } = await openDialog(page, app);
  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
  await expect(trigger).toBeFocused();

  // The other way out, the dialog's own Cancel button, must land in the same place.
  await trigger.click();
  await expect(dialog).toBeVisible();
  await dialog.getByRole("button", { name: "Cancel" }).click();
  await expect(dialog).toBeHidden();
  await expect(trigger).toBeFocused();
});
