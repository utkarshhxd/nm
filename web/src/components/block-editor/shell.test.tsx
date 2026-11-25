// SPDX-License-Identifier: AGPL-3.0-only

// The editor shell in a real browser: blocks render from state, every change goes through an op,
// and undo walks the document stack. The editing surfaces themselves are a later task's tests.

import AxeBuilder from "@axe-core/playwright";
import { type Page, expect } from "@playwright/test";

import { EDITOR, test } from "../../../e2e/support/app";
import { expectNoViolations } from "../../../e2e/support/axe";
import type { Doc } from "./model";

const doc = (page: Page) => page.evaluate(() => (window as unknown as { __doc: Doc }).__doc);
const kinds = async (page: Page) => (await doc(page)).blocks.map((b) => b.type);

test("renders_every_block_from_state_with_a_tag_as_one_token", async ({ page, app }) => {
  await page.goto(app + EDITOR);
  await expect(page.getByRole("button", { name: "Select Heading block 1" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Select Text block 2" })).toBeVisible();
  // Nested blocks are addressed by their path, and are rendered inside their column.
  const column = page.getByRole("group", { name: "Column 2 of Columns block 3" });
  await expect(column.getByRole("button", { name: "Select Button block 3.2.1" })).toBeVisible();
  await expect(page.locator("[data-tag]")).toHaveText('{{ first_name | "there" }}');
  await expectNoViolations(new AxeBuilder({ page }), "block editor shell");
});

test("one_block_is_selected_at_a_time_and_only_it_has_controls", async ({ page, app }) => {
  await page.goto(app + EDITOR);
  await page.getByRole("button", { name: "Select Heading block 1" }).click();
  await page.getByRole("button", { name: "Select Text block 2" }).click();
  await expect(page.getByRole("button", { name: "Select Heading block 1" })).toHaveAttribute("aria-pressed", "false");
  await expect(page.getByRole("button", { name: "Delete block" })).toHaveCount(1);
});

test("move_delete_and_insert_edit_the_document_and_undo_restores_it", async ({ page, app }) => {
  await page.goto(app + EDITOR);
  await page.getByRole("button", { name: "Select Heading block 1" }).click();
  await expect(page.getByRole("button", { name: "Move up" })).toBeDisabled();
  await page.getByRole("button", { name: "Move down" }).click();
  expect(await kinds(page)).toEqual(["Text", "Heading", "Columns"]);

  // Selection followed the moved block, so the next press moves the same one.
  await page.getByRole("button", { name: "Move down" }).click();
  expect(await kinds(page)).toEqual(["Text", "Columns", "Heading"]);
  await expect(page.getByRole("button", { name: "Move down" })).toBeDisabled();

  await page.getByRole("button", { name: "Add divider" }).click();
  expect(await kinds(page)).toEqual(["Text", "Columns", "Heading", "Divider"]);

  await page.getByRole("button", { name: "Delete block" }).click();
  expect(await kinds(page)).toEqual(["Text", "Columns", "Heading"]);

  for (let i = 0; i < 4; i++) await page.getByRole("button", { name: "Undo" }).click();
  expect(await kinds(page)).toEqual(["Heading", "Text", "Columns"]);
  await expect(page.getByRole("button", { name: "Undo" })).toBeDisabled();
  await page.getByRole("button", { name: "Redo" }).click();
  expect(await kinds(page)).toEqual(["Text", "Heading", "Columns"]);
});

test("a_nested_block_moves_only_inside_its_own_column", async ({ page, app }) => {
  await page.goto(app + EDITOR);
  await page.getByRole("button", { name: "Select Text block 3.1.1" }).click();
  // The only block in its column: both directions are closed, not half-offered.
  await expect(page.getByRole("button", { name: "Move up" })).toBeDisabled();
  await expect(page.getByRole("button", { name: "Move down" })).toBeDisabled();
  await page.getByRole("button", { name: "Delete block" }).click();
  const columns = (await doc(page)).blocks[2];
  expect(columns).toMatchObject({ type: "Columns", children: [[], [{ type: "Button" }]] });
});
