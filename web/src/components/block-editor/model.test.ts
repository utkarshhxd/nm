// SPDX-License-Identifier: AGPL-3.0-only

// The document model is pure, so this needs no browser page; it only borrows Playwright's runner.

import { expect, test } from "@playwright/test";

import {
  DEPTH,
  type Block,
  type Doc,
  type Mark,
  type Op,
  type Run,
  OpRejected,
  amend,
  apply,
  blank,
  commit,
  redo,
  start,
  undo,
} from "./model";

const t = (text: string, ...marks: Mark[]): Run => ({ type: "Text", text, marks });
const text = (...runs: Run[]): Block => ({ type: "Text", runs });
const plain = (s: string): Block => text(t(s));
const doc = (...blocks: Block[]): Doc => ({ blocks });
const words = (b: Block) => (b.type === "Text" ? b.runs.map((r) => ("text" in r ? r.text : "{tag}")).join("") : b.type);
const types = (d: Doc) => d.blocks.map(words);

test("insert_places_a_block_at_an_index_and_leaves_the_input_alone", () => {
  const before = doc(plain("a"), plain("c"));
  const after = apply(before, { type: "insert", at: [1], block: plain("b") });
  expect(types(after)).toEqual(["a", "b", "c"]);
  expect(types(before)).toEqual(["a", "c"]);
  expect(types(apply(before, { type: "insert", at: [2], block: blank("Divider") }))).toEqual(["a", "c", "Divider"]);
  expect(() => apply(before, { type: "insert", at: [3], block: plain("x") })).toThrow(OpRejected);
});

test("insert_and_delete_reach_into_a_column", () => {
  const cols = doc({ type: "Columns", children: [[plain("l")], [plain("r")]] });
  const inserted = apply(cols, { type: "insert", at: [0, 1, 1], block: plain("r2") });
  expect(inserted.blocks[0]).toMatchObject({ children: [[{}], [{}, {}]] });
  const deleted = apply(inserted, { type: "delete", at: [0, 0, 0] });
  expect(deleted.blocks[0]).toMatchObject({ children: [[], [{}, {}]] });
  // Columns do not nest: MJML has no column inside a column.
  expect(() => apply(cols, { type: "insert", at: [0, 0, 0], block: blank("Columns") })).toThrow(OpRejected);
});

test("delete_removes_exactly_one_block_and_rejects_a_missing_one", () => {
  const d = doc(plain("a"), plain("b"), plain("c"));
  expect(types(apply(d, { type: "delete", at: [1] }))).toEqual(["a", "c"]);
  expect(() => apply(d, { type: "delete", at: [3] })).toThrow(OpRejected);
  expect(() => apply(d, { type: "delete", at: [-1] })).toThrow(OpRejected);
  expect(() => apply(d, { type: "delete", at: [0, 0, 0] })).toThrow(OpRejected);
});

test("move_reorders_within_one_list_only", () => {
  const d = doc(plain("a"), plain("b"), plain("c"));
  expect(types(apply(d, { type: "move", from: [0], to: [2] }))).toEqual(["b", "c", "a"]);
  expect(types(apply(d, { type: "move", from: [2], to: [0] }))).toEqual(["c", "a", "b"]);
  expect(apply(d, { type: "move", from: [1], to: [1] })).toEqual(d);
  expect(() => apply(d, { type: "move", from: [0], to: [3] })).toThrow(OpRejected);
  const cols = doc({ type: "Columns", children: [[plain("l")], []] }, plain("x"));
  // Moving between lists is a delete and an insert, two steps, never one that can half-happen.
  expect(() => apply(cols, { type: "move", from: [1], to: [0, 1, 0] })).toThrow(OpRejected);
});

test("update_replaces_one_block", () => {
  const d = doc(plain("a"), plain("b"));
  expect(types(apply(d, { type: "update", at: [1], block: plain("B") }))).toEqual(["a", "B"]);
});

test("update_of_a_block_that_is_not_there_is_rejected", () => {
  const d = doc(plain("a"));
  expect(() => apply(d, { type: "update", at: [1], block: plain("x") })).toThrow(OpRejected);
  expect(() => apply(d, { type: "update", at: [0, 0, 0], block: plain("x") })).toThrow(OpRejected);
});

test("update_move_and_split_reach_into_a_column", () => {
  const d = doc({ type: "Columns", children: [[plain("a"), plain("bc"), plain("d")], [plain("r")]] });
  const cell = (out: Doc, c: number) =>
    (out.blocks[0] as Extract<Block, { type: "Columns" }>).children[c]?.map(words);
  expect(cell(apply(d, { type: "update", at: [0, 0, 1], block: plain("B") }), 0)).toEqual(["a", "B", "d"]);
  expect(cell(apply(d, { type: "move", from: [0, 0, 0], to: [0, 0, 2] }), 0)).toEqual(["bc", "d", "a"]);
  const split = apply(d, { type: "split", at: [0, 0, 1], run: 0, offset: 1 });
  expect(cell(split, 0)).toEqual(["a", "b", "c", "d"]);
  // The other column is untouched, and the top level still has one block.
  expect(cell(split, 1)).toEqual(["r"]);
  expect(split.blocks).toHaveLength(1);
  expect(() => apply(d, { type: "update", at: [0, 0, 3], block: plain("x") })).toThrow(OpRejected);
  expect(() => apply(d, { type: "move", from: [0, 1, 0], to: [0, 1, 1] })).toThrow(OpRejected);
});

