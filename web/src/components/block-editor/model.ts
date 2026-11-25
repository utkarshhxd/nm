// SPDX-License-Identifier: AGPL-3.0-only

import { normalize } from "./runs";

/**
 * The document model. The document is plain data in React state and is the only source of truth:
 * the DOM is a paint of it (rich-text.tsx reads one block's runs back after typing, nothing more),
 * and undo is a stack of these, not the browser's.
 *
 * `Block` and `Run` mirror the Rust enums in `src/template/block.rs` variant for variant, in the
 * shape `.superpowers/sdd/m2b/contract-block-tree.md` fixes. They are
 * the contract between the lanes: what the editor serialises is what the server parses. Every
 * variant name below is a Rust variant name.
 *
 * Every operation names exactly one block. Selecting across blocks is not supported (spec 5.1),
 * so no operation takes a range, and one that is handed several is rejected, never half-applied.
 */

export type Mark = "bold" | "italic";

/**
 * Inline content, as the cross-lane contract fixes it (`.superpowers/sdd/m2b/contract-block-tree.md`).
 * `marks` is always present, `[]` when empty, and is a set: bold and italic can sit on one run.
 * A mark list is replaced, never mutated, because a split leaves two runs holding the same array
 * and history holds both.
 */
export type Run =
  | { type: "Text"; text: string; marks: Mark[] }
  | { type: "Link"; href: string; text: string; marks: Mark[] }
  /** A merge tag. Atomic: never split, deleted whole. `source` is the whole tag, braces included. */
  | { type: "Tag"; source: string };

export type Block =
  | { type: "Text"; runs: Run[] }
  | { type: "Heading"; level: number; runs: Run[] }
  | { type: "Image"; src: string; alt: string }
  | { type: "Button"; label: string; href: string }
  | { type: "Divider" }
  | { type: "Spacer"; height: number }
  | { type: "Columns"; children: Block[][] };

export type Doc = { blocks: Block[] };

/**
 * Where one block is: `[i]` is the i-th block of the document, `[i, c, j]` the j-th block of column
 * c of the Columns block at i. As an insert target it is the index the new block will occupy.
 */
export type Loc = number[];

export type Op =
  | { type: "insert"; at: Loc; block: Block }
  | { type: "delete"; at: Loc }
  | { type: "update"; at: Loc; block: Block }
  /** Reorder inside one list: `to` is the index the block ends up at. Between lists is delete then insert. */
  | { type: "move"; from: Loc; to: Loc }
  /** Cut a Text block in two. `offset` counts characters in the run, or is 0 or 1 (before, after) for a Tag. */
  | { type: "split"; at: Loc; run: number; offset: number };

/** An operation that cannot be applied whole. The document is untouched when this is thrown. */
export class OpRejected extends Error {
  override name = "OpRejected";
}

const reject = (why: string): never => {
  throw new OpRejected(why);
};

/** A new block with the content a person would expect from the palette. */
export function blank(type: Block["type"]): Block {
  switch (type) {
    case "Text":
      return { type, runs: [{ type: "Text", text: "", marks: [] }] };
    case "Heading":
      return { type, level: 2, runs: [{ type: "Text", text: "", marks: [] }] };
    case "Image":
      return { type, src: "", alt: "" };
    case "Button":
      return { type, label: "Button", href: "" };
    case "Divider":
      return { type };
    case "Spacer":
      return { type, height: 24 };
    case "Columns":
      return { type, children: [[], []] };
  }
}

/** `[i]` or `[i, column, j]`: non-negative integers, odd length. A list of locations is not a Loc. */
function checked(loc: unknown, role: string): Loc {
  const ok =
    Array.isArray(loc) &&
    loc.length % 2 === 1 &&
    loc.every((n) => Number.isInteger(n) && (n as number) >= 0);
  return ok ? (loc as Loc) : reject(`${role} must name exactly one block, as [i] or [i, column, j]`);
}

