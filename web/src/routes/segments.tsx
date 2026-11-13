// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";

import { api, problemMessage, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Button } from "../components/button";
import { ConfirmDialog } from "../components/confirm-dialog";
import { Dialog } from "../components/dialog";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Alert, Field } from "../components/field";
import { CONTROL } from "../components/input";
import {
  type Catalog,
  type CountState,
  type Doc,
  Count,
  RuleBuilder,
  asDoc,
  asRules,
  complete,
  emptyDoc,
  fieldOptions,
  usePreview,
} from "../components/rule-builder";
import { Skeleton } from "../components/skeleton";
import { TableScroller } from "../components/table-scroller";
import { atLeast } from "../lib/roles";
import { handOver } from "../lib/selection";
import { useWorkspace } from "./workspace-layout";

type Segment = components["schemas"]["Segment"];

const day = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });
const th = "py-2 pe-3 text-start font-medium";

/**
 * One page of segments, and the server's `MAX_COUNT_IDS`: the page is counted in one request,
 * and that request is bounded by what a page shows.
 *
 * shortcut: no "Load more". Add one — asking for the new page's counts — the day a workspace
 * keeps more segments than this.
 */
const PAGE = 25;

/**
 * How many custom fields, lists and tags the pickers offer.
 *
 * shortcut: the first 200 of each, with no cursor. The builder says so when a list is at the cap,
 * because a field that is simply missing from a picker is indistinguishable from one that was
 * never defined. Page them the day a workspace has more.
 */
const CATALOG_LIMIT = 200;

const COUNT_PROBLEMS = {
  "rate-limited": "Counting is busy — the counts will catch up in a moment.",
};

/**
 * What the one page-level alert says when the batch arrived but some rows in it have no number.
 *
 * `failed` is its own fact: not "no honest number exists for this document" (`no_estimate`) and
 * not "the document is broken" (`unreadable`), but "we could not work one out this time". It is
 * announced once for the page and the rows stay quiet, exactly as a wholly failed batch is —
 * twenty-five Retry buttons with the same name were removed from this screen once already.
 */
const SOME_COUNTS_FAILED =
  "Some of these counts could not be worked out this time. Nothing is wrong with the segments.";

const SAVE_PROBLEMS = {
  "segment-name-taken": "A segment with that name already exists in this workspace.",
  "not-found": "This segment no longer exists. It may have been deleted in another window.",
  "insufficient-role": "Your role cannot change segments. It takes editor or above.",
};

export default function Segments() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return (
    <AllSegments
      ws={workspace.id}
      slug={workspace.slug}
      canEdit={atLeast(workspace.role, "editor")}
    />
  );
}

