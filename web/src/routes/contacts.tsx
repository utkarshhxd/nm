// SPDX-License-Identifier: AGPL-3.0-only

import {
  keepPreviousData,
  useInfiniteQuery,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";

import { api, problemMessage, problemType } from "../api/client";
import type { components } from "../api/schema";
import { Button } from "../components/button";
import { ConfirmDialog } from "../components/confirm-dialog";
import { ContactForm } from "../components/contact-form";
import { type Column, DataGrid, type Sort } from "../components/data-grid";
import { Dialog } from "../components/dialog";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Input } from "../components/input";
import { Select } from "../components/select";
import { Skeleton } from "../components/skeleton";
import { atLeast } from "../lib/roles";
import { MAX_EXCLUDED, MAX_IDS, createSelection, handOver, handedOver } from "../lib/selection";
import { Alert } from "../lib/ui";
import { useWorkspace } from "./workspace-layout";

type Contact = components["schemas"]["Contact"];
type Status = components["schemas"]["MembershipStatus"];

/** A page. The server allows 200; 100 keeps `maxPages: 20` at the 2,000 rows the brief bounds. */
const PAGE = 100;
const MAX_PAGES = 20;

/** The server answers 422 for a `q` under 3 characters, so the UI never sends one. */
const MIN_QUERY = 3;

const NAMES: Record<Status, string> = {
  pending: "Pending",
  subscribed: "Subscribed",
  unsubscribed: "Unsubscribed",
};

/**
 * The fields of a view that decide **which** contacts it covers: the `NewSelection` body, exactly,
 * and nothing else. `sort` and `dir` are not among them — re-ordering the grid changes not one row
 * that would be selected or exported.
 *
 * It is written once and used three ways: as both request bodies, and as the key `matched` is
 * stored under. Two lists that have to agree and are not made to is how the dialog came to say
 * "this is not the 3 contacts you selected" about the very three it would export, after a click
 * on a column header.
 */
const covered = (f: Filters) => ({
  q: f.q,
  list_id: f.list_id,
  list_status: f.list_status,
  tag_id: f.tag_id,
});

const fullName = (c: Contact) => `${c.first_name} ${c.last_name}`.trim();
const date = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });
const count = (n: number, noun: string) => `${n.toLocaleString()} ${noun}${n === 1 ? "" : "s"}`;

// The link is out of the tab order: the row is the grid's one tab stop, and Enter on it follows the link.
const columns = (slug: string): Column<Contact>[] => [
  { id: "name", header: "Name", width: "20ch", title: fullName, cell: (c) => fullName(c) || "—" },
  {
    id: "email",
    header: "Email",
    sortable: true,
    title: (c) => c.email,
    cell: (c) => (
      <Link
        to="/w/$slug/contacts/$contactId"
        params={{ slug, contactId: c.id }}
        tabIndex={-1}
        data-row-link
        className="text-accent-text underline"
      >
        {c.email}
      </Link>
    ),
  },
  {
    id: "created",
    header: "Added",
    width: "14ch",
    sortable: true,
    narrowHidden: true,
    cell: (c) => <span className="tabular-nums text-fg-muted">{date.format(new Date(c.created_at))}</span>,
  },
];

export default function Contacts() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return (
    <ContactsPage
      id={workspace.id}
      slug={workspace.slug}
      canEdit={atLeast(workspace.role, "editor")}
      // Spec §13: export is admin. The control is absent below rather than disabled — a greyed-out
      // Export tells every editor exactly which door to knock on.
      canExport={atLeast(workspace.role, "admin")}
    />
  );
}

type Filters = {
  q?: string;
  list_id?: string;
  list_status?: Status;
  tag_id?: string;
  sort: "created" | "email";
  dir: "asc" | "desc";
};

type Result = { tone: "ok" | "warn"; text: string };

/** The five bulk routes. Three of them are confirmed first; see `Confirming`. */
type Kind = "tag" | "untag" | "subscribe" | "unsubscribe" | "delete";

/**
 * Problem codes that mean the selection in hand can never be used again: its hour is up, its
 * snapshot is gone, or it would reach more contacts than the bar said. Repeating the request
 * cannot help, so the selection is dropped and the view refetched.
 */
const STALE_SELECTION = new Set([
  "selection-expired",
  "selection-invalid",
  "selection-count-mismatch",
]);
type Confirming = {
  kind: "tag" | "unsubscribe" | "delete";
  target: string;
  name: string;
  /**
   * How many contacts the dialog said it would act on, read when it opened. A delete is measured
   * against this and not against the server's `selected`: those two always agree (both come out
   * of the one statement), so a contact someone else deleted while the dialog was open would
   * otherwise leave the person told "deleted 4,980" after agreeing to 5,000, with nothing said.
   */
  promised: number;
};

/** The sentence the locked tick boxes point at, so it reaches a screen reader and not only a screen. */
const WHY_LOCKED = "frozen-page-loading";

/** The title and the button of each confirmation, so the dialog is not three nested ternaries. */
const CONFIRM: Record<Confirming["kind"], { title: string; label: (n: number) => string }> = {
  tag: { title: "Tag these contacts", label: (n) => `Tag ${count(n, "contact")}` },
  unsubscribe: {
    title: "Unsubscribe from this list",
    label: () => "Unsubscribe these contacts",
  },
  delete: { title: "Delete these contacts", label: (n) => `Delete ${count(n, "contact")}` },
};

