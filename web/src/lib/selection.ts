// SPDX-License-Identifier: AGPL-3.0-only

/**
 * What is selected in a table that may match ten million rows.
 *
 * A selection is a predicate, never a list of every id: "all matching" holds a signed server
 * token plus the few ids the user ticked back off. Ticking a handful of rows stays a plain set.
 */
export type Selection =
  | { mode: "none" }
  | { mode: "ids"; ids: ReadonlySet<string> }
  | {
      mode: "all";
      token: string;
      /** The frozen set the server wrote down, for `GET /contacts?snapshot_id=`. */
      snapshot: string;
      matched: number;
      excluded: ReadonlySet<string>;
    };

/** A selection made on another screen, waiting for the contacts grid to pick it up. */
export type Handover = { token: string; snapshot: string; matched: number };

let handed: Handover | null = null;

/**
 * Hands a selection to the contacts grid, which reads it when it mounts and then forgets it.
 *
 * Module state rather than the URL or `history.state`: the token is signed, and a URL would put
 * it in the back/forward stack, in the referer header and in every proxy log on the way. It
 * therefore dies with the tab, which is the safer direction anyway — a selection that outlives
 * the page is one the person may not remember making.
 */
export function handOver(selection: Handover | null): void {
  handed = selection;
}

/** Pure, so React may call it twice while rendering; the grid clears it with `handOver(null)`. */
export function handedOver(): Handover | null {
  return handed;
}

/** The server refuses more than 10,000 `excluded_ids` and 10,000 `ids`. */
export const MAX_EXCLUDED = 10_000;
export const MAX_IDS = 10_000;

export type SelectionStore = ReturnType<typeof createSelection>;

/**
 * An external store, read through `useSyncExternalStore`. Each row asks only `has(its id)`, so
 * ticking one box re-renders one row; a `useState<Set>` in the table would re-render all of them.
 */
export function createSelection() {
  let state: Selection = { mode: "none" };
  const listeners = new Set<() => void>();
  const set = (next: Selection) => {
    state = next;
    for (const listener of listeners) listener();
  };

  return {
    subscribe(listener: () => void) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    get: () => state,
    has(id: string): boolean {
      if (state.mode === "ids") return state.ids.has(id);
      if (state.mode === "all") return !state.excluded.has(id);
      return false;
    },
    /** `false` when a cap refused it: the caller says so, the selection is unchanged. */
    toggle(id: string): boolean {
      if (state.mode === "ids") {
        const ids = new Set(state.ids);
        if (ids.delete(id)) set(ids.size ? { mode: "ids", ids } : { mode: "none" });
        else if (ids.size >= MAX_IDS) return false;
        else set({ mode: "ids", ids: ids.add(id) });
      } else if (state.mode === "all") {
        const excluded = new Set(state.excluded);
        if (!excluded.delete(id)) {
          if (excluded.size >= MAX_EXCLUDED) return false;
          excluded.add(id);
        }
        set({ ...state, excluded });
      } else {
        set({ mode: "ids", ids: new Set([id]) });
      }
      return true;
    },
    /** Selects these ids, at most `MAX_IDS`. Returns how many were left out by the cap. */
    selectIds(ids: readonly string[]): number {
      set(ids.length ? { mode: "ids", ids: new Set(ids.slice(0, MAX_IDS)) } : { mode: "none" });
      return Math.max(0, ids.length - MAX_IDS);
    },
    /** `matched` is how many contacts the snapshot froze, which is exact by construction. */
    selectAll: (token: string, snapshot: string, matched: number) =>
      set({ mode: "all", token, snapshot, matched, excluded: new Set() }),
    clear() {
      if (state.mode !== "none") set({ mode: "none" });
    },
  };
}