function AllSegments({ ws, slug, canEdit }: { ws: string; slug: string; canEdit: boolean }) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  // `null` is closed; `"new"` is the create form; a segment is the edit form for it.
  const [form, setForm] = useState<Segment | "new" | null>(null);
  const [deleting, setDeleting] = useState<Segment | null>(null);

  const segments = useQuery({
    queryKey: ["segments", ws],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/segments", {
        params: { path: { workspace_id: ws }, query: { limit: PAGE } },
      });
      if (!data) throw new Error(problemMessage(error, "The segments could not be loaded."));
      return data;
    },
    retry: false,
  });

  const catalog = useCatalog(ws);
  /**
   * Both, always. `["segment-counts", ws, ids]` is keyed by the ids on the page, so a saved edit
   * that changes what a segment matches does not change the key: without this the row keeps the
   * number from before the edit, unmarked, until a reload — and `staleTime` is a minute.
   */
  const refreshSegments = async () => {
    await Promise.all([
      qc.invalidateQueries({ queryKey: ["segments", ws] }),
      qc.invalidateQueries({ queryKey: ["segment-counts", ws] }),
    ]);
  };
  const rows = segments.data?.data ?? [];
  const ids = rows.map((s) => s.id).join(",");

  /**
   * Every row's count in **one** request, from a route that counts stored segments by id.
   *
   * It is its own query, so the rows are on screen before any count lands — a count may hold a
   * connection for 1.5 seconds. It is one request rather than one per row because preview is 30
   * a minute for a workspace and a full page is 25 rows; and it resolves a stored document, so a
   * segment naming a custom field archived since it was saved still counts, where `preview`
   * would refuse it as a new rule.
   */
  const counts = useQuery({
    queryKey: ["segment-counts", ws, ids],
    queryFn: async () => {
      const { data, error } = await api.GET(
        "/api/v1/workspaces/{workspace_id}/segments/counts",
        { params: { path: { workspace_id: ws }, query: { ids } } },
      );
      if (!data) throw new Error(problemText(error, COUNT_PROBLEMS, "The counts could not be loaded."));
      return new Map(data.map((c) => [c.id, c]));
    },
    enabled: ids !== "",
    retry: false,
    refetchOnWindowFocus: false,
    staleTime: 60_000,
  });
  /** True when the batch came back but one or more of its rows could not be counted. */
  const someFailed = [...(counts.data?.values() ?? [])].some((c) => c.state === "failed");
  // A failed batch is one event, so it is announced once for the page (below) and the rows go
  // quiet: twenty-five `role="alert"` spans and twenty-five identically named Retry buttons are
  // the same failure read twenty-five times.
  const countOf = (id: string): CountState => {
    if (counts.isError) return { kind: "unavailable" };
    const found = counts.data?.get(id);
    return found ? { kind: "counted", state: found.state, count: found.count } : { kind: "counting" };
  };
  /**
   * Whether this row may offer to select its contacts.
   *
   * **Only from an exact count** (spec §14). Freezing a selection from an estimate — or from a
   * row that has no number at all — costs five seconds and may end in `422 selection-too-large`,
   * and there is nothing the person could do from this screen to make it work. So the control is
   * absent rather than disabled with a tooltip: a control nobody may use is not a control.
   */
  const countedExactly = (id: string): boolean => {
    const state = countOf(id);
    return state.kind === "counted" && state.state === "exact";
  };

  /**
   * Freezes the segment's contacts server-side and shows them on the contacts grid.
   *
   * The token goes to the grid through module state, never the URL: it is signed, and a URL
   * would put it in the back/forward stack, the referer header and every proxy log.
   */
  const select = useMutation({
    mutationFn: async (segment: Segment) => {
      const { data, error } = await api.POST(
        "/api/v1/workspaces/{workspace_id}/contacts/selection",
        { params: { path: { workspace_id: ws } }, body: { segment_id: segment.id } },
      );
      // The server's own sentence, which for `selection-too-large` names the limit it is over.
      // A generic "that did not work" would leave the person with nothing to act on.
      if (!data) throw new Error(problemMessage(error, "The contacts could not be selected."));
      return data;
    },
    onSuccess: (data) => {
      handOver({
        ws,
        token: data.selection_token,
        snapshot: data.snapshot_id,
        matched: data.selected,
      });
      void navigate({ to: "/w/$slug/contacts", params: { slug } });
    },
  });

  // Its own key rather than the members screen's: one cache entry served by two queryFns is one
  // whose failure behaviour depends on which screen mounted last.
  const people = useQuery({
    queryKey: ["segment-authors", ws],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{id}/members", {
        params: { path: { id: ws } },
      });
      return data?.data ?? [];
    },
    retry: false,
  });
  const author = (id: string | null | undefined) =>
    people.data?.find((m) => m.user_id === id)?.email ?? "—";

  const remove = useMutation({
    mutationFn: async (segment: Segment) => {
      const { response, error } = await api.DELETE(
        "/api/v1/workspaces/{workspace_id}/segments/{id}",
        { params: { path: { workspace_id: ws, id: segment.id } } },
      );
      if (!response.ok) {
        throw new Error(problemText(error, SAVE_PROBLEMS, "The segment could not be deleted."));
      }
    },
    onSuccess: async () => {
      setDeleting(null);
      await refreshSegments();
    },
  });

  return (
    <section className="mt-8">
      <div className="flex flex-wrap items-baseline justify-between gap-3">
        <h2 className="text-xl font-semibold">Segments</h2>
        {/* Absent rather than disabled below editor: a control nobody may use is not a control.
            With no segments yet the empty state carries the one offer, so it is not doubled. */}
        {canEdit && rows.length > 0 ? (
          <Button variant="solid" onClick={() => setForm("new")}>
            New segment
          </Button>
        ) : null}
      </div>

      <div className="mt-4">
        {segments.isPending ? (
          <Skeleton label="Loading segments" rows={5} />
        ) : segments.isError ? (
          <ErrorState
            title="The segments could not be loaded."
            onRetry={() => void segments.refetch()}
          >
            {segments.error.message}
          </ErrorState>
        ) : rows.length === 0 ? (
          <EmptyState
            title="No segments yet"
            actions={
              canEdit ? (
                <Button variant="solid" onClick={() => setForm("new")}>
                  New segment
                </Button>
              ) : null
            }
          >
            A segment is a saved set of rules that picks out contacts — everyone at a domain, on a
            list, or with a tag — and stays right as those contacts change.
          </EmptyState>
        ) : (
          <>
            {select.error ? <Alert>{select.error.message}</Alert> : null}
            {/* One alert for one failure, whether the whole batch failed or some rows in it did.
                The rows show "—" rather than repeating it. */}
            {counts.isError || someFailed ? (
              <p role="alert" className="mb-3 text-danger-text">
                {counts.isError ? counts.error.message : SOME_COUNTS_FAILED}{" "}
                <Button variant="ghost" onClick={() => void counts.refetch()}>
                  Count the segments again
                </Button>
              </p>
            ) : null}
            <TableScroller label="Segments table">
              <table className="w-full">
                <caption className="sr-only">Segments</caption>
                <thead>
                  <tr className="border-b border-border">
                    <th scope="col" className={th}>
                      Name
                    </th>
                    <th scope="col" className={th}>
                      Description
                    </th>
                    <th scope="col" className={th}>
                      Contacts
                    </th>
                    <th scope="col" className={th}>
                      Created by
                    </th>
                    <th scope="col" className={th}>
                      Updated
                    </th>
                    {canEdit ? (
                      <th scope="col" className={th}>
                        <span className="sr-only">Actions</span>
                      </th>
                    ) : null}
                  </tr>
                </thead>
                <tbody>
                  {rows.map((s) => (
                    <tr key={s.id} className="border-b border-border">
                      <td className="py-2 pe-3 font-medium wrap-anywhere">{s.name}</td>
                      <td className="py-2 pe-3 wrap-anywhere">{s.description || "—"}</td>
                      <td className="py-2 pe-3">
                        <Count state={countOf(s.id)} testId="segment-count" />
                      </td>
                      <td className="py-2 pe-3 wrap-anywhere">{author(s.created_by)}</td>
                      <td className="py-2 pe-3 tabular-nums text-fg-muted">
                        {day.format(new Date(s.updated_at))}
                      </td>
                      {canEdit ? (
                        <td className="py-2">
                          <span className="flex flex-wrap gap-2">
                            {countedExactly(s.id) ? (
                              <Button
                                aria-label={`Select contacts in ${s.name}`}
                                busy={select.isPending && select.variables?.id === s.id}
                                onClick={() => select.mutate(s)}
                              >
                                Select contacts
                              </Button>
                            ) : null}
                            <Button aria-label={`Edit ${s.name}`} onClick={() => setForm(s)}>
                              Edit
                            </Button>
                            <Button
                              variant="danger-ghost"
                              aria-label={`Delete ${s.name}`}
                              onClick={() => setDeleting(s)}
                            >
                              Delete
                            </Button>
                          </span>
                        </td>
                      ) : null}
                    </tr>
                  ))}
                </tbody>
              </table>
            </TableScroller>
            {segments.data?.next_cursor ? (
              <p className="mt-3 text-fg-muted">Showing the first {PAGE} segments.</p>
            ) : null}
          </>
        )}
      </div>

      <Dialog
        open={form !== null}
        onClose={() => setForm(null)}
        variant="drawer"
        title={form === "new" ? "New segment" : "Edit segment"}
      >
        {form === null ? null : (
          <SegmentForm
            ws={ws}
            existing={form === "new" ? undefined : form}
            catalog={catalog}
            onCancel={() => setForm(null)}
            onDone={async () => {
              setForm(null);
              await refreshSegments();
            }}
          />
        )}
      </Dialog>

      <ConfirmDialog
        open={deleting !== null}
        onClose={() => {
          setDeleting(null);
          remove.reset();
        }}
        title="Delete this segment"
        confirmLabel="Delete segment"
        busy={remove.isPending}
        error={remove.error?.message}
        onConfirm={() => deleting && remove.mutate(deleting)}
      >
        <p>
          <strong className="wrap-anywhere">{deleting?.name}</strong> is deleted. A segment is only
          a saved question, so no contact is touched and nothing they are on changes.
        </p>
        <p>Anything that pointed at this segment will have to name another one.</p>
      </ConfirmDialog>
    </section>
  );
}

