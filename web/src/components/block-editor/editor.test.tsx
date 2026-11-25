// SPDX-License-Identifier: AGPL-3.0-only

// The editing surfaces in a real browser. A contenteditable, a caret and a pointer drag are what
// jsdom does not implement, so these run against the built fixture in e2e/support/editor.

import AxeBuilder from "@axe-core/playwright";
import { type Locator, type Page, expect } from "@playwright/test";

import { EDITOR, test } from "../../../e2e/support/app";
import { expectNoViolations } from "../../../e2e/support/axe";
import { PAUSE } from "./editor";
import type { Block, Doc } from "./model";

const doc = (page: Page) => page.evaluate(() => (window as unknown as { __doc: Doc }).__doc);
const block = async (page: Page, i: number) => (await doc(page)).blocks[i] as Block;
const words = (b: Block) =>
  b.type === "Text" || b.type === "Heading" ? b.runs.map((r) => (r.type === "Tag" ? "{tag}" : r.text)).join("") : b.type;
const box = (page: Page, name: string) => page.getByRole("textbox", { name, exact: true });
// Clicks in the empty space at the end of the block, so the caret lands in the text and not on a tag.
const caretIn = async (page: Page, name: string) => {
  const b = box(page, name);
  const size = (await b.boundingBox()) as NonNullable<Awaited<ReturnType<Locator["boundingBox"]>>>;
  await b.click({ position: { x: size.width - 4, y: 6 } });
};
const open = async (page: Page, app: string) => {
  await page.goto(app + EDITOR);
  await expect(box(page, "Text block 2")).toBeVisible();
};
const tagCount = async (page: Page) => {
  const b = await block(page, 1);
  return b.type === "Text" ? b.runs.filter((r) => r.type === "Tag").length : -1;
};

test("text_and_heading_edit_in_place_and_the_heading_level_is_in_the_panel", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Heading block 1");
  await page.keyboard.press("End");
  await page.keyboard.type(" back");
  expect(words(await block(page, 0))).toBe("Welcome back");
  await page.getByLabel("Heading level").selectOption("3");
  expect(await block(page, 0)).toMatchObject({ type: "Heading", level: 3 });
  // Enter does not put a line break in a one-paragraph block.
  await page.keyboard.press("Enter");
  expect(words(await block(page, 0))).toBe("Welcome back");
});

test("button_image_spacer_divider_and_columns_each_edit", async ({ page, app }) => {
  await open(page, app);
  // Button: the label is edited in place, the address in the panel, and a bad scheme is called out.
  await page.getByRole("textbox", { name: "Button block 3.2.1 label" }).fill("Learn more");
  await page.getByLabel("Link address").fill("javascript:alert(1)");
  await expect(page.getByText("Use an address that starts with http")).toBeVisible();
  await page.getByLabel("Link address").fill("https://example.com/a");
  await expect(page.getByText("Use an address that starts with http")).toHaveCount(0);
  const cols = (await block(page, 2)) as Extract<Block, { type: "Columns" }>;
  expect(cols.children[1]?.[0]).toEqual({ type: "Button", label: "Learn more", href: "https://example.com/a" });

  await page.getByRole("button", { name: "Select Columns block 3" }).click();
  await page.getByRole("button", { name: "Add image" }).click();
  await page.getByLabel("Image address").fill("https://example.com/i.png");
  await page.getByLabel("Description").fill("A lighthouse");
  expect(await block(page, 3)).toEqual({ type: "Image", src: "https://example.com/i.png", alt: "A lighthouse" });
  await expect(page.getByText("A lighthouse")).toBeVisible();

  await page.getByRole("button", { name: "Add spacer" }).click();
  await page.getByLabel("Height in pixels").fill("48");
  expect(await block(page, 4)).toEqual({ type: "Spacer", height: 48 });

  await page.getByRole("button", { name: "Add divider" }).click();
  await expect(page.getByText("A divider has no properties.")).toBeVisible();
  expect((await doc(page)).blocks.map((b) => b.type)).toEqual(["Heading", "Text", "Columns", "Image", "Spacer", "Divider"]);
});