function ContactsPage({
  id,
  slug,
  canEdit,
  canExport,
}: {
  id: string;
  slug: string;
  canEdit: boolean;
  canExport: boolean;
}) {
  const queryClient = useQueryClient();
  const navigate = useNavigate({ from: "/w/$slug/contacts" });
  const search = useSearch({ from: "/w/$slug/contacts" });

  const sort = search.sort ?? "created";
  const filters: Filters = {
    q: search.q && search.q.length >= MIN_QUERY ? search.q : undefined,
    list_id: search.list_id,
    // The server refuses a status with no list.
    list_status: search.list_id ? search.list_status : undefined,
    tag_id: search.tag_id,
    sort,
    dir: search.dir ?? (sort === "email" ? "asc" : "desc"),
  };
  // Two keys, deliberately. `filterKey` is the grid's identity, so it carries `sort` and `dir`:
  // ordered differently, it really is a different view and a different set of page cursors.
  // `coveredKey` is which contacts a request would reach, which the order has nothing to do with.
  const filterKey = JSON.stringify(filters);
  const coveredKey = JSON.stringify(covered(filters));
  const narrowed = Boolean(filters.q || filters.list_id || filters.tag_id);

  const setSearch = (patch: Partial<typeof search>) =>
    void navigate({ search: (prev) => ({ ...prev, ...patch }), replace: true });

  // One polite live region for the whole table, debounced so typing does not stream announcements.
  const [announcement, setAnnouncement] = useState("");
  const announceTimer = useRef<number>(0);
  const announce = (text: string) => {
    window.clearTimeout(announceTimer.current);
    announceTimer.current = window.setTimeout(() => setAnnouncement(text), 500);
  };

  const lists = useQuery({
    // The pickers' key, shared on purpose with the contact page's: same request, same shape, one
    // cache entry. The lists screen's own paged query is ["lists", ws, "all"] for that reason.
    queryKey: ["lists", id],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/lists", {
        params: { path: { workspace_id: id }, query: { limit: 200 } },
      });
      return data?.data ?? [];
    },
  });
  const tags = useQuery({
    queryKey: ["tags", id],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/tags", {
        params: { path: { workspace_id: id }, query: { limit: 200 } },
      });
      return data?.data ?? [];
    },
  });

  /**
   * The selection, which the grid's own query reads: once it is a frozen set, the grid shows
   * **that set** rather than the live filter (spec §14, "here is what you selected").
   *
   * A selection handed over by another screen — the segments list — is picked up as this mounts,
   * and forgotten immediately, so returning here later never re-applies it.
   */
  const store = useMemo(() => {
    const made = createSelection();
    const handoff = handedOver(id);
    if (handoff) made.selectAll(handoff.token, handoff.snapshot, handoff.matched);
    return made;
  }, []);
  useEffect(() => handOver(null), []);
  const selection = useSyncExternalStore(store.subscribe, store.get);
  const snapshot = selection.mode === "all" ? selection.snapshot : undefined;
  // The grid's identity: two different sets of rows must never share one cache entry, and a
  // snapshot's page cursors are not the live filter's.
  const viewKey = `${filterKey}|${snapshot ?? ""}`;

  // Page start -> the cursor that fetched it. The API only pages forward, so this is how a page
  // dropped off the front by `maxPages` is fetched again when the user scrolls back.
  const cursors = useMemo(() => new Map<number, string | undefined>(), [viewKey]);
  const query = useInfiniteQuery({
    queryKey: ["contacts", id, viewKey],
    initialPageParam: { start: 0 } as { cursor?: string; start: number },
    queryFn: async ({ pageParam, signal }) => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/contacts", {
        params: {
          path: { workspace_id: id },
          query: { ...filters, snapshot_id: snapshot, limit: PAGE, cursor: pageParam.cursor },
        },
        signal,
      });
      if (error || !data) {
        // The same three codes the bulk path treats as a selection that can never be used again.
        // Only `selection-expired` can reach the read path, and a Retry against it refuses for
        // as long as the page is open — so the selection goes and the grid reloads itself as the
        // live view, which is the recovery `bulk` already performs.
        if (STALE_SELECTION.has(problemType(error).replace("urn:nm:problem:", ""))) store.clear();
        throw new Error(problemMessage(error, "The contacts could not be loaded."));
      }
      cursors.set(pageParam.start, pageParam.cursor);
      return { rows: data.data, next: data.next_cursor ?? null, start: pageParam.start };
    },
    getNextPageParam: (last) =>
      last.next ? { cursor: last.next, start: last.start + last.rows.length } : undefined,
    getPreviousPageParam: (first) => {
      const start = first.start - PAGE;
      return start >= 0 && cursors.has(start) ? { cursor: cursors.get(start), start } : undefined;
    },
    // 20 pages of 100: the browser holds at most 2,000 rows however far someone scrolls.
    maxPages: MAX_PAGES,
    placeholderData: keepPreviousData,
    retry: false,
    // Every refetch is up to 20 sequential requests; a window gaining focus is not worth that.
    refetchOnWindowFocus: false,
  });

  const rows = useMemo(() => query.data?.pages.flatMap((p) => p.rows) ?? [], [query.data]);
  const rowOffset = query.data?.pages[0]?.start ?? 0;
  // How many rows this view holds, known only once its last page is in.
  const shown = query.hasNextPage ? null : rowOffset + rows.length;
  // How many contacts the current filter matched, once someone has asked to select them all.
  // Until then (or after the filters change) it is unknown, and an unknown total is `-1`. Keyed
  // by what the count is a count *of*, so sorting the grid does not throw the number away.
  const [matched, setMatched] = useState<{ key: string; n: number } | null>(null);
  const total = shown ?? (matched?.key === coveredKey ? matched.n : null);

  // Remounts the grid (scroll, focus, virtualiser) once the new query's data is what is shown.
  const shownKey = useRef(viewKey);
  if (!query.isPlaceholderData) shownKey.current = viewKey;

  const [result, setResult] = useState<Result | null>(null);
  const resultRef = useRef<HTMLParagraphElement>(null);
  const toFocus = useRef(false);
  const [failure, setFailure] = useState("");
  const [creating, setCreating] = useState(false);
  const [confirming, setConfirming] = useState<Confirming | null>(null);
  const [exporting, setExporting] = useState(false);
  const [exportBusy, setExportBusy] = useState(false);
  const [busy, setBusy] = useState(false);
  const [counting, setCounting] = useState(false);
  // What the in-flight selection was asked for, so a token that arrives for a view nobody is on
  // any more is dropped. Sorting is not such a change: it moves no contact in or out.
  const live = useRef(coveredKey);
  live.current = coveredKey;

  // Ticked rows are rows somebody saw in *this* view, so changing the filter clears them: a
  // selection carried across a filter is how someone acts on the wrong million rows. A frozen
  // selection is different — it is written down on the server, the grid is narrowed to it, and a
  // filter from here narrows only what is shown (spec §14).
  useEffect(() => {
    if (store.get().mode === "ids") {
      store.clear();
      announce("Selection cleared because the filters changed.");
    }
    setResult(null);
    setFailure("");
  }, [filterKey]); // eslint-disable-line react-hooks/exhaustive-deps

  // How many contacts an action would run on, whichever way they were chosen.
  const chosen =
    selection.mode === "ids"
      ? selection.ids.size
      : selection.mode === "all"
        ? selection.matched - selection.excluded.size
        : 0;
  useEffect(() => {
    if (!result || !toFocus.current) return;
    toFocus.current = false;
    resultRef.current?.focus();
  }, [result]);

  const firstRun = useRef(true);
  // A bulk delete clears the selection itself; "Selection cleared" must not be announced in
  // place of what the delete did.
  const silentClear = useRef(false);
  useEffect(() => {
    if (firstRun.current) {
      firstRun.current = false;
      return;
    }
    if (silentClear.current) silentClear.current = false;
    else if (selection.mode === "none") announce("Selection cleared");
    else announce(`${count(chosen, "contact")} selected`);
  }, [selection]); // eslint-disable-line react-hooks/exhaustive-deps

  /**
   * The rows on screen are the *previous* view while the frozen page is in flight:
   * `placeholderData` keeps them, and `shownKey` deliberately does not remount the grid. One of
   * them may have left the filter before the snapshot was taken, so unticking it would exclude an
   * id the snapshot never held — the dialog's number would then be lower than what the action
   * reaches, and the server refuses with `409 selection-count-mismatch`. Ticking is off until the
   * rows are the frozen set.
   */
  const locked = query.isPlaceholderData && selection.mode === "all";

  const selecting = useMemo(
    () =>
      canEdit
        ? {
            store,
            lockedBy: locked ? WHY_LOCKED : undefined,
            label: (c: Contact) => `Select ${c.email}`,
            onToggle: (contactId: string) => {
              if (!store.toggle(contactId)) {
                setFailure(
                  store.get().mode === "all"
                    ? `You can leave out at most ${MAX_EXCLUDED.toLocaleString()} contacts. Narrow the search instead.`
                    : `You can tick at most ${MAX_IDS.toLocaleString()} contacts. Use “Select all contacts matching this view” instead.`,
                );
              }
            },
            onToggleAll: () => (store.get().mode === "none" ? selectLoaded() : store.clear()),
            onSelectLoaded: () => selectLoaded(),
            onClear: () => store.clear(),
          }
        : undefined,
    // `selectLoaded` reads `rows` through the ref below, so it is always the current one.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [canEdit, store, locked],
  );
  const loaded = useRef(rows);
  loaded.current = rows;
  function selectLoaded() {
    const left = store.selectIds(loaded.current.map((c) => c.id));
    setFailure(
      left ? `Only the first ${MAX_IDS.toLocaleString()} loaded contacts were selected.` : "",
    );
  }

  async function selectAllMatching() {
    // The count is a live count over the whole match, which is slow at ten million rows: it is only
    // asked for here, and the bar shows a skeleton until it lands.
    setCounting(true);
    setFailure("");
    const mine = coveredKey;
    const { data, error } = await api.POST("/api/v1/workspaces/{workspace_id}/contacts/selection", {
      params: { path: { workspace_id: id } },
      body: covered(filters),
    });
    setCounting(false);
    if (mine !== live.current) return; // the filters moved on; this token is for a view that is gone
    if (error || !data) {
      setFailure(problemMessage(error, "Could not select every matching contact."));
      return;
    }
    setMatched({ key: mine, n: data.selected });
    // `selected` is the materialising insert's own `rows_affected`, so it is exact by
    // construction and never an estimate (spec §5's last bullet) — which is why this control,
    // unlike the segments list's, needs no "only from an exact count" rule.
    store.selectAll(data.selection_token, data.snapshot_id, data.selected);
  }

  /**
   * The only exact count of *this* filter in hand (spec §10, ruling E2). `Page` carries no total,
   * and the one endpoint that counts a filter exactly materialises a snapshot — up to a million
   * rows written, and a bite out of `SELECTION_PER_MINUTE` — so no number is asked for to label a
   * dialog. This is the count "Select all matching" already wrote down, and it is used only while
   * the filter it was taken for is still the filter — `coveredKey`, not `filterKey`, because a
   * count is not stale for having been taken before someone clicked a column header.
   */
  const exportCount = matched?.key === coveredKey ? matched.n : null;
  /**
   * Whether the export would cover the same contacts as the selection. `POST /contacts/export`
   * takes a `NewSelection` and has no `snapshot_id`: it re-runs the filter, so it can never be
   * pointed at a frozen set. The two are the same set only when the selection *is* the whole of
   * this filter — taken from it, nothing left out, nothing changed since. Task 11 made the grid
   * show the frozen set, so in every other case the dialog has to say so out loud.
   */
  const exportIsSelection =
    selection.mode === "all" && selection.excluded.size === 0 && exportCount !== null;

  /**
   * The CSV, through `fetch` into a blob.
   *
   * shortcut: the whole file is held in browser memory before the download starts, which is what
   * §10 refuses to do on the server; upgrade to a signed short-lived GET URL the browser streams
   * straight to disk when an export outgrows a tab. `fetch` is forced either way — CSRF takes an
   * `X-CSRF-Token` header (`src/http/csrf.rs`), which neither a form post nor an `<a>` can send.
   */
  async function exportCsv() {
    if (exportBusy) return;
    setExportBusy(true);
    setFailure("");
    // A bulk action's success paragraph is a report of the last thing that happened; leaving it
    // above this one's refusal would put two of those on screen at once.
    setResult(null);
    try {
      const { data, error } = await api.POST(
        "/api/v1/workspaces/{workspace_id}/contacts/export",
        { params: { path: { workspace_id: id } }, body: covered(filters), parseAs: "blob" },
      );
      if (error || !data) {
        // The refusal arrives after the dialog was agreed to, so it lands in the page's own alert
        // rather than in the dialog `finally` is about to close. 403 and 429 are the server's own
        // sentences, like every other refusal here.
        setFailure(problemMessage(error, "The export failed and no file was downloaded."));
        return;
      }
      download(data, "contacts.csv");
    } catch {
      // No refusal to render: the connection went, or the stream was aborted part-way — which is
      // how `export.rs` reports a query that failed mid-book, rather than ending the response and
      // passing off a short file as a whole one. Reading the blob rejects, and without this the
      // dialog would simply close with nothing said.
      //
      // It covers `download` as well, on purpose: the five DOM calls in there will not throw, and
      // if they ever did, "the export did not finish, so no file was saved — try again" is still
      // both true and the right advice. Reviewed and left as is; not an oversight.
      setFailure("The export did not finish, so no file was saved. Try again.");
    } finally {
      setExportBusy(false);
      setExporting(false);
    }
  }

  async function bulk(kind: Kind, target: string, name: string, promised?: number) {
    const current = store.get();
    if (current.mode === "none" || (kind !== "delete" && !target)) return;
    setBusy(true);
    setFailure("");
    setResult(null);
    try {
      // A filter selection goes as its token and exclusions, ticked rows as their ids: the
      // server takes exactly one of the two, and applies the same rules to both. Each body
      // denies unknown fields, so a route is sent exactly the fields it declares and no more.
      //
      // `confirmed_count` is **the number on the button**: the selection's size less the rows
      // unticked here, which is what `chosen` renders and what the dialog says. The server
      // refuses the action if it would reach more than that, which is what closes "you confirmed
      // 1 and we deleted 2". Sending the selection's whole size instead would send the server a
      // number it already had signed into the token, and check nothing.
      const body =
        current.mode === "all"
          ? {
              selection_token: current.token,
              confirmed_count: current.matched - current.excluded.size,
              excluded_ids: [...current.excluded],
            }
          : { ids: [...current.ids] };
      const params = { path: { workspace_id: id } };
      const { data, error } = await (kind === "tag"
        ? api.POST("/api/v1/workspaces/{workspace_id}/contacts/bulk/tag", {
            params,
            body: { ...body, tag_id: target },
          })
        : kind === "untag"
          ? api.POST("/api/v1/workspaces/{workspace_id}/contacts/bulk/untag", {
              params,
              body: { ...body, tag_id: target },
            })
          : kind === "subscribe"
            ? api.POST("/api/v1/workspaces/{workspace_id}/contacts/bulk/subscribe", {
                params,
                body: { ...body, list_id: target },
              })
            : kind === "unsubscribe"
              ? api.POST("/api/v1/workspaces/{workspace_id}/contacts/bulk/unsubscribe", {
                  params,
                  body: { ...body, list_id: target },
                })
              : api.POST("/api/v1/workspaces/{workspace_id}/contacts/bulk/delete", {
                  params,
                  body,
                }));
      if (error || !data) {
        setFailure(problemMessage(error, "The action failed and nothing was changed."));
        // These three all mean "this selection cannot be used again": it expired, its snapshot is
        // gone, or what it would reach no longer matches what the bar showed. The server's
        // sentence says to select again, so clear the stale selection and refetch — otherwise the
        // same button repeats the same refusal for an hour.
        if (STALE_SELECTION.has(problemType(error).replace("urn:nm:problem:", ""))) {
          silentClear.current = true;
          store.clear();
          await queryClient.invalidateQueries({ queryKey: ["contacts", id] });
        }
        return;
      }
      // Deleting empties the selection: the rows it named are gone, and a stale set of ids is
      // how a second click lands on contacts that were never chosen.
      if (kind === "delete") {
        silentClear.current = true;
        store.clear();
        // The bar that held the button is about to unmount; focus goes to the outcome.
        toFocus.current = true;
      }
      // No "the view widened" line any more, and no exclusion count to compute one from: the
      // server acts on a set that was written down when the selection was made, so nothing can
      // join it afterwards, and anything it would reach beyond the confirmed number is refused
      // rather than reported.
      const message = describeOutcome(kind, name, data, promised);
      setResult(message);
      announce(message.text);
      // The table only changes when rows left it, or when it is filtered by what changed.
      if (kind === "delete" || filters.list_id || filters.tag_id) {
        await queryClient.invalidateQueries({ queryKey: ["contacts", id] });
      }
    } finally {
      setBusy(false);
    }
  }

  const cols = useMemo(() => columns(slug), [slug]);
  const gridSort: Sort = { id: filters.sort, dir: filters.dir };
  function onSort(column: string) {
    if (column !== "email" && column !== "created") return;
    const dir =
      column === filters.sort ? (filters.dir === "asc" ? "desc" : "asc") : column === "email" ? "asc" : "desc";
    setSearch({ sort: column, dir });
  }

  return (
    <section className="mt-8">
      <div className="flex flex-wrap items-baseline justify-between gap-3">
        <h2 className="text-xl font-semibold">Contacts</h2>
        <div className="flex flex-wrap items-center gap-3">
          {canExport ? <Button onClick={() => setExporting(true)}>Export CSV</Button> : null}
          {canEdit ? (
            <Button variant="solid" onClick={() => setCreating(true)}>
              New contact
            </Button>
          ) : null}
        </div>
      </div>

      <div className="mt-4">
        {/* Hidden only while rows are ticked by hand, because changing a filter throws those
            away. A frozen selection survives one, so its filters stay where they are. */}
        <div hidden={selection.mode === "ids"} className="flex flex-wrap items-start gap-3">
          <SearchBox
            initial={search.q ?? ""}
            onSearch={(q) => setSearch({ q })}
            announce={announce}
          />
          <Select
            aria-label="Filter by list"
            value={search.list_id ?? ""}
            onChange={(e) =>
              setSearch({ list_id: e.target.value || undefined, list_status: undefined })
            }
          >
            <option value="">All lists</option>
            {lists.data?.map((l) => (
              <option key={l.id} value={l.id}>
                {l.name}
              </option>
            ))}
          </Select>
          {search.list_id ? (
            <Select
              aria-label="Filter by list status"
              value={search.list_status ?? ""}
              onChange={(e) => setSearch({ list_status: (e.target.value || undefined) as Status })}
            >
              <option value="">Any status</option>
              {(Object.keys(NAMES) as Status[]).map((s) => (
                <option key={s} value={s}>
                  {NAMES[s]}
                </option>
              ))}
            </Select>
          ) : null}
          <Select
            aria-label="Filter by tag"
            value={search.tag_id ?? ""}
            onChange={(e) => setSearch({ tag_id: e.target.value || undefined })}
          >
            <option value="">All tags</option>
            {tags.data?.map((t) => (
              <option key={t.id} value={t.id}>
                {t.name}
              </option>
            ))}
          </Select>
        </div>

        {selection.mode !== "none" ? (
          <div
            role="region"
            aria-label="Bulk actions"
            className="mt-3 flex flex-wrap items-center gap-3 rounded-md border-s-4 border-accent bg-accent-subtle p-3"
          >
            {/* Never "matching this view" any more. The grid below is the frozen set, and a
                filter typed here narrows what is shown without changing what was selected, so a
                sentence tying the selection to the view would be false the moment one is. */}
            <p className="font-medium">
              {selection.mode === "ids"
                ? `${count(selection.ids.size, "contact")} selected`
                : selection.excluded.size
                  ? `Here is what you selected: ${(selection.matched - selection.excluded.size).toLocaleString()} of ${count(selection.matched, "contact")} (${selection.excluded.size.toLocaleString()} left out).`
                  : `Here is what you selected: all ${count(selection.matched, "contact")}.`}
            </p>
            {selection.mode === "ids" ? (
              counting ? (
                <div className="w-56">
                  <Skeleton label="Counting matching contacts" />
                </div>
              ) : (
                <Button variant="ghost" onClick={() => void selectAllMatching()}>
                  Select all contacts matching this view
                </Button>
              )
            ) : null}
            <BulkAction
              label="Tag to add or remove"
              placeholder="Choose a tag…"
              options={tags.data ?? []}
              disabled={busy || chosen === 0}
              actions={[
                {
                  label: "Add tag",
                  run: (target, name) =>
                    setConfirming({ kind: "tag", target, name, promised: chosen }),
                },
                { label: "Remove tag", run: (t, n) => void bulk("untag", t, n) },
              ]}
            />
            <BulkAction
              label="List to subscribe to or unsubscribe from"
              placeholder="Choose a list…"
              options={lists.data ?? []}
              disabled={busy || chosen === 0}
              actions={[
                { label: "Subscribe to list", run: (t, n) => void bulk("subscribe", t, n) },
                {
                  label: "Unsubscribe from list",
                  run: (target, name) =>
                    setConfirming({ kind: "unsubscribe", target, name, promised: chosen }),
                },
              ]}
            />
            <Button
              variant="danger-ghost"
              disabled={busy || chosen === 0}
              onClick={() =>
                setConfirming({ kind: "delete", target: "", name: "", promised: chosen })
              }
            >
              Delete contacts
            </Button>
            <Button variant="ghost" onClick={() => store.clear()}>
              Clear selection
            </Button>
          </div>
        ) : null}
      </div>

      {result ? (
        <p
          ref={resultRef}
          tabIndex={-1}
          data-testid="bulk-result"
          className={
            "mt-3 rounded-md border-s-4 p-3 " +
            (result.tone === "warn"
              ? "border-warning bg-warning-subtle text-warning-text"
              : "border-success bg-success-subtle text-success-text")
          }
        >
          {result.text}
        </p>
      ) : null}
      {failure ? <Alert>{failure}</Alert> : null}

      <div className="mt-4">
        {/* The frozen set, seen through a narrower view. Two facts, each said only when it is
            one: how many of the selection are on screen, and — only when a filter is why — that
            filtering changes what is shown and not what is selected. The figure needs the view's
            size, which is unknown until its last page is in; the explanation never does, and
            withholding it on a large selection would be silence in exactly the case it is for. */}
        {selection.mode === "all" &&
        (shown === null ? narrowed : shown < selection.matched) ? (
          <p className="mb-3 text-fg-muted">
            {shown === null
              ? `${count(selection.matched, "contact")} are selected.`
              : `Showing ${shown.toLocaleString()} of the ${count(selection.matched, "contact")} you selected.`}
            {narrowed ? " Filtering changes what is shown here, not what is selected." : ""}
          </p>
        ) : null}
        {/* Why the ticks are off. A disabled control that does not say why is a control nobody
            can act on, which is the pattern `m1a-decisions.md` settled for the bulk bar. */}
        {locked ? (
          <p id={WHY_LOCKED} className="mb-3 text-fg-muted">
            Loading the contacts you selected. These are the rows you were looking at before, so
            they cannot be ticked until it arrives.
          </p>
        ) : null}
        {query.isPending ? (
          <Skeleton label="Loading contacts" rows={8} />
        ) : query.isError && !query.data ? (
          <ErrorState title="The contacts could not be loaded." onRetry={() => void query.refetch()}>
            {query.error.message}
          </ErrorState>
        ) : rows.length === 0 ? (
          narrowed ? (
            <EmptyState
              title="No contacts match"
              actions={
                <Button onClick={() => setSearch({ q: undefined, list_id: undefined, list_status: undefined, tag_id: undefined })}>
                  Clear search and filters
                </Button>
              }
            >
              {filters.q ? `Nothing matches “${filters.q}” with these filters.` : "Nothing matches these filters."}
            </EmptyState>
          ) : (
            <EmptyState title="No contacts yet">
              Contacts you add or import will appear here.
            </EmptyState>
          )
        ) : (
          <DataGrid
            key={shownKey.current}
            caption="Contacts"
            columns={cols}
            rows={rows}
            getId={(c) => c.id}
            rowOffset={rowOffset}
            total={total}
            sort={gridSort}
            onSort={onSort}
            selecting={selecting}
            busy={query.isPlaceholderData}
            onNearEnd={() => {
              if (query.hasNextPage && !query.isFetching) void query.fetchNextPage();
            }}
            onNearStart={() => {
              if (query.hasPreviousPage && !query.isFetching) void query.fetchPreviousPage();
            }}
          />
        )}
        <p className="mt-2 min-h-6 text-fg-muted">
          {query.isFetchingNextPage || query.isFetchingPreviousPage ? (
            "Loading more…"
          ) : query.isFetchNextPageError || query.isFetchPreviousPageError ? (
            <>
              More contacts could not be loaded.{" "}
              <Button
                variant="ghost"
                onClick={() => void (query.hasNextPage ? query.fetchNextPage() : query.fetchPreviousPage())}
              >
                Retry
              </Button>
            </>
          ) : rows.length > 0 && !query.hasNextPage ? (
            "End of results"
          ) : null}
        </p>
      </div>

      <div role="status" aria-live="polite" className="sr-only">
        {announcement}
      </div>

      <ConfirmDialog
        open={confirming !== null}
        onClose={() => setConfirming(null)}
        title={confirming ? CONFIRM[confirming.kind].title : ""}
        confirmLabel={confirming ? CONFIRM[confirming.kind].label(confirming.promised) : ""}
        // Red is for what cannot be undone. "Remove tag" undoes this completely and §9.1 puts
        // `bulk/tag` on the reporting side; red two controls from the bulk delete that really is
        // irreversible is how red stops meaning anything. §9.1's weight is in the copy.
        variant={confirming?.kind === "tag" ? "solid" : "danger-solid"}
        onConfirm={() => {
          const go = confirming;
          setConfirming(null);
          if (go) void bulk(go.kind, go.target, go.name, go.promised);
        }}
      >
        {confirming?.kind === "tag" ? (
          <>
            <p>
              <strong>{count(confirming.promised, "contact")}</strong> are tagged with{" "}
              <strong className="wrap-anywhere">{confirming.name}</strong>. Anyone who already has
              it is left alone, and nothing else about them changes.
            </p>
            {/* Spec §9.1: snapshots answer *which* contacts an action reaches, structurally. What
                they cannot answer is what the operator understood they were agreeing to, and a
                tag is about to become a thing a campaign can be aimed at. */}
            <p>
              A tag is how a campaign will choose who to mail. Nothing sends to a tag yet, but once
              it can, tagging these{" "}
              <strong>{confirming.promised.toLocaleString()}</strong> contacts is what makes{" "}
              {confirming.promised === 1 ? "that person" : "those people"} mailable by anyone who
              sends to it — and nobody is asked again at that point. Tag people who agreed to hear
              from you about this.
            </p>
          </>
        ) : confirming?.kind === "delete" ? (
          <>
            <p>
              <strong>{count(confirming.promised, "contact")}</strong> and everything recorded
              against them — list memberships, tags and custom field values — are deleted. This
              cannot be undone and there is no copy to restore from.
            </p>
            <p>
              Their addresses are <strong>not suppressed</strong>. Nothing here records that these
              people were removed, so every address can be added again, by anyone, from an import
              tomorrow. To stop mailing someone for good, suppress the address instead; to honour
              an erasure request, erase the contact.
            </p>
          </>
        ) : (
          <>
            <p>
              <strong>{count(confirming?.promised ?? 0, "selected contact")}</strong> are
              unsubscribed from{" "}
              <strong className="wrap-anywhere">{confirming?.name}</strong>. Anyone who is not
              subscribed to it is left alone.
            </p>
            <p>
              This affects <strong>only this list</strong>. It is not an opt-out from the whole
              workspace, and no contact is changed or deleted. Nothing re-subscribes them in bulk
              afterwards, so do this only if they asked for it.
            </p>
          </>
        )}
      </ConfirmDialog>

      {/* Red, by the default `variant`, and for the opposite reason to the tag dialog's `solid`:
          you cannot un-download a file. §10 is explicit that admin-only "does not prevent
          exfiltration — it raises the cost and creates one audit point where there was none", and
          the red button is part of raising that cost. */}
      <ConfirmDialog
        open={exporting}
        onClose={() => setExporting(false)}
        title="Export these contacts"
        confirmLabel="Export CSV"
        busy={exportBusy}
        onConfirm={() => void exportCsv()}
      >
        <p>
          The file contains <strong>personal data</strong>: every contact&apos;s name, email
          address, tags, list memberships and custom field values, in plain text. A file that has
          been downloaded cannot be recalled. Who exported, what was filtered and how many rows
          left are recorded in the audit log.
        </p>
        <p>
          It covers{" "}
          {narrowed
            ? "everything matching the filters on this screen"
            : "every contact in this workspace, because no filters are set"}
          .{" "}
          {exportCount === null
            ? "The number of rows is not known here; it is recorded in the audit log."
            : `${count(exportCount, "contact")} matched when you selected them, and the export runs the filters again, so the file holds whatever matches when it runs.`}
        </p>
        {/* Ruling E1. The grid shows the frozen set whenever a selection exists, so "export the
            contacts you selected" would be false in exactly the situation the person is most
            likely to be in. The export route takes a filter and has no `snapshot_id`. */}
        {selection.mode !== "none" && !exportIsSelection ? (
          <p>
            This is <strong>not</strong> the {count(chosen, "contact")} you selected. An export
            cannot be pointed at a selection: it runs the filters on this screen.
          </p>
        ) : null}
        {exportBusy ? (
          <p role="status">
            Preparing the file. The whole export has to arrive before the download starts, so a
            large one takes a while — nothing is stuck.
          </p>
        ) : null}
      </ConfirmDialog>

      <Dialog open={creating} onClose={() => setCreating(false)} title="New contact">
        <ContactForm
          ws={id}
          onCancel={() => setCreating(false)}
          onDone={async () => {
            setCreating(false);
            // The new contact is newest-first in the default view, so the grid must refetch.
            await queryClient.invalidateQueries({ queryKey: ["contacts", id] });
          }}
        />
      </Dialog>
    </section>
  );
}