test("split_cuts_a_text_block_inside_a_run", () => {
  const link: Run = { type: "Link", href: "https://a.test", text: "g", marks: [] };
  const d = doc(text(t("ab"), t("cdef", "bold"), link));
  const out = apply(d, { type: "split", at: [0], run: 1, offset: 2 });
  expect(out.blocks).toEqual([text(t("ab"), t("cd", "bold")), text(t("ef", "bold"), link)]);
});

test("split_at_a_run_edge_leaves_no_empty_run_and_an_empty_half_is_one_empty_run", () => {
  const d = doc(text(t("ab"), t("cd", "italic")));
  expect(apply(d, { type: "split", at: [0], run: 1, offset: 0 }).blocks).toEqual([text(t("ab")), text(t("cd", "italic"))]);
  // An empty half has the one spelling `blank` uses, never `runs: []`.
  expect(apply(d, { type: "split", at: [0], run: 1, offset: 2 }).blocks).toEqual([text(t("ab"), t("cd", "italic")), text(t(""))]);
  expect(apply(doc(plain("ab")), { type: "split", at: [0], run: 0, offset: 0 }).blocks[0]).toEqual(blank("Text"));
});

test("split_never_cuts_a_tag", () => {
  const tag = { type: "Tag", source: '{{ first_name | "there" }}' } as const;
  const d = doc(text(t("Hi "), tag, t("!")));
  expect(apply(d, { type: "split", at: [0], run: 1, offset: 0 }).blocks).toEqual([text(t("Hi ")), text(tag, t("!"))]);
  expect(apply(d, { type: "split", at: [0], run: 1, offset: 1 }).blocks).toEqual([text(t("Hi "), tag), text(t("!"))]);
  // Offset 3 into a tag would be half of it.
  expect(() => apply(d, { type: "split", at: [0], run: 1, offset: 3 })).toThrow(OpRejected);
});

test("split_counts_code_points_so_an_emoji_is_never_torn", () => {
  const out = apply(doc(plain("a\u{1F600}b")), { type: "split", at: [0], run: 0, offset: 2 });
  expect(out.blocks).toEqual([plain("a\u{1F600}"), plain("b")]);
  // Offset 3 is past the end: three code points, not four code units.
  expect(() => apply(doc(plain("a\u{1F600}")), { type: "split", at: [0], run: 0, offset: 3 })).toThrow(OpRejected);
});

test("split_rejects_a_block_that_is_not_text_a_bad_offset_and_a_bad_run_index", () => {
  const d = doc(blank("Divider"), plain("ab"));
  const bad = (op: unknown) => expect(() => apply(d, op as Op)).toThrow(OpRejected);
  bad({ type: "split", at: [0], run: 0, offset: 0 });
  bad({ type: "split", at: [1], run: 0, offset: 3 });
  bad({ type: "split", at: [1], run: 0, offset: -1 });
  bad({ type: "split", at: [1], run: 0, offset: 0.5 });
  bad({ type: "split", at: [1], run: 1, offset: 0 });
  bad({ type: "split", at: [1], run: -1, offset: 0 });
  // A string is not a run index: "0" would otherwise find run 0 and double the content.
  bad({ type: "split", at: [1], run: "0", offset: 0 });
});

test("the_halves_of_a_split_do_not_share_a_marks_array", () => {
  const out = apply(doc(text(t("abcd", "bold"))), { type: "split", at: [0], run: 0, offset: 2 });
  const [a, b] = out.blocks.map((x) => (x.type === "Text" ? x.runs[0] : undefined));
  if (a?.type !== "Text" || b?.type !== "Text") throw new Error("expected two text runs");
  expect(a.marks).not.toBe(b.marks);
});

test("undo_and_redo_walk_a_stack_of_documents", () => {
  let h = start(doc(plain("a")));
  h = commit(h, { type: "insert", at: [1], block: plain("b") });
  h = commit(h, { type: "insert", at: [2], block: plain("c") });
  expect(types(h.present)).toEqual(["a", "b", "c"]);
  h = undo(h);
  expect(types(h.present)).toEqual(["a", "b"]);
  h = undo(h);
  h = undo(h); // nothing left: a no-op, not a throw
  expect(types(h.present)).toEqual(["a"]);
  h = redo(h);
  h = redo(h);
  h = redo(h);
  expect(types(h.present)).toEqual(["a", "b", "c"]);
  // A new edit after an undo discards the redo branch.
  h = undo(h);
  h = commit(h, { type: "delete", at: [0] });
  expect(h.future).toEqual([]);
  expect(types(h.present)).toEqual(["b"]);
});