test("columns_grow_and_shrink_without_dropping_a_block", async ({ page, app }) => {
  await open(page, app);
  await page.getByRole("button", { name: "Select Columns block 3" }).click();
  await page.getByRole("button", { name: "Add column", exact: true }).click();
  await expect(page.getByText("3 columns")).toBeVisible();
  await page.getByLabel("Block to add").selectOption("Divider");
  await page.getByRole("button", { name: "Add to column 3" }).click();
  await page.getByRole("button", { name: "Select Columns block 3" }).click();
  expect(((await block(page, 2)) as Extract<Block, { type: "Columns" }>).children[2]).toEqual([{ type: "Divider" }]);
  // The last column now has a block in it, so it cannot be removed and take the block with it.
  await expect(page.getByRole("button", { name: "Remove last column" })).toBeDisabled();
  await page.getByRole("button", { name: "Select Divider block 3.3.1" }).click();
  await page.getByRole("button", { name: "Delete block" }).click();
  await page.getByRole("button", { name: "Select Columns block 3" }).click();
  await page.getByRole("button", { name: "Remove last column" }).click();
  await expect(page.getByText("2 columns")).toBeVisible();
});

test("a_merge_tag_deletes_whole_with_backspace_and_is_restored_by_one_undo", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("End");
  await page.keyboard.press("ArrowLeft"); // before the "!", so right after the tag
  await page.keyboard.press("Backspace");
  expect(await tagCount(page)).toBe(0);
  expect(words(await block(page, 1))).toBe("Hi !");
  await page.getByRole("button", { name: "Undo" }).click();
  expect(await tagCount(page)).toBe(1);
  expect(words(await block(page, 1))).toBe("Hi {tag}!");
});

test("deleting_a_merge_tag_is_its_own_undo_step_even_straight_after_typing", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("End");
  await page.keyboard.press("ArrowLeft");
  await page.keyboard.type("a");
  await page.keyboard.press("ArrowLeft");
  await page.keyboard.press("Backspace");
  expect(words(await block(page, 1))).toBe("Hi a!");
  // One Undo brings back the tag and only the tag: the typed "a" is a step of its own.
  await page.getByRole("button", { name: "Undo" }).click();
  expect(words(await block(page, 1))).toBe("Hi {tag}a!");
});

test("a_merge_tag_deletes_whole_with_forward_delete_too", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("Home");
  for (let i = 0; i < 3; i++) await page.keyboard.press("ArrowRight"); // past "Hi ", before the tag
  await page.keyboard.press("Delete");
  expect(words(await block(page, 1))).toBe("Hi !");
});

test("selecting_a_merge_tag_and_deleting_removes_all_of_it_and_nothing_else", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("End");
  await page.keyboard.press("ArrowLeft");
  await page.keyboard.press("Shift+ArrowLeft");
  await page.keyboard.press("Backspace");
  expect(words(await block(page, 1))).toBe("Hi !");
  expect(await tagCount(page)).toBe(0);
});

test("typing_beside_a_merge_tag_never_changes_it", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("Home");
  for (let i = 0; i < 3; i++) await page.keyboard.press("ArrowRight");
  await page.keyboard.type("dear ");
  await page.keyboard.press("ArrowRight"); // over the tag in one step
  await page.keyboard.type("?");
  expect(words(await block(page, 1))).toBe("Hi dear {tag}?!");
  expect(((await block(page, 1)) as Extract<Block, { type: "Text" }>).runs.find((r) => r.type === "Tag")).toEqual({
    type: "Tag",
    source: '{{ first_name | "there" }}',
  });
});

test("the_merge_tag_field_puts_a_tag_at_the_caret_and_the_caret_follows_it", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("Home");
  for (let i = 0; i < 2; i++) await page.keyboard.press("ArrowRight"); // "Hi|"
  await page.getByLabel("Merge tag").fill("last_name");
  await page.getByRole("button", { name: "Insert merge tag" }).click();
  const runs = ((await block(page, 1)) as Extract<Block, { type: "Text" }>).runs;
  expect(runs.map((r) => (r.type === "Tag" ? r.source : r.text))).toEqual(["Hi", "{{ last_name }}", " ", '{{ first_name | "there" }}', "!"]);
  await page.keyboard.type("x");
  expect(words(await block(page, 1))).toBe("Hi{tag}x {tag}!");
  await page.getByLabel("Merge tag").fill("{{ }}");
  await page.getByRole("button", { name: "Insert merge tag" }).click();
  await expect(page.getByText("Enter a merge tag")).toBeVisible();
});

