// SPDX-License-Identifier: AGPL-3.0-only

import {
  keepPreviousData,
  useInfiniteQuery,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";

import { api, problemMessage } from "../api/client";
import type { components } from "../api/schema";
import { Button } from "../components/button";
import { type Column, DataGrid, type Sort } from "../components/data-grid";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Input } from "../components/input";
import { Select } from "../components/select";
import { Skeleton } from "../components/skeleton";
import { atLeast } from "../lib/roles";
import { MAX_EXCLUDED, MAX_IDS, createSelection } from "../lib/selection";
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

function ContactsPage({ id, slug, canEdit }: { id: string; slug: string; canEdit: boolean }) {
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
  const filterKey = JSON.stringify(filters);
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

  // Page start -> the cursor that fetched it. The API only pages forward, so this is how a page
  // dropped off the front by `maxPages` is fetched again when the user scrolls back.
  const cursors = useMemo(() => new Map<number, string | undefined>(), [filterKey]);
  const query = useInfiniteQuery({
    queryKey: ["contacts", id, filterKey],
    initialPageParam: { start: 0 } as { cursor?: string; start: number },
    queryFn: async ({ pageParam, signal }) => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/contacts", {
        params: {
          path: { workspace_id: id },
          query: { ...filters, limit: PAGE, cursor: pageParam.cursor },
        },
        signal,
      });
      if (error || !data) throw new Error(problemMessage(error, "The contacts could not be loaded."));
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
  // How many contacts the current filter matched, once someone has asked to select them all.
  // Until then (or after the filters change) it is unknown, and an unknown total is `-1`.
  const [matched, setMatched] = useState<{ key: string; n: number } | null>(null);
  const total = !query.hasNextPage
    ? rowOffset + rows.length
    : matched?.key === filterKey
      ? matched.n
      : null;

  // Remounts the grid (scroll, focus, virtualiser) once the new query's data is what is shown.
  const shownKey = useRef(filterKey);
  if (!query.isPlaceholderData) shownKey.current = filterKey;

  const store = useMemo(createSelection, []);
  const selection = useSyncExternalStore(store.subscribe, store.get);
  const [result, setResult] = useState<Result | null>(null);
  const [failure, setFailure] = useState("");
  const [busy, setBusy] = useState(false);
  const [counting, setCounting] = useState(false);
  const live = useRef(filterKey);
  live.current = filterKey;

  // Changing the query clears the selection and says so: a selection carried across a filter is
  // how someone acts on the wrong million rows.
  useEffect(() => {
    if (store.get().mode !== "none") {
      store.clear();
      announce("Selection cleared because the filters changed.");
    }
    setResult(null);
    setFailure("");
  }, [filterKey]); // eslint-disable-line react-hooks/exhaustive-deps

  const selectedCount = selection.mode === "ids" ? selection.ids.size : 0;
  const firstRun = useRef(true);
  useEffect(() => {
    if (firstRun.current) {
      firstRun.current = false;
      return;
    }
    if (selection.mode === "none") announce("Selection cleared");
    else if (selection.mode === "ids") announce(`${count(selectedCount, "contact")} selected`);
    else announce("All contacts matching this view selected");
  }, [selection]); // eslint-disable-line react-hooks/exhaustive-deps

  const selecting = useMemo(
    () =>
      canEdit
        ? {
            store,
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
    [canEdit, store],
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
    const mine = filterKey;
    const { data, error } = await api.POST("/api/v1/workspaces/{workspace_id}/contacts/selection", {
      params: { path: { workspace_id: id } },
      body: {
        q: filters.q,
        list_id: filters.list_id,
        list_status: filters.list_status,
        tag_id: filters.tag_id,
      },
    });
    setCounting(false);
    if (mine !== live.current) return; // the filters moved on; this token is for a view that is gone
    if (error || !data) {
      setFailure(problemMessage(error, "Could not select every matching contact."));
      return;
    }
    setMatched({ key: mine, n: data.selected });
    store.selectAll(data.selection_token, data.selected);
  }

  async function bulk(kind: "tag" | "subscribe", target: string, name: string) {
    const current = store.get();
    if (current.mode === "none" || !target) return;
    setBusy(true);
    setFailure("");
    setResult(null);
    try {
      // A filter selection goes as its token and exclusions, ticked rows as their ids: the
        // server takes exactly one of the two, and applies the same rules to both.
      const body =
        current.mode === "all"
          ? { selection_token: current.token, excluded_ids: [...current.excluded] }
          : { ids: [...current.ids] };
      const { data, error } =
        kind === "tag"
          ? await api.POST("/api/v1/workspaces/{workspace_id}/contacts/bulk/tag", {
              params: { path: { workspace_id: id } },
              body: { ...body, tag_id: target },
            })
          : await api.POST("/api/v1/workspaces/{workspace_id}/contacts/bulk/subscribe", {
              params: { path: { workspace_id: id } },
              body: { ...body, list_id: target },
            });
      if (error || !data) {
        setFailure(problemMessage(error, "The action failed and nothing was changed."));
        return;
      }
      const message = describe(kind, name, data);
      setResult(message);
      announce(message.text);
      // The table only changes when it is filtered by what the action changed.
      if (filters.list_id || filters.tag_id) {
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
      <h2 className="text-xl font-semibold">Contacts</h2>

      <div className="mt-4">
        <div hidden={selection.mode !== "none"} className="flex flex-wrap items-start gap-3">
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
            className="flex flex-wrap items-center gap-3 rounded-md border-s-4 border-accent bg-accent-subtle p-3"
          >
            <p className="font-medium">
              {selection.mode === "ids"
                ? `${count(selection.ids.size, "contact")} selected`
                : selection.excluded.size
                  ? `${(selection.matched - selection.excluded.size).toLocaleString()} of ${selection.matched.toLocaleString()} contacts matching this view are selected (${selection.excluded.size.toLocaleString()} left out).`
                  : `All ${selection.matched.toLocaleString()} contacts matching this view are selected.`}
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
              label="Tag to add"
              placeholder="Choose a tag…"
              action="Add tag"
              options={tags.data ?? []}
              busy={busy}
              onRun={(target, name) => void bulk("tag", target, name)}
            />
            <BulkAction
              label="List to subscribe to"
              placeholder="Choose a list…"
              action="Subscribe to list"
              options={lists.data ?? []}
              busy={busy}
              onRun={(target, name) => void bulk("subscribe", target, name)}
            />
            <Button variant="ghost" onClick={() => store.clear()}>
              Clear selection
            </Button>
          </div>
        ) : null}
      </div>

      {result ? (
        <p
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
    </section>
  );
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

function BulkAction({
  label,
  placeholder,
  action,
  options,
  busy,
  onRun,
}: {
  label: string;
  placeholder: string;
  action: string;
  options: { id: string; name: string }[];
  busy: boolean;
  onRun: (id: string, name: string) => void;
}) {
  const [picked, setPicked] = useState("");
  return (
    <span className="flex max-w-full items-center gap-2">
      <Select aria-label={label} value={picked} onChange={(e) => setPicked(e.target.value)}>
        <option value="">{placeholder}</option>
        {options.map((o) => (
          <option key={o.id} value={o.id}>
            {o.name}
          </option>
        ))}
      </Select>
      <Button
        variant="outline"
        disabled={!picked || busy}
        onClick={() => onRun(picked, options.find((o) => o.id === picked)?.name ?? "")}
      >
        {action}
      </Button>
    </span>
  );
}

/**
 * What a bulk action did, in words that cannot be mistaken for more. `affected` is how many it
 * changed and `selected` how many it was aimed at; they differ whenever some were already done.
 * A `pending` subscribe wrote rows that are not mailable until confirmed and nothing in this
 * milestone confirms them, so it is never reported as a successful subscribe.
 */
function describe(
  kind: "tag" | "subscribe",
  name: string,
  { affected, selected, status }: components["schemas"]["BulkResult"],
): Result {
  const rest = selected - affected;
  if (kind === "tag") {
    return affected === selected
      ? { tone: "ok", text: `Tagged ${count(affected, "contact")} with “${name}”.` }
      : {
          tone: "warn",
          text: `Tagged ${affected.toLocaleString()} of ${count(selected, "selected contact")} with “${name}”. The other ${rest.toLocaleString()} already had it.`,
        };
  }
  const of = `${affected.toLocaleString()} of ${count(selected, "selected contact")}`;
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