/**
 * Saves a blob as a file. `name` repeats the server's own `content-disposition` filename: a body
 * the page read itself never reaches the browser's download machinery, so that header is not
 * applied and the name has to be given here.
 */
function download(blob: Blob, name: string): void {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  // Firefox only downloads from a link that is in the document.
  document.body.append(link);
  link.click();
  link.remove();
  // One task later: the browser has taken the blob by then, and never revoking it would hold the
  // whole CSV in memory until the tab closes.
  window.setTimeout(() => URL.revokeObjectURL(url), 0);
}

/**
 * Uncontrolled, with a trailing debounce: its text never lives in state the table reads, so
 * typing does not re-render the rows. Fewer than 3 characters never reach the server (a 422).
 */
function SearchBox({
  initial,
  onSearch,
  announce,
}: {
  initial: string;
  onSearch: (q: string | undefined) => void;
  announce: (text: string) => void;
}) {
  const [short, setShort] = useState(false);
  const timer = useRef<number>(0);
  return (
    <div>
      <Input
        type="search"
        aria-label="Search contacts"
        aria-describedby={short ? "search-hint" : undefined}
        placeholder="Search name or email"
        defaultValue={initial}
        className="w-64"
        onChange={(event) => {
          const value = event.target.value.trim();
          setShort(value.length > 0 && value.length < MIN_QUERY);
          window.clearTimeout(timer.current);
          timer.current = window.setTimeout(() => {
            if (value.length === 0) onSearch(undefined);
            else if (value.length >= MIN_QUERY) onSearch(value);
            else announce(`Type at least ${MIN_QUERY} characters to search`);
          }, 250);
        }}
      />
      {short ? (
        <span id="search-hint" className="mt-1 block text-fg-muted">
          Type at least {MIN_QUERY} characters to search
        </span>
      ) : null}
    </div>
  );
}