test("bold_and_italic_apply_together_to_the_selection_and_come_off_again", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("Home");
  await page.keyboard.press("Shift+ArrowRight");
  await page.keyboard.press("Shift+ArrowRight"); // "Hi"
  const bold = page.getByRole("button", { name: "Bold" });
  const italic = page.getByRole("button", { name: "Italic" });
  await expect(bold).toBeEnabled();
  await bold.click();
  await italic.click();
  await expect(bold).toHaveAttribute("aria-pressed", "true");
  const marks = async () => {
    const b = (await block(page, 1)) as Extract<Block, { type: "Text" }>;
    return b.runs.map((r) => (r.type === "Tag" ? "tag" : `${r.text}:${[...r.marks].sort().join("+")}`));
  };
  expect(await marks()).toEqual(["Hi:bold+italic", " :", "tag", "!:bold"]);
  await bold.click();
  expect(await marks()).toEqual(["Hi:italic", " :", "tag", "!:bold"]);
  // The selection survived the repaint, so the next keystroke is still about "Hi".
  await page.keyboard.press("Control+b");
  expect((await marks())[0]).toBe("Hi:bold+italic");
});

test("a_link_takes_an_allowed_scheme_only", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("Home");
  await page.keyboard.press("Shift+ArrowRight");
  await page.keyboard.press("Shift+ArrowRight");
  const address = page.getByLabel("Link address");
  await address.fill("javascript:alert(1)");
  await page.getByRole("button", { name: "Apply link" }).click();
  await expect(page.getByText("Use an address that starts with http")).toBeVisible();
  expect(JSON.stringify(await block(page, 1))).not.toContain("Link");
  await address.fill("https://example.com");
  await page.getByRole("button", { name: "Apply link" }).click();
  expect(((await block(page, 1)) as Extract<Block, { type: "Text" }>).runs[0]).toEqual({
    type: "Link",
    href: "https://example.com",
    text: "Hi",
    marks: [],
  });
  await page.getByRole("button", { name: "Remove link" }).click();
  expect(JSON.stringify(await block(page, 1))).not.toContain("Link");
});

test("a_drag_moves_a_block_and_a_drag_that_ends_where_it_began_moves_nothing", async ({ page, app }) => {
  await open(page, app);
  const rows = page.locator('[data-list=""]');
  type Box = { x: number; y: number; width: number; height: number };
  const drag = async (handle: Locator, toY: (r: Box) => number, target: Locator) => {
    const h = (await handle.boundingBox()) as NonNullable<Awaited<ReturnType<Locator["boundingBox"]>>>;
    const t = (await target.boundingBox()) as typeof h;
    await page.mouse.move(h.x + h.width / 2, h.y + h.height / 2);
    await page.mouse.down();
    await page.mouse.move(t.x + 20, toY(t), { steps: 8 });
    await page.mouse.up();
  };
  const kinds = async () => (await doc(page)).blocks.map((b) => b.type);

  await drag(page.getByRole("button", { name: "Drag Heading block 1 to reorder" }), (r) => r.y + r.height - 2, rows.nth(1));
  expect(await kinds()).toEqual(["Text", "Heading", "Columns"]);
  await drag(page.getByRole("button", { name: "Drag Columns block 3 to reorder" }), (r) => r.y + 2, rows.nth(0));
  expect(await kinds()).toEqual(["Columns", "Text", "Heading"]);

  // Back to where it began: nothing is committed, so one Undo still reaches the previous move.
  await drag(page.getByRole("button", { name: "Drag Columns block 1 to reorder" }), (r) => r.y + 2, rows.nth(0));
  expect(await kinds()).toEqual(["Columns", "Text", "Heading"]);
  await page.getByRole("button", { name: "Undo" }).click();
  expect(await kinds()).toEqual(["Text", "Heading", "Columns"]);
});

