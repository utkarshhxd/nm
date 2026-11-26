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

type KeyInit = { isComposing?: boolean; keyCode?: number };
const kinds = async (page: Page) => (await doc(page)).blocks.map((b) => b.type);
const tagsIn = (b: Block) => (b.type === "Text" ? b.runs.filter((r) => r.type === "Tag") : []);
const TAG = { type: "Tag", source: '{{ first_name | "there" }}' };

test("enter_in_the_middle_of_a_text_block_splits_it_at_the_caret_and_the_caret_moves_on", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("Home");
  for (let i = 0; i < 2; i++) await page.keyboard.press("ArrowRight"); // "Hi|"
  await page.keyboard.press("Enter");
  expect(await kinds(page)).toEqual(["Heading", "Text", "Text", "Columns"]);
  expect([words(await block(page, 1)), words(await block(page, 2))]).toEqual(["Hi", " {tag}!"]);
  // The caret is at the start of the second half, so what is typed goes there.
  await expect(box(page, "Text block 3")).toBeFocused();
  await page.keyboard.type("X");
  expect(words(await block(page, 2))).toBe("X {tag}!");
  // The split is one undo step of its own, and the typing after it is another.
  await page.getByRole("button", { name: "Undo" }).click();
  expect(await kinds(page)).toEqual(["Heading", "Text", "Text", "Columns"]);
  await page.getByRole("button", { name: "Undo" }).click();
  expect(await kinds(page)).toEqual(["Heading", "Text", "Columns"]);
  expect(words(await block(page, 1))).toBe("Hi {tag}!");
});

test("enter_at_the_end_of_a_block_adds_an_empty_text_block_below_with_the_caret_in_it", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("End");
  await page.keyboard.press("Enter");
  expect(await kinds(page)).toEqual(["Heading", "Text", "Text", "Columns"]);
  expect(await block(page, 2)).toEqual({ type: "Text", runs: [{ type: "Text", text: "", marks: [] }] });
  expect(words(await block(page, 1))).toBe("Hi {tag}!");
  await expect(box(page, "Text block 3")).toBeFocused();
  await page.keyboard.type("new");
  expect(words(await block(page, 2))).toBe("new");
});

test("enter_beside_or_on_a_merge_tag_never_tears_it", async ({ page, app }) => {
  const runsOf = async (i: number) => ((await block(page, i)) as Extract<Block, { type: "Text" }>).runs;
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("Home");
  for (let i = 0; i < 3; i++) await page.keyboard.press("ArrowRight"); // just before the tag
  await page.keyboard.press("Enter");
  expect(await runsOf(1)).toEqual([{ type: "Text", text: "Hi ", marks: [] }]);
  expect(await runsOf(2)).toEqual([TAG, { type: "Text", text: "!", marks: ["bold"] }]);

  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("End");
  await page.keyboard.press("ArrowLeft"); // just after the tag
  await page.keyboard.press("Enter");
  expect(await runsOf(1)).toEqual([{ type: "Text", text: "Hi ", marks: [] }, TAG]);
  expect(await runsOf(2)).toEqual([{ type: "Text", text: "!", marks: ["bold"] }]);

  // A click on the tag itself puts the caret in its text; the split still lands on one side of it.
  await open(page, app);
  await page.locator("[data-tag]").click();
  await page.keyboard.press("Enter");
  const [a, b] = [await block(page, 1), await block(page, 2)];
  expect(a.type === "Text" && b.type === "Text").toBe(true);
  expect([...tagsIn(a), ...tagsIn(b)]).toEqual([TAG]);
  const halves = JSON.stringify([words(a), words(b)]);
  expect([JSON.stringify(["Hi ", "{tag}!"]), JSON.stringify(["Hi {tag}", "!"])]).toContain(halves);
});

test("shift_enter_is_a_line_break_inside_the_block_not_a_split", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("Home");
  for (let i = 0; i < 2; i++) await page.keyboard.press("ArrowRight"); // "Hi|"
  await page.keyboard.press("Shift+Enter");
  expect(await kinds(page)).toEqual(["Heading", "Text", "Columns"]);
  await page.keyboard.type("X");
  expect(words(await block(page, 1))).toBe("Hi\nX {tag}!");
  // At the very end, too: the break is real, not swallowed, and the caret is after it.
  await page.keyboard.press("End");
  await page.keyboard.press("Shift+Enter");
  await page.keyboard.type("Y");
  expect(words(await block(page, 1))).toBe("Hi\nX {tag}!\nY");
  expect(await kinds(page)).toEqual(["Heading", "Text", "Columns"]);
});

