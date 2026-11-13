// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
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
  type Doc,
  CountText,
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
import { useWorkspace } from "./workspace-layout";

type Segment = components["schemas"]["Segment"];

const day = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });
const th = "py-2 pe-3 text-start font-medium";

/**
 * One page of segments. Each row counts itself, and preview is 30 per minute per workspace
 * (`PREVIEW_PER_MINUTE`), so the page size is what a workspace can afford to count in one visit.
 *
 * shortcut: no "Load more". Add one — and count only the rows on screen — the day a workspace
 * keeps more segments than this.
 */
const PAGE = 25;

const SAVE_PROBLEMS = {
  "segment-name-taken": "A segment with that name already exists in this workspace.",
  "not-found": "This segment no longer exists. It may have been deleted in another window.",
  "insufficient-role": "Your role cannot change segments. It takes editor or above.",
};

export default function Segments() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return <AllSegments ws={workspace.id} canEdit={atLeast(workspace.role, "editor")} />;
}

function AllSegments({ ws, canEdit }: { ws: string; canEdit: boolean }) {
  const qc = useQueryClient();
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
      await qc.invalidateQueries({ queryKey: ["segments", ws] });
    },
  });

  const rows = segments.data?.data ?? [];

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
                        <RowCount ws={ws} segment={s} />
                      </td>
                      <td className="py-2 pe-3 wrap-anywhere">{author(s.created_by)}</td>
                      <td className="py-2 pe-3 tabular-nums text-fg-muted">
                        {day.format(new Date(s.updated_at))}
                      </td>
                      {canEdit ? (
                        <td className="py-2">
                          <span className="flex flex-wrap gap-2">
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
              await qc.invalidateQueries({ queryKey: ["segments", ws] });
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

/** The live field, list and tag options the pickers are built from. */
function useCatalog(ws: string): Catalog {
  const definitions = useQuery({
    queryKey: ["custom-fields", ws, "segments"],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/custom-fields", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      return data?.data ?? [];
    },
    retry: false,
  });
  const lists = useQuery({
    queryKey: ["lists", ws],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/lists", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      return data?.data ?? [];
    },
    retry: false,
  });
  const tags = useQuery({
    queryKey: ["tags", ws],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/tags", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      return data?.data ?? [];
    },
    retry: false,
  });
  return {
    fields: fieldOptions(definitions.data ?? []),
    lists: lists.data ?? [],
    tags: tags.data ?? [],
  };
}

/**
 * One row's count: its own query, so the list is readable before any of them land. A count can
 * hold a connection for 1.5 seconds and there is one per row, so nothing waits for them.
 */
function RowCount({ ws, segment }: { ws: string; segment: Segment }) {
  const preview = usePreview(ws, asDoc(segment.rules));
  if (preview.isError) {
    return (
      <span role="alert">
        {preview.error.message}{" "}
        <Button variant="ghost" aria-label={`Count ${segment.name} again`} onClick={() => void preview.refetch()}>
          Retry
        </Button>
      </span>
    );
  }
  if (!preview.data) {
    return (
      <div className="w-24">
        <Skeleton label="Counting contacts" />
      </div>
    );
  }
  return <CountText preview={preview.data} testId="segment-count" />;
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
  catalog: Catalog;
  onCancel: () => void;
  onDone: () => void;
}) {
  const [name, setName] = useState(existing?.name ?? "");
  const [description, setDescription] = useState(existing?.description ?? "");
  const [doc, setDoc] = useState<Doc>(existing ? asDoc(existing.rules) : emptyDoc());
  const [unfinished, setUnfinished] = useState("");

  const preview = usePreview(ws, doc);

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
    if (!complete(doc)) {
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
        <RuleBuilder doc={doc} onChange={setDoc} catalog={catalog} />
      </div>

      <div className="mt-4 rounded-md border border-border p-3">
        <span className="text-fg-muted">Matching contacts: </span>
        {!complete(doc) ? (
          <span>no count yet — every rule needs a value.</span>
        ) : preview.isError ? (
          <Alert>{preview.error.message}</Alert>
        ) : preview.data ? (
          <CountText preview={preview.data} testId="preview-count" />
        ) : (
          <div className="w-24">
            <Skeleton label="Counting contacts" />
          </div>
        )}
      </div>

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
