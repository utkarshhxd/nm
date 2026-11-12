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
  | { mode: "all"; token: string; matched: number; excluded: ReadonlySet<string> };

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
    /** `matched` is how many contacts the token's filter matched when it was issued. */
    selectAll: (token: string, matched: number) =>
      set({ mode: "all", token, matched, excluded: new Set() }),
    clear() {
      if (state.mode !== "none") set({ mode: "none" });
    },
  };
}