test("a_burst_of_typing_amends_one_undo_step", () => {
  let h = start(doc(plain("")));
  h = commit(h, { type: "update", at: [0], block: plain("a") });
  for (let i = 2; i <= 30; i++) h = amend(h, { type: "update", at: [0], block: plain("a".repeat(i)) });
  expect(h.past).toHaveLength(1);
  expect(types(h.present)).toEqual(["a".repeat(30)]);
  expect(types(undo(h).present)).toEqual([""]);
  expect(() => amend(h, { type: "delete", at: [9] })).toThrow(OpRejected);
});

test("history_keeps_at_most_depth_steps_and_drops_the_oldest", () => {
  let h = start(doc(plain("0")));
  for (let i = 1; i <= DEPTH + 5; i++) h = commit(h, { type: "update", at: [0], block: plain(String(i)) });
  expect(h.past).toHaveLength(DEPTH);
  expect(types(h.past[0] as Doc)).toEqual(["5"]);
});

test("a_rejected_op_leaves_history_alone", () => {
  const h = start(doc(plain("a")));
  expect(() => commit(h, { type: "delete", at: [5] })).toThrow(OpRejected);
  expect(h.past).toEqual([]);
});

// The document has a Columns block, so a loc that reaches into one is well-formed enough to get
// past every later check: only the shape guard can be what rejects these.
const withColumns = (): Doc => doc(plain("a"), { type: "Columns", children: [[plain("l")], [plain("r")]] }, plain("c"));
const bad = (op: unknown) => expect(() => apply(withColumns(), op as Op)).toThrow(OpRejected);

test("an_op_naming_two_blocks_is_rejected_not_half_applied", () => {
  bad({ type: "delete", at: [[0], [1]] });
  bad({ type: "delete", at: [1, 0] });
  bad({ type: "delete", at: [1, 0, 0, 0] });
  bad({ type: "delete", at: [] });
  bad({ type: "delete", at: [0.5] });
  bad({ type: "delete", at: { from: 0, to: 1 } });
  bad({ type: "update", at: [[0], [1]], block: plain("x") });
  bad({ type: "split", at: [[0], [1]], run: 0, offset: 0 });
  bad({ type: "move", from: [[0], [1]], to: [2] });
  bad({ type: "move", from: [1, 0, 0], to: [1, 1, 0, 0] });
  bad({ type: "insert", at: [1], block: [plain("x"), plain("y")] });
  bad({ type: "reverse-everything" });
  bad({});
  bad(null);
});

test("an_op_with_a_field_its_variant_does_not_define_is_rejected_not_half_applied", () => {
  // A range written as a second field must not delete one block and ignore the rest.
  bad({ type: "delete", at: [0], to: [2] });
  bad({ type: "delete", at: [0], count: 2 });
  bad({ type: "update", at: [0], block: plain("x"), also: [1] });
  bad({ type: "insert", at: [0], block: plain("x"), after: [1] });
  bad({ type: "move", from: [0], to: [2], through: [1] });
  bad({ type: "split", at: [0], run: 0, offset: 0, then: 1 });
});

test("a_block_must_be_a_known_variant_and_columns_do_not_nest", () => {
  bad({ type: "insert", at: [0], block: { type: "Bogus" } });
  bad({ type: "update", at: [0], block: { type: "Bogus" } });
  bad({ type: "insert", at: [0], block: { type: "Text", runs: [{ type: "Plain", text: "x" }] } });
  bad({ type: "insert", at: [0], block: { type: "Text", runs: [{ type: "Text", text: "x", marks: ["underline"] }] } });
  bad({ type: "insert", at: [0], block: { type: "Heading", level: 7, runs: [] } });
  bad({ type: "insert", at: [0], block: { type: "Spacer", height: -1 } });
  bad({ type: "insert", at: [0], block: { type: "Button", label: "x" } });
  // Nested Columns three ways: into a column, in a top-level block that carries one, and by update.
  bad({ type: "insert", at: [1, 0, 0], block: blank("Columns") });
  bad({ type: "insert", at: [0], block: { type: "Columns", children: [[blank("Columns")]] } });
  bad({ type: "update", at: [1, 0, 0], block: blank("Columns") });
  expect(apply(withColumns(), { type: "insert", at: [0], block: blank("Columns") }).blocks).toHaveLength(4);
});

test("every_blank_block_is_a_valid_block", () => {
  for (const type of ["Text", "Heading", "Image", "Button", "Divider", "Spacer", "Columns"] as const) {
    expect(() => apply(doc(), { type: "insert", at: [0], block: blank(type) })).not.toThrow();
  }
});

test("a_rejected_op_does_not_touch_the_input", () => {
  const d = withColumns();
  const before = JSON.stringify(d);
  expect(() => apply(d, { type: "delete", at: [0], to: [2] } as unknown as Op)).toThrow(OpRejected);
  expect(JSON.stringify(d)).toBe(before);
});
