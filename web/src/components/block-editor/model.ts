// SPDX-License-Identifier: AGPL-3.0-only

/**
 * The document model. The document is plain data in React state and is the only source of truth:
 * the DOM is never read back as a document, and undo is a stack of these, not the browser's.
 *
 * `Block` and `Run` mirror the Rust enums in `src/template/block.rs` variant for variant. They are
 * the contract between the lanes: what the editor serialises is what the server parses. Every
 * variant name below is a Rust variant name.
 *
 * Every operation names exactly one block. Selecting across blocks is not supported (spec 5.1),
 * so no operation takes a range, and one that is handed several is rejected, never half-applied.
 */

/** Inline content. A run is flat, never nested: bold-italic is a later decision, not an accident. */
export type Run =
  | { type: "Plain"; text: string }
  | { type: "Bold"; text: string }
  | { type: "Italic"; text: string }
  | { type: "Link"; href: string; text: string }
  /** A merge tag. Atomic: never split, deleted whole. `source` is what sits between `{{` and `}}`. */
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
      return { type, runs: [{ type: "Plain", text: "" }] };
    case "Heading":
      return { type, level: 2, runs: [{ type: "Plain", text: "" }] };
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

const oneBlock = (b: unknown): Block =>
  typeof b === "object" && b !== null && !Array.isArray(b) && typeof (b as Block).type === "string"
    ? (b as Block)
    : reject("an operation carries exactly one block");

// Array#toSpliced is ES2023 and this project targets ES2022.
const splice = <T,>(list: T[], at: number, remove: number, ...add: T[]): T[] => [
  ...list.slice(0, at),
  ...add,
  ...list.slice(at + remove),
];

const sameList = (a: Loc, b: Loc) => a.length === b.length && a.slice(0, -1).every((n, k) => n === b[k]);

/** Splits one run at `offset`; a Tag has only two edges, so it is never cut. */
function cut(run: Run, offset: number): [Run[], Run[]] {
  if (run.type === "Tag") {
    return offset === 0 ? [[], [run]] : offset === 1 ? [[run], []] : reject("a merge tag is never split");
  }
  if (!Number.isInteger(offset) || offset < 0 || offset > run.text.length) {
    return reject("split offset is outside the run");
  }
  const head = { ...run, text: run.text.slice(0, offset) };
  const tail = { ...run, text: run.text.slice(offset) };
  return [head.text ? [head] : [], tail.text ? [tail] : []];
}

/** Applies one operation. Pure: returns a new Doc, or throws OpRejected and changes nothing. */
export function apply(doc: Doc, op: Op): Doc {
  // `op` can come from anywhere, so the discriminant is checked, not trusted.
  switch (op?.type) {
    case "insert": {
      const at = checked(op.at, "insert");
      const b = oneBlock(op.block);
      if (at.length > 1 && b.type === "Columns") reject("Columns do not nest");
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
      const b = oneBlock(op.block);
      if (at.length > 1 && b.type === "Columns") reject("Columns do not nest");
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
      return {
        blocks: within(doc.blocks, at, (list, i) => {
          const b = list[i];
          if (b?.type !== "Text") return reject("only a Text block can be split");
          const run = b.runs[op.run];
          if (!run) return reject("split names a run that does not exist");
          const [head, tail] = cut(run, op.offset);
          const first: Block = { type: "Text", runs: [...b.runs.slice(0, op.run), ...head] };
          const second: Block = { type: "Text", runs: [...tail, ...b.runs.slice(op.run + 1)] };
          return splice(list, i, 1, first, second);
        }),
      };
    }
    default:
      return reject("unknown operation");
  }
}

/** Undo is a stack of Docs: `past` oldest first, `future` next redo first. */
export type History = { past: Doc[]; present: Doc; future: Doc[] };

const DEPTH = 200;

export const start = (doc: Doc): History => ({ past: [], present: doc, future: [] });

/** Applies `op` and records it. A rejected op throws before anything is recorded. */
export function commit(h: History, op: Op): History {
  return { past: [...h.past, h.present].slice(-DEPTH), present: apply(h.present, op), future: [] };
}

export const undo = (h: History): History => {
  const previous = h.past.at(-1);
  return previous ? { past: h.past.slice(0, -1), present: previous, future: [h.present, ...h.future] } : h;
};

export const redo = (h: History): History => {
  const [next, ...future] = h.future;
  return next ? { past: [...h.past, h.present], present: next, future } : h;
};
