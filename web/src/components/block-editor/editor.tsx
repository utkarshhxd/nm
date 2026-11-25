// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useRef, useState } from "react";

import { Button } from "../button";
import { BlockView } from "./block";
import type { Surface } from "./blocks/surface";
import {
  type Block,
  type Doc,
  type History,
  type Loc,
  type Mark,
  type Op,
  type Run,
  OpRejected,
  amend,
  blank,
  commit,
  redo,
  start,
  undo,
} from "./model";
import { Properties } from "./properties";
import { Preview } from "./preview";
import { type PreviewSource, previewSource } from "./preview-source";
import type { Sel } from "./rich-text";
import { edit, hrefAllowed, length, locate, setLink, toggleMark } from "./runs";

const PALETTE: Block["type"][] = ["Text", "Heading", "Image", "Button", "Divider", "Spacer", "Columns"];

/** Typing that pauses this long (ms), or leaves the editor, ends an undo step. */
export const PAUSE = 700;

const same = (a: Loc | null, b: Loc) => a !== null && a.join() === b.join();
const label = (b: Block, loc: Loc) => `${b.type} block ${loc.map((n) => n + 1).join(".")}`;

type TextBlock = Extract<Block, { runs: unknown }>;
const isText = (b: Block | null): b is TextBlock => b?.type === "Text" || b?.type === "Heading";

/**
 * The block editor. The document is `history.present`, the one source of truth; every change is an
 * `Op` through `commit`, so undo is a stack of documents and a rejected op changes nothing. One
 * block is selected at a time and every operation acts on it. There is no multi-block selection,
 * so there is nothing to offer for one.
 *
 * Typing is one undo step per burst: the first keystroke commits, the ones after it amend that
 * step, and a blur, a pause, or any other edit ends the burst.
 */
