// SPDX-License-Identifier: AGPL-3.0-only

import { useState } from "react";

import { Button } from "../button";
import { BlockView } from "./block";
import {
  type Block,
  type Doc,
  type History,
  type Loc,
  type Op,
  OpRejected,
  blank,
  commit,
  redo,
  start,
  undo,
} from "./model";

const PALETTE: Block["type"][] = ["Text", "Heading", "Image", "Button", "Divider", "Spacer", "Columns"];

const same = (a: Loc | null, b: Loc) => a !== null && a.join() === b.join();
const label = (b: Block, loc: Loc) => `${b.type} block ${loc.map((n) => n + 1).join(".")}`;

/**
 * The block editor shell. The document is `history.present`, the one source of truth; every change
 * is an `Op` through `commit`, so undo is a stack of documents and a rejected op changes nothing.
 * One block is selected at a time and every operation acts on it. There is no multi-block
 * selection, so there is nothing to offer for one.
 */
export function BlockEditor({ initial, onChange }: { initial: Doc; onChange?: (doc: Doc) => void }) {
  const [history, setHistory] = useState<History>(() => start(initial));
  const [selected, setSelected] = useState<Loc | null>(null);
  const [problem, setProblem] = useState<string | null>(null);

  function show(next: History, select: Loc | null) {
    setHistory(next);
    setSelected(select);
    setProblem(null);
    onChange?.(next.present);
  }

  function run(op: Op, select: Loc | null) {
    try {
      show(commit(history, op), select);
    } catch (e) {
      if (!(e instanceof OpRejected)) throw e;
      setProblem(e.message);
    }
  }

  function add(type: Block["type"]) {
    // After the selected block, in its own list; with nothing selected, at the end of the document.
    const at = selected ? [...selected.slice(0, -1), (selected.at(-1) as number) + 1] : [history.present.blocks.length];
    run({ type: "insert", at, block: blank(type) }, at);
  }

  function row(block: Block, loc: Loc, lastInList: boolean) {
    const isSelected = same(selected, loc);
    const parent = loc.slice(0, -1);
    const index = loc.at(-1) as number;
    const columns =
      block.type === "Columns"
        ? block.children.map((list, c) => (
            <div key={c} role="group" aria-label={`Column ${c + 1} of ${label(block, loc)}`} className="min-h-12 space-y-2">
              {list.map((child, j) => row(child, [...loc, c, j], j === list.length - 1))}
            </div>
          ))
        : undefined;
    return (
      <div
        key={loc.join()}
        className={`rounded-sm border p-2 ${isSelected ? "border-accent ring-2 ring-accent" : "border-border"}`}
      >
        <div className="flex flex-wrap items-center gap-1">
          <Button
            variant="ghost"
            aria-pressed={isSelected}
            aria-label={`Select ${label(block, loc)}`}
            onClick={() => setSelected(isSelected ? null : loc)}
          >
            {block.type}
          </Button>
          {isSelected && (
            <>
              <Button
                variant="ghost"
                disabled={index === 0}
                onClick={() => run({ type: "move", from: loc, to: [...parent, index - 1] }, [...parent, index - 1])}
              >
                Move up
              </Button>
              <Button
                variant="ghost"
                disabled={lastInList}
                onClick={() => run({ type: "move", from: loc, to: [...parent, index + 1] }, [...parent, index + 1])}
              >
                Move down
              </Button>
              <Button variant="danger-ghost" onClick={() => run({ type: "delete", at: loc }, null)}>
                Delete block
              </Button>
            </>
          )}
        </div>
        <div className="mt-2">
          <BlockView block={block} columns={columns} />
        </div>
      </div>
    );
  }

  const { blocks } = history.present;
  return (
    <section aria-label="Block editor" className="space-y-3">
      <div role="toolbar" aria-label="Editor" className="flex flex-wrap gap-2">
        {PALETTE.map((type) => (
          <Button key={type} onClick={() => add(type)}>
            Add {type.toLowerCase()}
          </Button>
        ))}
        <Button disabled={history.past.length === 0} onClick={() => show(undo(history), null)}>
          Undo
        </Button>
        <Button disabled={history.future.length === 0} onClick={() => show(redo(history), null)}>
          Redo
        </Button>
      </div>
      {problem && (
        <p role="alert" className="text-danger-text">
          {problem}
        </p>
      )}
      <div className="space-y-2">
        {blocks.length === 0 ? <p className="text-fg-muted">No blocks yet. Add one above.</p> : null}
        {blocks.map((b, i) => row(b, [i], i === blocks.length - 1))}
      </div>
    </section>
  );
}