/** One picker driving every action that takes what it names. */
function BulkAction({
  label,
  placeholder,
  options,
  disabled,
  actions,
}: {
  label: string;
  placeholder: string;
  options: { id: string; name: string }[];
  disabled: boolean;
  actions: { label: string; run: (id: string, name: string) => void }[];
}) {
  const [picked, setPicked] = useState("");
  return (
    <span className="flex max-w-full flex-wrap items-center gap-2">
      <Select aria-label={label} value={picked} onChange={(e) => setPicked(e.target.value)}>
        <option value="">{placeholder}</option>
        {options.map((o) => (
          <option key={o.id} value={o.id}>
            {o.name}
          </option>
        ))}
      </Select>
      {actions.map((a) => (
        <Button
          key={a.label}
          variant="outline"
          disabled={!picked || disabled}
          onClick={() => a.run(picked, options.find((o) => o.id === picked)?.name ?? "")}
        >
          {a.label}
        </Button>
      ))}
    </span>
  );
}

/**
 * The outcome in that action's own terms. `affected` is how many it changed and `selected` how
 * many it was aimed at; they differ whenever some were already done. A `pending` subscribe wrote
 * rows that are not mailable until confirmed and nothing in this milestone confirms them, so it
 * is never reported as a successful subscribe.
 */