/** Rewrites the list that `loc` points into, and rebuilds the path above it. Nothing is mutated. */
function within(blocks: Block[], loc: Loc, f: (list: Block[], index: number) => Block[]): Block[] {
  const [i, column, ...rest] = loc as [number, number | undefined, ...number[]];
  if (column === undefined) return f(blocks, i);
  const host = blocks[i];
  if (host?.type !== "Columns") return reject(`no Columns block at [${i}]`);
  const list = host.children[column];
  if (!list) return reject(`no column ${column} in the Columns block at [${i}]`);
  const children = host.children.slice();
  children[column] = within(list, rest, f);
  return splice(blocks, i, 1, { ...host, children });
}

const isObject = (x: unknown): x is Record<string, unknown> =>
  typeof x === "object" && x !== null && !Array.isArray(x);
const isText = (x: unknown) => typeof x === "string";
const areMarks = (m: unknown) => Array.isArray(m) && m.every((x) => x === "bold" || x === "italic");

function validRun(r: unknown): boolean {
  if (!isObject(r)) return false;
  switch (r["type"]) {
    case "Text":
      return isText(r["text"]) && areMarks(r["marks"]);
    case "Link":
      return isText(r["href"]) && isText(r["text"]) && areMarks(r["marks"]);
    case "Tag":
      return isText(r["source"]);
    default:
      return false;
  }
}

/** One well-formed block. `nested` is true inside a column, where a Columns block cannot go. */
function oneBlock(b: unknown, nested: boolean): Block {
  if (!isObject(b)) return reject("an operation carries exactly one block");
  switch (b["type"]) {
    case "Text":
    case "Heading": {
      const { runs, level } = b;
      if (!Array.isArray(runs) || !runs.every(validRun)) return reject(`a ${b["type"]} block needs valid runs`);
      if (b["type"] === "Heading" && !(Number.isInteger(level) && (level as number) >= 1 && (level as number) <= 6)) {
        return reject("a Heading level is 1 to 6");
      }
      break;
    }
    case "Image":
      if (!isText(b["src"]) || !isText(b["alt"])) return reject("an Image block needs src and alt");
      break;
    case "Button":
      if (!isText(b["label"]) || !isText(b["href"])) return reject("a Button block needs label and href");
      break;
    case "Divider":
      break;
    case "Spacer": {
      const h = b["height"];
      if (!Number.isInteger(h) || (h as number) < 0 || (h as number) > 65535) return reject("a Spacer height is 0 to 65535");
      break;
    }
    case "Columns": {
      if (nested) return reject("Columns do not nest");
      const { children } = b;
      if (!Array.isArray(children) || !children.every((c) => Array.isArray(c))) {
        return reject("Columns children are a list of lists");
      }
      for (const column of children as unknown[][]) for (const child of column) oneBlock(child, true);
      break;
    }
    default:
      return reject("unknown block type");
  }
  return b as unknown as Block;
}

// Array#toSpliced is ES2023 and this project targets ES2022.
const splice = <T,>(list: T[], at: number, remove: number, ...add: T[]): T[] => [
  ...list.slice(0, at),
  ...add,
  ...list.slice(at + remove),
];

const sameList = (a: Loc, b: Loc) => a.length === b.length && a.slice(0, -1).every((n, k) => n === b[k]);

/** Splits one run at `offset`, counted in code points so an emoji is never torn; a Tag has only two edges. */
function cut(run: Run, offset: number): [Run[], Run[]] {
  if (run.type === "Tag") {
    return offset === 0 ? [[], [run]] : offset === 1 ? [[run], []] : reject("a merge tag is never split");
  }
  const chars = [...run.text];
  if (!Number.isInteger(offset) || offset < 0 || offset > chars.length) {
    return reject("split offset is outside the run");
  }
  return [
    [{ ...run, text: chars.slice(0, offset).join("") }],
    [{ ...run, text: chars.slice(offset).join("") }],
  ];
}

/** The fields each op defines. An op carrying any other is rejected: a stray `to` must not be ignored. */
const FIELDS: Record<Op["type"], string[]> = {
  insert: ["at", "block"],
  delete: ["at"],
  update: ["at", "block"],
  move: ["from", "to"],
  split: ["at", "run", "offset"],
};