export function BlockEditor({
  initial,
  onChange,
  source = previewSource,
}: {
  initial: Doc;
  onChange?: (doc: Doc) => void;
  source?: PreviewSource;
}) {
  const [history, setHistory] = useState<History>(() => start(initial));
  const [selected, setSelected] = useState<Loc | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [sel, setSel] = useState<(Sel & { key: string }) | null>(null);
  const [restore, setRestore] = useState<(Sel & { key: string }) | null>(null);
  const [drag, setDrag] = useState<{ from: Loc; to: number } | null>(null);
  const burst = useRef<string | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  const list = useRef<HTMLDivElement>(null);
  useEffect(() => () => clearTimeout(timer.current), []);

  function endBurst() {
    burst.current = null;
    clearTimeout(timer.current);
  }

  function show(next: History, select: Loc | null) {
    setHistory(next);
    setSelected(select);
    setProblem(null);
    onChange?.(next.present);
  }

  /** One or more ops as one undo step: the first is committed, the rest amend it. */
  function run(op: Op | Op[], select: Loc | null, how: { burst?: string; restore?: Sel } = {}) {
    const key = how.burst === undefined ? undefined : `${select?.join()}:${how.burst}`;
    try {
      const [first, ...rest] = Array.isArray(op) ? op : [op];
      let next = (key !== undefined && key === burst.current ? amend : commit)(history, first as Op);
      for (const o of rest) next = amend(next, o);
      endBurst();
      if (key !== undefined) {
        burst.current = key;
        timer.current = setTimeout(endBurst, PAUSE);
      }
      setRestore(how.restore && select ? { key: select.join(), ...how.restore } : null);
      show(next, select);
    } catch (e) {
      if (!(e instanceof OpRejected)) throw e;
      setProblem(e.message);
    }
  }

  function step(f: (h: History) => History) {
    endBurst();
    setRestore(null);
    show(f(history), null);
  }

  function add(type: Block["type"]) {
    // After the selected block, in its own list; with nothing selected, at the end of the document.
    const at = selected ? [...selected.slice(0, -1), (selected.at(-1) as number) + 1] : [history.present.blocks.length];
    run({ type: "insert", at, block: blank(type) }, at);
  }

  const get = ([i, c, j]: Loc): Block | null => {
    const top = history.present.blocks[i as number];
    if (c === undefined) return top ?? null;
    return top?.type === "Columns" ? (top.children[c]?.[j as number] ?? null) : null;
  };

  /** The selection inside the selected block's text, when there is one. */
  const selOf = (loc: Loc): Sel | null => (sel && sel.key === loc.join() ? sel : null);

  function mark(loc: Loc, block: TextBlock, m: Mark) {
    const s = selOf(loc);
    if (s && s.from !== s.to) run({ type: "update", at: loc, block: { ...block, runs: toggleMark(block.runs, s.from, s.to, m) } }, loc, { restore: s });
  }

  function link(loc: Loc, block: TextBlock, href: string | null): string | null {
    const s = selOf(loc);
    if (!s || s.from === s.to) return "Select the text to link first.";
    if (href !== null && !hrefAllowed(href)) return "Use an address that starts with http, https, mailto or tel.";
    run({ type: "update", at: loc, block: { ...block, runs: setLink(block.runs, s.from, s.to, href) } }, loc, { restore: s });
    return null;
  }

  function insertTag(loc: Loc, block: TextBlock, source: string) {
    const s = selOf(loc) ?? { from: length(block.runs), to: length(block.runs) };
    const at = s.from + 1;
    run(
      { type: "update", at: loc, block: { ...block, runs: edit(block.runs, s.from, s.to, [{ type: "Tag", source }]) } },
      loc,
      { restore: { from: at, to: at } },
    );
  }

  /**
   * Enter: split at the caret, or add a text block below at the end. One undo step. The caret was
   * located in `runs`, which is normalised, so the first op writes those runs into the block: a
   * stored document need not be normalised, and a run index from one list is wrong in the other.
   * With a selection, `runs` is first cut down to what is left of it.
   */
  function enter(loc: Loc, block: TextBlock, runs: Run[], sel: Sel) {
    const below = [...loc.slice(0, -1), (loc.at(-1) as number) + 1];
    const now = sel.from === sel.to ? runs : edit(runs, sel.from, sel.to);
    // Split names a run and an offset in it, so a merge tag has only two edges and is never cut.
    const ops: Op[] = [
      { type: "update", at: loc, block: { ...block, runs: now } },
      sel.from >= length(now)
        ? { type: "insert", at: below, block: blank("Text") }
        : { type: "split", at: loc, ...locate(now, sel.from) },
    ];
    run(ops, below, { restore: { from: 0, to: 0 } });
  }

  function row(block: Block, loc: Loc, count: number) {
    const key = loc.join();
    const isSelected = same(selected, loc);
    const parent = loc.slice(0, -1);
    const index = loc.at(-1) as number;
    const go = (d: number) => run({ type: "move", from: loc, to: [...parent, index + d] }, [...parent, index + d]);
    const dragging = drag && same(drag.from, loc);
    // Where the dragged block will land: before the row now at that slot, or after the last row.
    let drop: string | undefined;
    if (drag && !dragging && drag.from.slice(0, -1).join() === parent.join()) {
      const rank = index < (drag.from.at(-1) as number) ? index : index - 1;
      if (rank === drag.to) drop = "0 -4px 0 0 var(--accent)";
      else if (rank === count - 2 && drag.to === count - 1) drop = "0 4px 0 0 var(--accent)";
    }
    const surface: Surface = {
      change: (b, how) => run({ type: "update", at: loc, block: b }, loc, how),
      focus: () => !isSelected && setSelected(loc),
      select: (s) => setSel((p) => (p && p.key === key && p.from === s.from && p.to === s.to ? p : { key, ...s })),
      mark: (m) => isText(block) && mark(loc, block, m),
      enter: (runs, s) => isText(block) && enter(loc, block, runs, s),
      restore: restore && restore.key === key ? restore : null,
    };
    const columns =
      block.type === "Columns"
        ? block.children.map((children, c) => (
            <div key={c} role="group" aria-label={`Column ${c + 1} of ${label(block, loc)}`} className="min-h-12 space-y-2">
              {children.map((child, j) => row(child, [...loc, c, j], children.length))}
            </div>
          ))
        : undefined;
    return (
      <div
        key={key}
        data-list={parent.join()}
        data-index={index}
        style={drop ? { boxShadow: drop } : undefined}
        className={`rounded-sm border p-2 ${dragging ? "opacity-60" : ""} ${isSelected ? "border-accent ring-2 ring-accent" : "border-border"}`}
      >
        <div className="flex flex-wrap items-center gap-1">
          {/* Pointer Events, not drag-and-drop: capture keeps the move events coming outside the handle. */}
          <Button
            variant="ghost"
            aria-label={`Drag ${label(block, loc)} to reorder`}
            title="Drag to reorder"
            className="cursor-grab touch-none select-none"
            onPointerDown={(e) => {
              if (e.button !== 0) return;
              e.currentTarget.setPointerCapture(e.pointerId);
              setDrag({ from: loc, to: index });
            }}
            onPointerMove={(e) => {
              if (!drag || !dragging) return;
              const rows = [...(list.current?.querySelectorAll<HTMLElement>(`[data-list="${parent.join()}"]`) ?? [])];
              const to = rows.filter((r) => r.dataset["index"] !== String(index) && e.clientY > r.getBoundingClientRect().top + r.getBoundingClientRect().height / 2).length;
              if (to !== drag.to) setDrag({ from: loc, to });
            }}
            onPointerUp={() => {
              setDrag(null);
              if (drag && dragging && drag.to !== index) run({ type: "move", from: loc, to: [...parent, drag.to] }, [...parent, drag.to]);
            }}
            onPointerCancel={() => setDrag(null)}
          >
            ⋮⋮
          </Button>
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
              <Button variant="ghost" disabled={index === 0} onClick={() => go(-1)}>
                Move up
              </Button>
              <Button variant="ghost" disabled={index === count - 1} onClick={() => go(1)}>
                Move down
              </Button>
              <Button variant="danger-ghost" onClick={() => run({ type: "delete", at: loc }, null)}>
                Delete block
              </Button>
            </>
          )}
        </div>
        <div className="mt-2">
          <BlockView block={block} label={label(block, loc)} s={surface} columns={columns} />
        </div>
      </div>
    );
  }

  const { blocks } = history.present;
  const current = selected ? get(selected) : null;
  return (
    <section aria-label="Block editor" className="space-y-3" onBlur={endBurst}>
      <div role="toolbar" aria-label="Editor" className="flex flex-wrap gap-2">
        {PALETTE.map((type) => (
          <Button key={type} onClick={() => add(type)}>
            Add {type.toLowerCase()}
          </Button>
        ))}
        <Button disabled={history.past.length === 0} onClick={() => step(undo)}>
          Undo
        </Button>
        <Button disabled={history.future.length === 0} onClick={() => step(redo)}>
          Redo
        </Button>
      </div>
      {problem && (
        <p role="alert" className="text-danger-text">
          {problem}
        </p>
      )}
      <div className="gap-6 lg:grid lg:grid-cols-[minmax(0,1fr)_20rem]">
        <div ref={list} className="space-y-2">
          {blocks.length === 0 ? <p className="text-fg-muted">No blocks yet. Add one above.</p> : null}
          {blocks.map((b, i) => row(b, [i], blocks.length))}
        </div>
        <aside aria-label="Block properties" className="mt-4 self-start lg:sticky lg:top-4 lg:mt-0">
          <Properties
            key={selected?.join() ?? "none"}
            block={current}
            id={`p-${selected?.join("-") ?? "none"}`}
            change={(b, field) => selected && run({ type: "update", at: selected, block: b }, selected, field ? { burst: field } : {})}
            text={{
              sel: selected ? selOf(selected) : null,
              mark: (m) => selected && isText(current) && mark(selected, current, m),
              link: (href) => (selected && isText(current) ? link(selected, current, href) : null),
              tag: (source) => selected && isText(current) && insertTag(selected, current, source),
            }}
            addTo={(column, type) => {
              if (selected && current?.type === "Columns") {
                const at = [...selected, column, current.children[column]?.length ?? 0];
                run({ type: "insert", at, block: blank(type) }, at);
              }
            }}
          />
        </aside>
      </div>
      <Preview doc={history.present} source={source} onLocate={(at) => { endBurst(); setSelected(at); }} />
    </section>
  );
}