function describeOutcome(
  kind: Kind,
  name: string,
  { affected, selected, status }: components["schemas"]["BulkResult"],
  promised?: number,
): Result {
  const rest = selected - affected;
  const of = `${affected.toLocaleString()} of ${count(selected, "selected contact")}`;
  const was = rest === 1 ? "was" : "were";
  if (kind === "tag") {
    return affected === selected
      ? { tone: "ok", text: `Tagged ${count(affected, "contact")} with “${name}”.` }
      : {
          tone: "warn",
          text: `Tagged ${of} with “${name}”. The other ${rest.toLocaleString()} already had it.`,
        };
  }
  if (kind === "untag") {
    return affected === selected
      ? { tone: "ok", text: `Removed “${name}” from ${count(affected, "contact")}.` }
      : {
          tone: "warn",
          text: `Removed “${name}” from ${of}. The other ${rest.toLocaleString()} did not have it.`,
        };
  }
  if (kind === "unsubscribe") {
    return affected === selected
      ? { tone: "ok", text: `Unsubscribed ${count(affected, "contact")} from “${name}”.` }
      : {
          tone: "warn",
          text: `Unsubscribed ${of} from “${name}”. The other ${rest.toLocaleString()} ${was} not subscribed to it.`,
        };
  }
  if (kind === "delete") {
    // Against the dialog's number, not the server's: `affected` and `selected` are taken from
    // the one statement and always agree, so a row someone else removed between the dialog
    // opening and the delete running shows up here or nowhere.
    const want = promised ?? selected;
    const short = want - affected;
    const them = short === 1 ? "it" : "them";
    // More than was confirmed. The server now refuses that before it deletes anything — it
    // compares this very number against what the action would reach — so this is a backstop for
    // an id-list selection, which carries no confirmed count, and for a server that stopped
    // checking. It is the direction that cannot be undone, so it says what happened rather than
    // reading as success.
    if (short < 0) {
      return {
        tone: "warn",
        text: `Deleted ${affected.toLocaleString()} contacts — more than the ${count(want, "contact")} you confirmed. No address was suppressed.`,
      };
    }
    return short > 0
      ? {
          tone: "warn",
          text: `Deleted ${affected.toLocaleString()} of the ${count(want, "contact")} you confirmed. The other ${short.toLocaleString()} ${short === 1 ? "was" : "were"} already gone — someone else deleted or changed ${them} first. No address was suppressed.`,
        }
      : { tone: "ok", text: `Deleted ${count(affected, "contact")}. No address was suppressed.` };
  }
  const head =
    affected === 0
      ? `${of} were added to “${name}”.`
      : status === "pending"
        ? `${of} were added to “${name}” as pending, not subscribed. The list uses double opt-in and nothing sends the confirmation yet, so they cannot be mailed.`
        : affected === selected
          ? `Subscribed ${count(affected, "contact")} to “${name}”.`
          : `${of} were subscribed to “${name}”.`;
  const tail =
    rest > 0
      ? ` The other ${rest.toLocaleString()} were already on the list, whatever their status; anyone who unsubscribed is never subscribed again from here.`
      : "";
  return { tone: status === "pending" || rest > 0 ? "warn" : "ok", text: head + tail };
}
