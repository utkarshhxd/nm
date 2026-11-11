// SPDX-License-Identifier: AGPL-3.0-only

import { useVirtualizer } from "@tanstack/react-virtual";
import {
  type FocusEvent,
  type KeyboardEvent,
  type ReactNode,
  memo,
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";

import type { SelectionStore } from "../lib/selection";
import { Checkbox } from "./checkbox";

export type Column<T> = {
  id: string;
  header: string;
  /** A CSS width; the one column without one takes the rest. */
  width?: string;
  sortable?: boolean;
  /** Dropped when the grid's own container is under 40rem, not the viewport. */
  narrowHidden?: boolean;
  cell: (row: T) => ReactNode;
};

export type Sort = { id: string; dir: "asc" | "desc" };

export type Selecting<T> = {
  store: SelectionStore;
  /** The checkbox's accessible name, e.g. `Select jo@example.com`. Never a bare "Select". */
  label: (row: T) => string;
  onToggle: (id: string) => void;
  onToggleAll: () => void;
  /** Ctrl/Cmd+A: the loaded rows only. */
  onSelectLoaded: () => void;
  onClear: () => void;
};

type Props<T> = {
  caption: string;
  columns: Column<T>[];
  rows: T[];
  getId: (row: T) => string;
  /** 0-based position of `rows[0]` in the whole result: pages dropped off the front shift it. */
  rowOffset: number;
  /** The exact number of rows when the end has been reached, else `null` (an estimate is not a count). */
  total: number | null;
  sort?: Sort;
  onSort?: (id: string) => void;
  selecting?: Selecting<T>;
  /** Shows the previous data dimmed while the next arrives, rather than unmounting to a spinner. */
  busy?: boolean;
  onNearStart?: () => void;
  onNearEnd?: () => void;
};

/** Rows from an edge at which the next page is asked for: more than a screen plus the overscan. */
const NEAR = 20;
const noopSubscribe = () => () => {};

/** `--row-h` in pixels, once: 2.25rem, or 2.75rem on a coarse pointer. */
function rowPx(): number {
  const rem = parseFloat(getComputedStyle(document.documentElement).fontSize) || 16;
  return rem * (matchMedia("(pointer: coarse)").matches ? 2.75 : 2.25);
}

/**
 * A real `<table>` over a virtualised window: two spacer rows stand in for everything above and
 * below the ~40 rendered rows, so sticky headers and table semantics survive. It knows nothing
 * about the other 9,999,000 rows and never holds more than the caller gives it.
 *
 * `role="grid"` with a row-level roving tabindex: the rows are one tab stop, arrow keys move
 * between them, and the checkbox and link inside a row are `tabindex=-1`. `aria-rowindex` is on
 * every rendered row and `aria-rowcount` is `-1` until the real total is known; without them a
 * screen reader announces "row 3 of 40" for whatever happens to be rendered third.
 *
 * Tab stops: the rows are ONE stop however many are loaded; the header's select-all checkbox and
 * its sort buttons are ordinary stops, so the grid is a small, fixed number of them (four on the
 * contacts table). That is deliberate and bounded: the rule exists so nobody tabs through a
 * million rows, and three header controls that Tab reaches directly are more discoverable than
 * arrow-only access. Do not "fix" it by roving the header row too, and do not let it grow with
 * the data.
 *
 * Remount it (`key`) when the query changes: scroll and focus then start over by construction.
 *
 * shortcut: no cell navigation, Shift-range selection, Ctrl+Home or sticky identity column; add
 * them when a second table needs them.
 */
export function DataGrid<T>({
  caption,
  columns,
  rows,
  getId,
  rowOffset,
  total,
  sort,
  onSort,
  selecting,
  busy,
  onNearStart,
  onNearEnd,
}: Props<T>) {
  const scrollerRef = useRef<HTMLDivElement>(null);
  const bodyRef = useRef<HTMLTableSectionElement>(null);
  const [px] = useState(rowPx);
  // A global index, so it stays on the same contact when pages are dropped off the front.
  const [active, setActive] = useState(rowOffset);
  const pendingFocus = useRef(false);
  const colCount = columns.length + (selecting ? 1 : 0);

  const itemKey = useCallback((index: number) => getId(rows[index] as T), [rows, getId]);
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollerRef.current,
    estimateSize: () => px,
    overscan: 8,
    // The sticky header sits above the first row inside the same scroller.
    scrollMargin: px,
    scrollPaddingStart: px,
    getItemKey: itemKey,
  });
  const items = virtualizer.getVirtualItems();
  const first = items[0];
  const last = items[items.length - 1];
  const padTop = first ? first.start - px : 0;
  const padBottom = last ? virtualizer.getTotalSize() - (last.end - px) : 0;

  // Rows dropped off the front (or fetched back onto it) move everything: keep the same row
  // under the same pixel.
  const prevOffset = useRef(rowOffset);
  useLayoutEffect(() => {
    const shift = prevOffset.current - rowOffset;
    prevOffset.current = rowOffset;
    if (shift && scrollerRef.current) scrollerRef.current.scrollTop += shift * px;
  }, [rowOffset, px]);

  // Read from the DOM, not from `items`: right after the shift above the virtualizer has not seen
  // the scroll yet, and a stale range would ask for page after page.
  const firstIndex = first?.index ?? 0;
  const lastIndex = last?.index ?? -1;
  useEffect(() => {
    const el = scrollerRef.current;
    if (!el || rows.length === 0) return;
    const top = Math.floor(el.scrollTop / px);
    const bottom = Math.ceil((el.scrollTop + el.clientHeight) / px);
    if (bottom >= rows.length - NEAR) onNearEnd?.();
    if (top <= NEAR) onNearStart?.();
    // The callbacks are redefined every render; what matters is where we are and how much we have.
  }, [firstIndex, lastIndex, rows.length, rowOffset]); // eslint-disable-line react-hooks/exhaustive-deps

  const activeLocal = Math.min(Math.max(active - rowOffset, 0), rows.length - 1);
  // The last-active row if it is rendered, else the first rendered one: the grid never loses its tab stop.
  const tabbable =
    first && last && activeLocal >= first.index && activeLocal <= last.index
      ? activeLocal
      : (first?.index ?? 0);

  function move(to: number) {
    const local = Math.min(Math.max(to, 0), rows.length - 1);
    setActive(rowOffset + local);
    pendingFocus.current = true;
    virtualizer.scrollToIndex(local);
  }
  useLayoutEffect(() => {
    if (!pendingFocus.current) return;
    const row = bodyRef.current?.querySelector<HTMLElement>(`tr[data-index="${activeLocal}"]`);
    if (row) {
      row.focus({ preventScroll: true });
      pendingFocus.current = false;
    }
  });

  function onKeyDown(event: KeyboardEvent) {
    const row = (event.target as HTMLElement).closest<HTMLElement>("tbody tr[data-index]");
    if (!row) return;
    const here = Number(row.dataset["index"]);
    const screen = Math.max(1, Math.floor((scrollerRef.current?.clientHeight ?? 0) / px) - 1);
    const go = (to: number) => {
      event.preventDefault();
      move(to);
    };
    switch (event.key) {
      case "ArrowDown":
        return go(here + 1);
      case "ArrowUp":
        return go(here - 1);
      case "Home":
        return go(0);
      case "End":
        return go(rows.length - 1);
      case "PageDown":
        return go(here + screen);
      case "PageUp":
        return go(here - screen);
      case " ":
        // A checkbox inside the row toggles itself; this is the row's own Space.
        if (event.target === row && selecting) {
          event.preventDefault();
          selecting.onToggle(getId(rows[here] as T));
        }
        return;
      case "Enter":
        // The row is the tab stop, so its link (`tabindex=-1`) is opened from the row itself.
        if (event.target === row) row.querySelector("a")?.click();
        return;
      case "Escape":
        return selecting?.onClear();
      case "a":
        if ((event.ctrlKey || event.metaKey) && selecting) {
          event.preventDefault();
          selecting.onSelectLoaded();
        }
    }
  }

  function onFocus(event: FocusEvent) {
    const row = (event.target as HTMLElement).closest<HTMLElement>("tbody tr[data-index]");
    if (row) setActive(rowOffset + Number(row.dataset["index"]));
  }

  return (
    <div
      ref={scrollerRef}
      data-scroller
      aria-busy={busy || undefined}
      className={
        "@container relative max-h-[70vh] overflow-auto rounded-md border border-border bg-surface" +
        (busy ? " opacity-60" : "")
      }
    >
      <table
        role="grid"
        aria-rowcount={total === null ? -1 : total + 1}
        aria-colcount={colCount}
        className="w-full table-fixed border-collapse text-left"
        onKeyDown={onKeyDown}
        onFocus={onFocus}
      >
        <caption className="sr-only">{caption}</caption>
        <colgroup>
          {selecting ? <col style={{ width: "3rem" }} /> : null}
          {columns.map((c) => (
            <col
              key={c.id}
              style={c.width ? { width: c.width } : undefined}
              className={c.narrowHidden ? "@max-[40rem]:hidden" : undefined}
            />
          ))}
        </colgroup>
        <thead>
          <tr aria-rowindex={1} className="h-(--row-h)">
            {selecting ? (
              <th scope="col" aria-colindex={1} className={HEAD}>
                <SelectAll rows={rows} getId={getId} selecting={selecting} />
              </th>
            ) : null}
            {columns.map((c, n) => (
              <th
                key={c.id}
                scope="col"
                aria-colindex={n + 1 + (selecting ? 1 : 0)}
                aria-sort={
                  sort?.id === c.id ? (sort.dir === "asc" ? "ascending" : "descending") : undefined
                }
                className={`${HEAD} px-3${c.narrowHidden ? " @max-[40rem]:hidden" : ""}`}
              >
                {c.sortable && onSort ? (
                  <button
                    type="button"
                    className="inline-flex items-center gap-1 font-medium hover:text-accent-text"
                    onClick={() => onSort(c.id)}
                  >
                    {c.header}
                    <span aria-hidden="true" className="text-fg-subtle">
                      {sort?.id === c.id ? (sort.dir === "asc" ? "▲" : "▼") : ""}
                    </span>
                  </button>
                ) : (
                  c.header
                )}
              </th>
            ))}
          </tr>
        </thead>
        <tbody ref={bodyRef}>
          <tr role="presentation" style={{ height: padTop }}>
            <td role="presentation" colSpan={colCount} className="p-0" />
          </tr>
          {items.map((item) => {
            const row = rows[item.index] as T;
            return (
              <GridRow
                key={item.key}
                row={row}
                id={getId(row)}
                local={item.index}
                ariaIndex={rowOffset + item.index + 2}
                tabbable={item.index === tabbable}
                columns={columns}
                selecting={selecting}
              />
            );
          })}
          <tr role="presentation" style={{ height: padBottom }}>
            <td role="presentation" colSpan={colCount} className="p-0" />
          </tr>
        </tbody>
      </table>
    </div>
  );
}