test("a_drag_inside_a_column_stays_in_that_column", async ({ page, app }) => {
  await open(page, app);
  await page.getByRole("button", { name: "Select Columns block 3" }).click();
  await page.getByRole("button", { name: "Add to column 1" }).click();
  const col = page.getByRole("group", { name: "Column 1 of Columns block 3" });
  const first = col.getByRole("button", { name: "Drag Text block 3.1.1 to reorder" });
  const second = col.locator('[data-list="2,0"]').nth(1);
  const h = (await first.boundingBox()) as NonNullable<Awaited<ReturnType<Locator["boundingBox"]>>>;
  const t = (await second.boundingBox()) as typeof h;
  await page.mouse.move(h.x + h.width / 2, h.y + h.height / 2);
  await page.mouse.down();
  // Past the column's end and the whole document: only the column's own rows count.
  await page.mouse.move(t.x + 10, t.y + t.height + 300, { steps: 8 });
  await page.mouse.up();
  const cols = (await block(page, 2)) as Extract<Block, { type: "Columns" }>;
  expect(cols.children[0]?.map(words)).toEqual(["", "left"]);
  expect((await doc(page)).blocks.map((b) => b.type)).toEqual(["Heading", "Text", "Columns"]);
  expect(cols.children[1]).toHaveLength(1);
});

test("the_panel_edits_the_selected_block_and_no_other", async ({ page, app }) => {
  await open(page, app);
  await expect(page.getByText("Select a block to edit its properties.")).toBeVisible();
  const before = JSON.stringify(await doc(page));

  await page.getByRole("button", { name: "Select Heading block 1" }).click();
  await expect(page.getByLabel("Heading level")).toBeVisible();
  await page.getByRole("button", { name: "Select Text block 2" }).click();
  await expect(page.getByLabel("Heading level")).toHaveCount(0);
  await expect(page.getByRole("group", { name: "Text formatting" })).toBeVisible();

  await page.getByRole("button", { name: "Select Heading block 1" }).click();
  await page.getByLabel("Heading level").selectOption("5");
  const after = (await doc(page)).blocks;
  expect(after[0]).toMatchObject({ level: 5 });
  expect(JSON.stringify(after.slice(1))).toBe(JSON.stringify((JSON.parse(before) as Doc).blocks.slice(1)));

  await page.getByRole("button", { name: "Select Heading block 1" }).click(); // deselect
  await expect(page.getByText("Select a block to edit its properties.")).toBeVisible();
});

test("a_burst_of_typing_is_one_undo_step_and_a_pause_or_a_blur_ends_it", async ({ page, app }) => {
  await open(page, app);
  const text = () => page.evaluate(() => (window as unknown as { __doc: Doc }).__doc).then((d) => words(d.blocks[1] as Block));
  const undo = page.getByRole("button", { name: "Undo" });

  // 40 keystrokes, one step: one Undo returns the original.
  await caretIn(page, "Text block 2");
  await page.keyboard.press("End");
  await page.keyboard.type("a".repeat(40));
  expect(await text()).toBe("Hi {tag}!" + "a".repeat(40));
  await undo.click();
  expect(await text()).toBe("Hi {tag}!");
  await expect(undo).toBeDisabled();

  // A pause ends the step: two bursts, two Undos.
  await caretIn(page, "Text block 2");
  await page.keyboard.press("End");
  await page.keyboard.type("one");
  await page.waitForTimeout(PAUSE + 150);
  await page.keyboard.type("two");
  await undo.click();
  expect(await text()).toBe("Hi {tag}!one");
  await undo.click();
  expect(await text()).toBe("Hi {tag}!");

  // Leaving the block ends it as well.
  await caretIn(page, "Text block 2");
  await page.keyboard.press("End");
  await page.keyboard.type("x");
  await caretIn(page, "Heading block 1");
  await caretIn(page, "Text block 2");
  await page.keyboard.press("End");
  await page.keyboard.type("y");
  await undo.click();
  expect(await text()).toBe("Hi {tag}!x");
});

test("the_editor_with_a_text_block_selected_has_no_accessibility_violations", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await expectNoViolations(new AxeBuilder({ page }), "block editor with a text block selected");
});
