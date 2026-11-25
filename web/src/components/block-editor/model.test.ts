// SPDX-License-Identifier: AGPL-3.0-only

// The document model is pure, so this needs no browser page; it only borrows Playwright's runner.

import { expect, test } from "@playwright/test";

import {
  type Block,
  type Doc,
  type Op,
  type Run,
  OpRejected,
  apply,
  blank,
  commit,
  redo,
  start,
  undo,
} from "./model";

const text = (...runs: Run[]): Block => ({ type: "Text", runs });
const plain = (s: string): Block => text({ type: "Plain", text: s });
const doc = (...blocks: Block[]): Doc => ({ blocks });
const types = (d: Doc) => d.blocks.map((b) => (b.type === "Text" ? b.runs.map((r) => ("text" in r ? r.text : "{tag}")).join("") : b.type));

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

test("split_cuts_a_text_block_inside_a_run", () => {
  const d = doc(
    text({ type: "Plain", text: "ab" }, { type: "Bold", text: "cdef" }, { type: "Link", href: "https://a.test", text: "g" }),
  );
  const out = apply(d, { type: "split", at: [0], run: 1, offset: 2 });
  expect(out.blocks).toEqual([
    text({ type: "Plain", text: "ab" }, { type: "Bold", text: "cd" }),
    text({ type: "Bold", text: "ef" }, { type: "Link", href: "https://a.test", text: "g" }),
  ]);
});

test("split_at_a_run_edge_leaves_no_empty_run", () => {
  const d = doc(text({ type: "Plain", text: "ab" }, { type: "Italic", text: "cd" }));
  expect(apply(d, { type: "split", at: [0], run: 1, offset: 0 }).blocks).toEqual([
    text({ type: "Plain", text: "ab" }),
    text({ type: "Italic", text: "cd" }),
  ]);
  expect(apply(d, { type: "split", at: [0], run: 1, offset: 2 }).blocks).toEqual([
    text({ type: "Plain", text: "ab" }, { type: "Italic", text: "cd" }),
    text(),
  ]);
});

test("split_never_cuts_a_tag", () => {
  const tag = { type: "Tag", source: 'first_name | "there"' } as const;
  const d = doc(text({ type: "Plain", text: "Hi " }, tag, { type: "Plain", text: "!" }));
  expect(apply(d, { type: "split", at: [0], run: 1, offset: 0 }).blocks).toEqual([
    text({ type: "Plain", text: "Hi " }),
    text(tag, { type: "Plain", text: "!" }),
  ]);
  expect(apply(d, { type: "split", at: [0], run: 1, offset: 1 }).blocks).toEqual([
    text({ type: "Plain", text: "Hi " }, tag),
    text({ type: "Plain", text: "!" }),
  ]);
  // Offset 3 into a tag would be half of it.
  expect(() => apply(d, { type: "split", at: [0], run: 1, offset: 3 })).toThrow(OpRejected);
});

test("split_rejects_a_block_that_is_not_text_and_an_offset_outside_the_run", () => {
  const d = doc(blank("Divider"), plain("ab"));
  expect(() => apply(d, { type: "split", at: [0], run: 0, offset: 0 })).toThrow(OpRejected);
  expect(() => apply(d, { type: "split", at: [1], run: 0, offset: 3 })).toThrow(OpRejected);
  expect(() => apply(d, { type: "split", at: [1], run: 1, offset: 0 })).toThrow(OpRejected);
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

test("a_rejected_op_leaves_history_alone", () => {
  const h = start(doc(plain("a")));
  expect(() => commit(h, { type: "delete", at: [5] })).toThrow(OpRejected);
  expect(h.past).toEqual([]);
});

test("an_op_naming_two_blocks_is_rejected_not_half_applied", () => {
  const d = doc(plain("a"), plain("b"), plain("c"));
  // The types forbid these; the runtime must too, since an op can come from anywhere.
  const two = (op: unknown) => () => apply(d, op as Op);
  expect(two({ type: "delete", at: [[0], [1]] })).toThrow(OpRejected);
  expect(two({ type: "delete", at: [0, 1] })).toThrow(OpRejected);
  expect(two({ type: "update", at: [[0], [1]], block: plain("x") })).toThrow(OpRejected);
  expect(two({ type: "split", at: [[0], [1]], run: 0, offset: 0 })).toThrow(OpRejected);
  expect(two({ type: "move", from: [[0], [1]], to: [2] })).toThrow(OpRejected);
  expect(two({ type: "insert", at: [1], block: [plain("x"), plain("y")] })).toThrow(OpRejected);
  expect(two({ type: "delete", at: { from: 0, to: 1 } })).toThrow(OpRejected);
  expect(two({ type: "reverse-everything" })).toThrow(OpRejected);
  expect(d.blocks).toHaveLength(3);
});