test("enter_in_a_heading_splits_it_into_two_headings_and_at_its_end_adds_a_text_block", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Heading block 1");
  await page.keyboard.press("Home");
  for (let i = 0; i < 3; i++) await page.keyboard.press("ArrowRight"); // "Wel|come"
  await page.keyboard.press("Enter");
  expect(await kinds(page)).toEqual(["Heading", "Heading", "Text", "Columns"]);
  expect(await block(page, 0)).toMatchObject({ level: 1, runs: [{ text: "Wel" }] });
  expect(await block(page, 1)).toMatchObject({ level: 1, runs: [{ text: "come" }] });
  await page.keyboard.press("End");
  await page.keyboard.press("Enter");
  expect(await kinds(page)).toEqual(["Heading", "Heading", "Text", "Text", "Columns"]);
});

test("enter_over_a_selection_replaces_it_with_the_split_as_one_undo_step", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("Home");
  await page.keyboard.press("ArrowRight");
  await page.keyboard.press("Shift+ArrowRight");
  await page.keyboard.press("Shift+ArrowRight"); // "i "
  await page.keyboard.press("Enter");
  expect([words(await block(page, 1)), words(await block(page, 2))]).toEqual(["H", "{tag}!"]);
  await page.getByRole("button", { name: "Undo" }).click();
  expect(await kinds(page)).toEqual(["Heading", "Text", "Columns"]);
  expect(words(await block(page, 1))).toBe("Hi {tag}!");
});

// A document the editor did not make: stored on a server or written by an API client, so it need
// not be normalised. The fixture is the point of these tests; a normalised one cannot fail them.
const given = (page: Page, app: string, blocks: Block[]) =>
  page.goto(app + EDITOR + "?doc=" + encodeURIComponent(JSON.stringify({ blocks })));
const T = (text: string, ...marks: ("bold" | "italic")[]) => ({ type: "Text" as const, text, marks });

test("enter_splits_at_the_caret_in_a_document_that_is_not_normalised", async ({ page, app }) => {
  // Adjacent runs of one kind: the caret at 8 is inside run 0 "Hello " only if runs are merged.
  await given(page, app, [{ type: "Text", runs: [T("Hello "), T("world")] }]);
  await caretIn(page, "Text block 1");
  await page.keyboard.press("Home");
  for (let i = 0; i < 8; i++) await page.keyboard.press("ArrowRight");
  await page.keyboard.press("Enter");
  await expect(page.getByRole("alert")).toHaveCount(0);
  expect([words(await block(page, 0)), words(await block(page, 1))]).toEqual(["Hello wo", "rld"]);

  // An empty run in front shifts every index by one: the split must still land at "abcd|ef".
  await given(page, app, [{ type: "Text", runs: [T(""), T("abc"), T("def", "bold")] }]);
  await caretIn(page, "Text block 1");
  await page.keyboard.press("Home");
  for (let i = 0; i < 4; i++) await page.keyboard.press("ArrowRight");
  await page.keyboard.press("Enter");
  await expect(page.getByRole("alert")).toHaveCount(0);
  expect([words(await block(page, 0)), words(await block(page, 1))]).toEqual(["abcd", "ef"]);
  expect(await block(page, 1)).toEqual({ type: "Text", runs: [T("ef", "bold")] });
  // One Undo takes the whole Enter back, to the document as it was given.
  await page.getByRole("button", { name: "Undo" }).click();
  expect((await doc(page)).blocks).toHaveLength(1);
});

test("enter_that_confirms_an_ime_candidate_does_not_split_the_block", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("Home");
  // Enter during composition is how an input method confirms a candidate. Chromium flags it with
  // isComposing; Safari sends the confirming Enter after compositionend, so isComposing is false and
  // only keyCode 229 says it belongs to the input method.
  const prevented = (init: KeyInit) =>
    page.evaluate((i) => {
      const el = document.querySelector('[aria-label="Text block 2"]') as HTMLElement;
      const e = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true, ...i });
      el.dispatchEvent(e);
      return e.defaultPrevented;
    }, init);
  expect(await prevented({ isComposing: true })).toBe(false);
  expect(await prevented({ isComposing: false, keyCode: 229 })).toBe(false);
  expect(await kinds(page)).toEqual(["Heading", "Text", "Columns"]);
  // The same key outside a composition still splits.
  await page.keyboard.press("Enter");
  expect(await kinds(page)).toEqual(["Heading", "Text", "Text", "Columns"]);
});