/**
 * The live field, list and tag options the pickers are built from.
 *
 * **Loaded, failed and loaded-but-empty are three different states and the caller is told which.**
 * Collapsing a failure into an empty catalog is how the builder came to say a live field had been
 * archived; a fetch that did not happen is not evidence about anything.
 */
function useCatalog(ws: string): {
  catalog: Catalog;
  isPending: boolean;
  isError: boolean;
  truncated: boolean;
  retry: () => void;
} {
  const definitions = useQuery({
    queryKey: ["custom-fields", ws, "segments"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/custom-fields", {
        params: { path: { workspace_id: ws }, query: { limit: CATALOG_LIMIT } },
      });
      if (!data) throw new Error(problemMessage(error, "The custom fields could not be loaded."));
      return data.data;
    },
    retry: false,
  });
  const lists = useQuery({
    queryKey: ["lists", ws, "segments"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/lists", {
        params: { path: { workspace_id: ws }, query: { limit: CATALOG_LIMIT } },
      });
      if (!data) throw new Error(problemMessage(error, "The lists could not be loaded."));
      return data.data;
    },
    retry: false,
  });
  const tags = useQuery({
    queryKey: ["tags", ws, "segments"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/tags", {
        params: { path: { workspace_id: ws }, query: { limit: CATALOG_LIMIT } },
      });
      if (!data) throw new Error(problemMessage(error, "The tags could not be loaded."));
      return data.data;
    },
    retry: false,
  });
  const all = [definitions, lists, tags];
  return {
    catalog: {
      fields: fieldOptions(definitions.data ?? []),
      lists: lists.data ?? [],
      tags: tags.data ?? [],
    },
    isPending: all.some((q) => q.isPending),
    isError: all.some((q) => q.isError),
    truncated: all.some((q) => (q.data?.length ?? 0) >= CATALOG_LIMIT),
    retry: () => {
      for (const q of all) void q.refetch();
    },
  };
}