// Opaque, so rows scrolling under it do not show through; `top-0` on the cell, not the `<thead>`.
const HEAD = "sticky top-0 z-2 border-b border-border bg-sunken text-left font-medium";

type RowProps<T> = {
  row: T;
  id: string;
  local: number;
  ariaIndex: number;
  tabbable: boolean;
  columns: Column<T>[];
  selecting: Selecting<T> | undefined;
};

/** Memoised: a scroll frame re-renders the list, and only rows whose own props moved redraw. */
const GridRow = memo(function GridRow<T>({
  row,
  id,
  local,
  ariaIndex,
  tabbable,
  columns,
  selecting,
}: RowProps<T>) {
  const selected = useSyncExternalStore(
    selecting?.store.subscribe ?? noopSubscribe,
    () => selecting?.store.has(id) ?? false,
  );
  return (
    <tr
      data-index={local}
      aria-rowindex={ariaIndex}
      aria-selected={selecting ? selected : undefined}
      tabIndex={tabbable ? 0 : -1}
      className={
        "group h-(--row-h) border-b border-border hover:bg-row-hover aria-selected:bg-row-selected " +
        "focus-visible:outline-offset-[-2px]"
      }
    >
      {selecting ? (
        <td
          aria-colindex={1}
          // The fourth cue for a selected row, after tint, checkbox and aria-selected.
          className="group-aria-selected:shadow-[inset_3px_0_0_0_var(--accent)]"
        >
          <Checkbox
            tabIndex={-1}
            checked={selected}
            onChange={() => selecting.onToggle(id)}
            aria-label={selecting.label(row)}
          />
        </td>
      ) : null}
      {columns.map((c, n) => (
        <td
          key={c.id}
          aria-colindex={n + 1 + (selecting ? 1 : 0)}
          className={`truncate px-3${c.narrowHidden ? " @max-[40rem]:hidden" : ""}`}
        >
          {c.cell(row)}
        </td>
      ))}
    </tr>
  );
}) as <T>(props: RowProps<T>) => ReactNode;

/** Re-renders on every selection change, which is why it is the one place besides the bar that does. */
function SelectAll<T>({
  rows,
  getId,
  selecting,
}: {
  rows: T[];
  getId: (row: T) => string;
  selecting: Selecting<T>;
}) {
  const state = useSyncExternalStore(selecting.store.subscribe, selecting.store.get);
  const ticked = rows.reduce((n, row) => n + (selecting.store.has(getId(row)) ? 1 : 0), 0);
  const all = state.mode !== "none" && ticked === rows.length;
  return (
    <Checkbox
      aria-label="Select all loaded contacts"
      checked={all}
      indeterminate={state.mode !== "none" && !all}
      onChange={selecting.onToggleAll}
    />
  );
}
