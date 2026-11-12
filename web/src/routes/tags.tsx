// SPDX-License-Identifier: AGPL-3.0-only

import { useInfiniteQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";

import { api, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Button } from "../components/button";
import { ConfirmDialog } from "../components/confirm-dialog";
import { Dialog } from "../components/dialog";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Field } from "../components/field";
import { Skeleton } from "../components/skeleton";
import { TableScroller } from "../components/table-scroller";
import { atLeast } from "../lib/roles";
import { useWorkspace } from "./workspace-layout";

type Tag = components["schemas"]["Tag"];

const day = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });
const PAGE = 200;
const th = "py-2 pe-3 text-start font-medium";
const KNOWN = {
  "tag-name-taken": "A tag with that name already exists.",
  "insufficient-role": "Your role cannot change tags. It takes editor or above.",
};

export default function Tags() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return <AllTags ws={workspace.id} canEdit={atLeast(workspace.role, "editor")} />;
}

function AllTags({ ws, canEdit }: { ws: string; canEdit: boolean }) {
  const qc = useQueryClient();
  const tags = useInfiniteQuery({
    queryKey: ["tags", ws],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/tags", {
        params: { path: { workspace_id: ws }, query: { limit: PAGE, cursor: pageParam } },
      });
      if (!data) throw new Error(problemText(error, KNOWN, "The tags could not be loaded."));
      return data;
    },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    retry: false,
  });
  const rows = tags.data?.pages.flatMap((p) => p.data) ?? [];
  const refresh = () => qc.invalidateQueries({ queryKey: ["tags", ws] });

  const [name, setName] = useState("");
  const create = useMutation({
    mutationFn: async () => {
      const { data, error } = await api.POST("/api/v1/workspaces/{workspace_id}/tags", {
        params: { path: { workspace_id: ws } },
        body: { name: name.trim() },
      });
      if (!data) throw new Error(problemText(error, KNOWN, "The tag could not be added."));
    },
    onSuccess: async () => {
      setName("");
      await refresh();
    },
  });
  function add(event: FormEvent) {
    event.preventDefault();
    create.mutate();
  }

  const [renaming, setRenaming] = useState<Tag | null>(null);
  const [newName, setNewName] = useState("");
  const rename = useMutation({
    mutationFn: async (tag: Tag) => {
      const { data, error } = await api.PATCH("/api/v1/workspaces/{workspace_id}/tags/{id}", {
        params: { path: { workspace_id: ws, id: tag.id } },
        body: { name: newName.trim() },
      });
      if (!data) throw new Error(problemText(error, KNOWN, "The tag could not be renamed."));
    },
    onSuccess: async () => {
      setRenaming(null);
      await refresh();
    },
  });

  const [deleting, setDeleting] = useState<Tag | null>(null);
  const remove = useMutation({
    mutationFn: async (tag: Tag) => {
      const { response, error } = await api.DELETE("/api/v1/workspaces/{workspace_id}/tags/{id}", {
        params: { path: { workspace_id: ws, id: tag.id } },
      });
      if (!response.ok) throw new Error(problemText(error, KNOWN, "The tag could not be deleted."));
    },
    onSuccess: async () => {
      setDeleting(null);
      await refresh();
    },
  });

  return (
    <section className="mt-8">
      <h2 className="text-xl font-semibold">Tags</h2>
      {canEdit ? (
        <form onSubmit={add} className="mt-2 max-w-md">
          <Field
            id="new-tag"
            label="New tag name"
            value={name}
            maxLength={100}
            onChange={(e) => setName(e.target.value)}
            error={create.error?.message}
          />
          <Button type="submit" variant="solid" className="mt-3" busy={create.isPending}>
            Add tag
          </Button>
        </form>
      ) : null}
      <div className="mt-6">
        {tags.isPending ? (
          <Skeleton label="Loading tags" rows={5} />
        ) : tags.isError && rows.length === 0 ? (
          <ErrorState title="The tags could not be loaded." onRetry={() => void tags.refetch()}>
            {tags.error.message}
          </ErrorState>
        ) : rows.length === 0 ? (
          <EmptyState title="No tags yet">Tags you create will appear here.</EmptyState>
        ) : (
          <TableScroller label="Tags table"><table className="w-full">
            <caption className="sr-only">Tags</caption>
            <thead>
              <tr className="border-b border-border">
                <th scope="col" className={th}>
                  Name
                </th>
                <th scope="col" className={th}>
                  Created
                </th>
                {canEdit ? (
                  <th scope="col" className={th}>
                    <span className="sr-only">Actions</span>
                  </th>
                ) : null}
              </tr>
            </thead>
            <tbody>
              {rows.map((t) => (
                <tr key={t.id} className="border-b border-border">
                  <td className="py-2 pe-3 wrap-anywhere">{t.name}</td>
                  <td className="py-2 pe-3 tabular-nums text-fg-muted">
                    {day.format(new Date(t.created_at))}
                  </td>
                  {canEdit ? (
                    <td className="py-2 text-end">
                      <Button
                        variant="ghost"
                        aria-label={`Rename ${t.name}`}
                        onClick={() => {
                          setNewName(t.name);
                          setRenaming(t);
                        }}
                      >
                        Rename
                      </Button>
                      <Button
                        variant="danger-ghost"
                        aria-label={`Delete ${t.name}`}
                        onClick={() => setDeleting(t)}
                      >
                        Delete
                      </Button>
                    </td>
                  ) : null}
                </tr>
              ))}
            </tbody>
          </table></TableScroller>
        )}
        {tags.isFetchNextPageError ? (
          <p role="alert" className="mt-3">
            More tags could not be loaded.
          </p>
        ) : null}
        {tags.hasNextPage ? (
          <Button className="mt-3" busy={tags.isFetchingNextPage} onClick={() => void tags.fetchNextPage()}>
            Load more tags
          </Button>
        ) : null}
      </div>

      <Dialog
        open={renaming !== null}
        onClose={() => {
          setRenaming(null);
          rename.reset();
        }}
        title={`Rename the tag ${renaming?.name ?? ""}`}
      >
        <form
          onSubmit={(e) => {
            e.preventDefault();
            if (renaming) rename.mutate(renaming);
          }}
        >
          <Field
            id="rename-tag"
            label="Name"
            value={newName}
            maxLength={100}
            onChange={(e) => setNewName(e.target.value)}
            error={rename.error?.message}
          />
          <div className="mt-6 flex justify-end gap-3">
            <Button onClick={() => setRenaming(null)}>Cancel</Button>
            <Button type="submit" variant="solid" busy={rename.isPending}>
              Save
            </Button>
          </div>
        </form>
      </Dialog>

      <ConfirmDialog
        open={deleting !== null}
        onClose={() => {
          setDeleting(null);
          remove.reset();
        }}
        title={`Delete the tag ${deleting?.name ?? ""}`}
        confirmLabel="Delete tag"
        busy={remove.isPending}
        error={remove.error?.message}
        onConfirm={() => deleting && remove.mutate(deleting)}
      >
        <p>
          Deleting this tag removes it from every contact that has it. The contacts themselves are
          not deleted.
        </p>
        <p>This cannot be undone.</p>
      </ConfirmDialog>
    </section>
  );
}