function SegmentForm({
  ws,
  existing,
  catalog,
  onCancel,
  onDone,
}: {
  ws: string;
  existing?: Segment;
  catalog: ReturnType<typeof useCatalog>;
  onCancel: () => void;
  onDone: () => void;
}) {
  const [name, setName] = useState(existing?.name ?? "");
  const [description, setDescription] = useState(existing?.description ?? "");
  const [doc, setDoc] = useState<Doc>(existing ? asDoc(existing.rules) : emptyDoc());
  const [unfinished, setUnfinished] = useState("");

  // The name, the description and the way out are here from the first frame; only the rules wait
  // for the pickers. The drawer used to render nothing but a skeleton and a heading until the
  // catalog landed: no Cancel for a mouse, and first focus stuck on the heading afterwards.
  const ready = !catalog.isPending && !catalog.isError;
  const fields = catalog.catalog.fields;
  const count = usePreview(ws, doc, fields, ready);

  const save = useMutation({
    mutationFn: async () => {
      // `rules` is replaced as a whole, never merge-patched: a partial document would flip the
      // connective and silently keep the old rules (section 12).
      const { data, error } = existing
        ? await api.PATCH("/api/v1/workspaces/{workspace_id}/segments/{id}", {
            params: { path: { workspace_id: ws, id: existing.id } },
            body: { name: name.trim(), description: description.trim(), rules: asRules(doc) },
          })
        : await api.POST("/api/v1/workspaces/{workspace_id}/segments", {
            params: { path: { workspace_id: ws } },
            body: { name: name.trim(), description: description.trim(), rules: asRules(doc) },
          });
      if (!data) throw new Error(problemText(error, SAVE_PROBLEMS, "The segment could not be saved."));
    },
    onSuccess: onDone,
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    if (doc.rules.length === 0) {
      setUnfinished("A segment needs at least one rule: an empty one would match every contact.");
      return;
    }
    if (!ready) {
      setUnfinished("The fields, lists and tags are still loading.");
      return;
    }
    if (!complete(doc, fields)) {
      setUnfinished("Finish every rule first — each one still needs a value.");
      return;
    }
    setUnfinished("");
    save.mutate();
  }

  return (
    <form onSubmit={submit} noValidate>
      <Field
        id="segment-name"
        label="Name"
        autoComplete="off"
        required
        maxLength={100}
        value={name}
        onChange={(e) => setName(e.target.value)}
      />
      <div className="mt-4">
        <label className="block font-medium" htmlFor="segment-description">
          Description
        </label>
        <textarea
          id="segment-description"
          rows={2}
          maxLength={1000}
          className={`${CONTROL} mt-1 w-full py-2`}
          value={description}
          onChange={(e) => setDescription(e.target.value)}
        />
      </div>

      <div className="mt-6">
        <h3 className="font-medium">Rules</h3>
        {/* The builder is not drawn until the fields, lists and tags are really loaded: it reads
            "not in the catalog" as "archived", which is true only of a catalog that arrived.
            Against a failed request it would tell someone a live field had been archived and
            offer removal as the only way on. */}
        {catalog.isError ? (
          <div className="mt-3">
            <ErrorState
              title="The fields, lists and tags could not be loaded."
              onRetry={catalog.retry}
            >
              Until they are, the rules cannot be shown correctly, so they are not shown at all.
            </ErrorState>
          </div>
        ) : catalog.isPending ? (
          <div className="mt-3">
            <Skeleton label="Loading the fields, lists and tags" rows={3} />
          </div>
        ) : (
          <>
            <RuleBuilder doc={doc} onChange={setDoc} catalog={catalog.catalog} />
            {catalog.truncated ? (
              <p className="mt-3 text-fg-muted">
                The pickers offer the first {CATALOG_LIMIT} custom fields, lists and tags. A
                workspace with more than that has some that cannot be chosen here yet.
              </p>
            ) : null}
          </>
        )}
      </div>

      {ready ? (
        <div className="mt-4 rounded-md border border-border p-3">
          <span className="text-fg-muted">Matching contacts: </span>
          <Count state={count} testId="preview-count" />
        </div>
      ) : null}

      {unfinished ? <Alert>{unfinished}</Alert> : null}
      {save.error ? <Alert>{save.error.message}</Alert> : null}
      <div className="mt-6 flex flex-wrap justify-end gap-3">
        <Button onClick={onCancel}>Cancel</Button>
        <Button type="submit" variant="solid" busy={save.isPending}>
          Save segment
        </Button>
      </div>
    </form>
  );
}