/** Applies one operation. Pure: returns a new Doc, or throws OpRejected and changes nothing. */
export function apply(doc: Doc, op: Op): Doc {
  // `op` can come from anywhere, so its shape is checked, not trusted.
  const fields = isObject(op) && typeof op.type === "string" && Object.hasOwn(FIELDS, op.type) ? FIELDS[op.type] : undefined;
  if (!fields) return reject("unknown operation");
  for (const key of Object.keys(op)) {
    if (key !== "type" && !fields.includes(key)) reject(`${op.type} has no field "${key}"`);
  }
  switch (op.type) {
    case "insert": {
      const at = checked(op.at, "insert");
      const b = oneBlock(op.block, at.length > 1);
      return {
        blocks: within(doc.blocks, at, (list, i) =>
          i > list.length ? reject(`cannot insert at ${i}: the list has ${list.length}`) : splice(list, i, 0, b),
        ),
      };
    }
    case "delete": {
      const at = checked(op.at, "delete");
      return {
        blocks: within(doc.blocks, at, (list, i) =>
          i < list.length ? splice(list, i, 1) : reject(`no block at ${i}`),
        ),
      };
    }
    case "update": {
      const at = checked(op.at, "update");
      const b = oneBlock(op.block, at.length > 1);
      return {
        blocks: within(doc.blocks, at, (list, i) =>
          i < list.length ? splice(list, i, 1, b) : reject(`no block at ${i}`),
        ),
      };
    }
    case "move": {
      const from = checked(op.from, "move");
      const to = checked(op.to, "move");
      if (!sameList(from, to)) return reject("a move stays inside one list");
      const target = to[to.length - 1] as number;
      return {
        blocks: within(doc.blocks, from, (list, i) => {
          if (i >= list.length || target >= list.length) return reject("move is outside the list");
          return splice(splice(list, i, 1), target, 0, list[i] as Block);
        }),
      };
    }
    case "split": {
      const at = checked(op.at, "split");
      if (!Number.isInteger(op.run) || op.run < 0) return reject("split names a run by its index");
      return {
        blocks: within(doc.blocks, at, (list, i) => {
          const b = list[i];
          if (b?.type !== "Text") return reject("only a Text block can be split");
          const run = b.runs[op.run];
          if (!run) return reject("split names a run that does not exist");
          const [head, tail] = cut(run, op.offset);
          // `normalize` drops empty runs and leaves an empty block as one empty run, the shape `blank` makes.
          const first: Block = { type: "Text", runs: normalize([...b.runs.slice(0, op.run), ...head]) };
          const second: Block = { type: "Text", runs: normalize([...tail, ...b.runs.slice(op.run + 1)]) };
          return splice(list, i, 1, first, second);
        }),
      };
    }
  }
}

/** Undo is a stack of Docs: `past` oldest first, `future` next redo first. */
export type History = { past: Doc[]; present: Doc; future: Doc[] };

export const DEPTH = 200;

export const start = (doc: Doc): History => ({ past: [], present: doc, future: [] });

/** Applies `op` and records it. A rejected op throws before anything is recorded. */
export function commit(h: History, op: Op): History {
  return { past: [...h.past, h.present].slice(-DEPTH), present: apply(h.present, op), future: [] };
}

/**
 * Applies `op` to the present without a new undo step: a burst of typing is one step, the one its
 * first keystroke committed. A rejected op throws, as in `commit`.
 */
export const amend = (h: History, op: Op): History => ({ ...h, present: apply(h.present, op), future: [] });

export const undo = (h: History): History => {
  const previous = h.past.at(-1);
  return previous ? { past: h.past.slice(0, -1), present: previous, future: [h.present, ...h.future] } : h;
};

export const redo = (h: History): History => {
  const [next, ...future] = h.future;
  return next ? { past: [...h.past, h.present], present: next, future } : h;
};