test("shift_enter_leaves_the_newline_inside_the_run_it_was_pressed_in", async ({ page, app }) => {
  const link = { type: "Link" as const, href: "https://a.test", text: "click here", marks: ["bold" as const] };
  await given(page, app, [{ type: "Text", runs: [link, T(" now")] }]);
  await caretIn(page, "Text block 1");
  await page.keyboard.press("Home");
  for (let i = 0; i < 5; i++) await page.keyboard.press("ArrowRight"); // "click|"
  await page.keyboard.press("Shift+Enter");
  expect(((await block(page, 0)) as Extract<Block, { type: "Text" }>).runs).toEqual([
    { ...link, text: "click\n here" },
    T(" now"),
  ]);
  // The same in a bold run: one bold run holding the break, not bold, plain, bold.
  await given(page, app, [{ type: "Text", runs: [T("a bold sentence", "bold")] }]);
  await caretIn(page, "Text block 1");
  await page.keyboard.press("Home");
  for (let i = 0; i < 6; i++) await page.keyboard.press("ArrowRight");
  await page.keyboard.press("Shift+Enter");
  expect(((await block(page, 0)) as Extract<Block, { type: "Text" }>).runs).toEqual([T("a bold\n sentence", "bold")]);
});

test("a_line_break_at_the_end_of_a_block_is_visible", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("End");
  const height = async () => ((await box(page, "Text block 2").boundingBox()) as { height: number }).height;
  const one = await height();
  await page.keyboard.press("Shift+Enter");
  // The caret now sits on a second line, so the block is taller by a line, before anything is typed after it.
  const two = await height();
  expect(two).toBeGreaterThanOrEqual(one + 15); // a line is 24px (text-base); a break adds one
  await page.keyboard.press("Shift+Enter");
  expect(await height()).toBeGreaterThanOrEqual(two + 15);
});

test("a_soft_keyboards_paragraph_and_line_break_are_handled_like_enter_and_shift_enter", async ({ page, app }) => {
  // Untrusted events have no default action, so the handler's preventDefault is read off the event:
  // without it a real keyboard would insert its own <div> or <br> as well as run the split.
  const send = (inputType: string, isComposing = false) =>
    page.evaluate(
      ([type, composing]) => {
        const el = document.querySelector('[aria-label="Text block 2"]') as HTMLElement;
        const e = new InputEvent("beforeinput", { inputType: type as string, isComposing: composing as boolean, bubbles: true, cancelable: true });
        el.dispatchEvent(e);
        return e.defaultPrevented;
      },
      [inputType, isComposing],
    );
  await open(page, app);
  await caretIn(page, "Text block 2");
  await page.keyboard.press("Home");
  for (let i = 0; i < 2; i++) await page.keyboard.press("ArrowRight"); // "Hi|"
  // Inside a composition neither is ours: the input method owns the text until it commits.
  expect(await send("insertParagraph", true)).toBe(false);
  expect(await send("insertLineBreak", true)).toBe(false);
  expect(words(await block(page, 1))).toBe("Hi {tag}!");
  expect(await send("insertLineBreak")).toBe(true);
  expect(words(await block(page, 1))).toBe("Hi\n {tag}!");
  expect(await kinds(page)).toEqual(["Heading", "Text", "Columns"]);
  expect(await send("insertParagraph")).toBe(true);
  expect(await kinds(page)).toEqual(["Heading", "Text", "Text", "Columns"]);
  expect([words(await block(page, 1)), words(await block(page, 2))]).toEqual(["Hi\n", " {tag}!"]);
});

test("enter_that_confirms_an_ime_candidate_in_the_merge_tag_field_does_not_insert_the_tag", async ({ page, app }) => {
  await open(page, app);
  await caretIn(page, "Text block 2");
  const field = page.getByLabel("Merge tag");
  await field.fill('first_name | "');
  const prevented = (init: KeyInit) =>
    field.evaluate((el, i) => {
      const e = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true, ...i });
      el.dispatchEvent(e);
      return e.defaultPrevented;
    }, init);
  expect(await prevented({ isComposing: true })).toBe(false);
  expect(await prevented({ isComposing: false, keyCode: 229 })).toBe(false);
  expect(await tagCount(page)).toBe(1); // only the one that was there
  await expect(field).toHaveValue('first_name | "');
  // Outside a composition the same key inserts it.
  await field.fill("last_name");
  await field.press("Enter");
  expect(await tagCount(page)).toBe(2);
});
