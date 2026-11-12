// SPDX-License-Identifier: AGPL-3.0-only

import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";

import { api, problemMessage, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Button } from "../components/button";
import { Checkbox } from "../components/checkbox";
import { ConfirmDialog } from "../components/confirm-dialog";
import { Dialog } from "../components/dialog";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Alert, Field } from "../components/field";
import { CONTROL } from "../components/input";
import { MembershipBadge } from "../components/membership-badge";
import { Skeleton } from "../components/skeleton";
import { TableScroller } from "../components/table-scroller";
import { atLeast } from "../lib/roles";
import { useWorkspace } from "./workspace-layout";

type List = components["schemas"]["List"];

const day = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });
const link = "text-accent-text underline";
const th = "py-2 pe-3 text-start font-medium";
const KNOWN = {
  "list-name-taken": "A list with that name already exists in this workspace.",
  "insufficient-role": "Your role cannot create lists. It takes editor or above.",
};

const optIn = (l: List) => (l.double_optin ? "Double opt-in" : "Single opt-in");

export default function Lists() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return (
    <AllLists
      ws={workspace.id}
      slug={workspace.slug}
      canEdit={atLeast(workspace.role, "editor")}
    />
  );
}

function AllLists({ ws, slug, canEdit }: { ws: string; slug: string; canEdit: boolean }) {
  const [creating, setCreating] = useState(false);
  const lists = useQuery({
    // Under the pickers' key as a prefix, never equal to it: the pickers on the contacts grid
    // and the contact page read ["lists", ws] with a queryFn of their own, and one cache entry
    // serving several queryFns means whichever mounted last decides what a failure does.
    queryKey: ["lists", ws, "all"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/lists", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      if (!data) throw new Error(problemMessage(error, "The lists could not be loaded."));
      // shortcut: the first 200 lists only; add "Load more" when a workspace has more.
      return data.data;
    },
    retry: false,
  });
  return (
    <section className="mt-8">
      <div className="flex flex-wrap items-baseline justify-between gap-3">
        <h2 className="text-xl font-semibold">Lists</h2>
        {canEdit ? (
          <Button variant="solid" onClick={() => setCreating(true)}>
            New list
          </Button>
        ) : null}
      </div>
      <div className="mt-4">
        {lists.isPending ? (
          <Skeleton label="Loading lists" rows={5} />
        ) : lists.isError ? (
          <ErrorState title="The lists could not be loaded." onRetry={() => void lists.refetch()}>
            {lists.error.message}
          </ErrorState>
        ) : lists.data.length === 0 ? (
          <EmptyState title="No lists yet">Lists you create will appear here.</EmptyState>
        ) : (
          <TableScroller label="Lists table"><table className="w-full">
            <caption className="sr-only">Lists</caption>
            <thead>
              <tr className="border-b border-border">
                <th scope="col" className={th}>
                  Name
                </th>
                <th scope="col" className={th}>
                  Opt-in
                </th>
                <th scope="col" className={th}>
                  Visibility
                </th>
                <th scope="col" className={th}>
                  Created
                </th>
              </tr>
            </thead>
            <tbody>
              {lists.data.map((l) => (
                <tr key={l.id} className="border-b border-border">
                  <td className="py-2 pe-3 wrap-anywhere">
                    <Link to="/w/$slug/lists/$listId" params={{ slug, listId: l.id }} className={link}>
                      {l.name}
                    </Link>
                  </td>
                  <td className="py-2 pe-3">{optIn(l)}</td>
                  <td className="py-2 pe-3 capitalize">{l.visibility}</td>
                  <td className="py-2 tabular-nums text-fg-muted">
                    {day.format(new Date(l.created_at))}
                  </td>
                </tr>
              ))}
            </tbody>
          </table></TableScroller>
        )}
      </div>

      <Dialog open={creating} onClose={() => setCreating(false)} title="New list">
        <ListForm ws={ws} onCancel={() => setCreating(false)} onDone={() => setCreating(false)} />
      </Dialog>
    </section>
  );
}

/**
 * Create a list, or edit one. Double opt-in is on by default, as it is on the server: a new
 * member is then `pending` and not mailable until they confirm, and nothing in this milestone
 * sends the confirmation — so the checkbox says it rather than leaving someone to find out.
 *
 * Visibility is not offered: it does nothing until the preference center exists.
 */
function ListForm({
  ws,
  existing,
  onCancel,
  onDone,
}: {
  ws: string;
  existing?: List;
  onCancel: () => void;
  onDone: () => void;
}) {
  const qc = useQueryClient();
  const [name, setName] = useState(existing?.name ?? "");
  const [description, setDescription] = useState(existing?.description ?? "");
  const [doubleOptin, setDoubleOptin] = useState(existing?.double_optin ?? true);

  const create = useMutation({
    mutationFn: async () => {
      // A merge patch: only what changed goes, so the audit log records the fields that were
      // really edited. See `update_list`, which stores exactly the names it was sent.
      const { data, error } = existing
        ? await api.PATCH("/api/v1/workspaces/{workspace_id}/lists/{id}", {
            params: { path: { workspace_id: ws, id: existing.id } },
            body: {
              ...(name.trim() === existing.name ? {} : { name: name.trim() }),
              ...(description.trim() === existing.description
                ? {}
                : { description: description.trim() }),
              ...(doubleOptin === existing.double_optin ? {} : { double_optin: doubleOptin }),
            },
          })
        : await api.POST("/api/v1/workspaces/{workspace_id}/lists", {
            params: { path: { workspace_id: ws } },
            body: {
              name: name.trim(),
              description: description.trim(),
              double_optin: doubleOptin,
            },
          });
      if (!data) throw new Error(problemText(error, KNOWN, "The list could not be saved."));
    },
    onSuccess: async () => {
      onDone();
      await Promise.all([
        qc.invalidateQueries({ queryKey: ["lists", ws] }),
        existing ? qc.invalidateQueries({ queryKey: ["list", ws, existing.id] }) : null,
      ]);
    },
  });
  function submit(event: FormEvent) {
    event.preventDefault();
    create.mutate();
  }

  return (
    <form onSubmit={submit} noValidate>
      <Field
        id="list-name"
        label="Name"
        autoComplete="off"
        required
        maxLength={100}
        value={name}
        onChange={(e) => setName(e.target.value)}
      />
      <div className="mt-4">
        <label className="block font-medium" htmlFor="list-description">
          Description
        </label>
        <textarea
          id="list-description"
          rows={2}
          maxLength={1000}
          className={`${CONTROL} mt-1 w-full py-2`}
          value={description}
          onChange={(e) => setDescription(e.target.value)}
        />
      </div>
      <div className="mt-4">
        <label className="flex w-fit items-center gap-2 font-medium">
          <Checkbox
            checked={doubleOptin}
            aria-describedby="list-optin-hint"
            onChange={(e) => setDoubleOptin(e.target.checked)}
          />
          Double opt-in
        </label>
        <span id="list-optin-hint" className="mt-1 block text-sm text-fg-muted">
          A new member stays pending, and cannot be mailed, until they confirm. Nothing sends that
          confirmation yet, so they stay pending. Turn it off and a new member is subscribed at
          once.
          {existing
            ? " Changing it decides the status of members added from now on; anyone already pending stays pending."
            : ""}
        </span>
      </div>
      {create.error ? <Alert>{create.error.message}</Alert> : null}
      <div className="mt-6 flex flex-wrap justify-end gap-3">
        <Button onClick={onCancel}>Cancel</Button>
        <Button type="submit" variant="solid" busy={create.isPending}>
          {existing ? "Save list" : "Create list"}
        </Button>
      </div>
    </form>
  );
}

export function ListDetail() {
  const { workspace } = useWorkspace();
  const { listId } = useParams({ from: "/w/$slug/lists/$listId" });
  if (!workspace) return null;
  return (
    <Detail
      ws={workspace.id}
      slug={workspace.slug}
      listId={listId}
      canEdit={atLeast(workspace.role, "editor")}
    />
  );
}

function Detail({
  ws,
  slug,
  listId,
  canEdit,
}: {
  ws: string;
  slug: string;
  listId: string;
  canEdit: boolean;
}) {
  const list = useQuery({
    queryKey: ["list", ws, listId],
    queryFn: async () => {
      const { data, response } = await api.GET("/api/v1/workspaces/{workspace_id}/lists/{id}", {
        params: { path: { workspace_id: ws, id: listId } },
      });
      if (response.status === 404) return null;
      if (!data) throw new Error("The list could not be loaded.");
      return data;
    },
    retry: false,
  });
  return (
    <section className="mt-8">
      <Link to="/w/$slug/lists" params={{ slug }} className={link}>
        All lists
      </Link>
      {list.isPending ? (
        <div className="mt-4">
          <Skeleton label="Loading list" rows={3} />
        </div>
      ) : list.isError ? (
        <div className="mt-4">
          <ErrorState title="The list could not be loaded." onRetry={() => void list.refetch()}>
            {list.error.message}
          </ErrorState>
        </div>
      ) : list.data === null ? (
        <EmptyState title="List not found">
          It may have been deleted, or it belongs to another workspace.
        </EmptyState>
      ) : (
        <>
          <h2 className="mt-4 text-xl font-semibold wrap-anywhere">{list.data.name}</h2>
          {canEdit ? <ListActions ws={ws} slug={slug} list={list.data} /> : null}
          <Settings list={list.data} />
          {/* Its own component and query: the count is slow on a big list and blocks nothing. */}
          <MailableCount ws={ws} listId={listId} />
          <Members ws={ws} slug={slug} listId={listId} />
        </>
      )}
    </section>
  );
}

/**
 * Edit and delete one list. Deleting takes every membership with it, by `on delete cascade`
 * (`migrations/0012_lists.sql`), so the dialog says what that costs: an unsubscribe recorded
 * against this list is part of what is deleted.
 */
function ListActions({ ws, slug, list }: { ws: string; slug: string; list: List }) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);

  const remove = useMutation({
    mutationFn: async () => {
      const { response, error } = await api.DELETE(
        "/api/v1/workspaces/{workspace_id}/lists/{id}",
        { params: { path: { workspace_id: ws, id: list.id } } },
      );
      if (!response.ok) throw new Error(problemText(error, KNOWN, "The list could not be deleted."));
    },
    onSuccess: async () => {
      // Every screen that counts or names a list: the grid's filter, a contact's memberships.
      await Promise.all(
        ["lists", "contact-lists", "mailable-count", "list-members", "contacts"].map((k) =>
          qc.invalidateQueries({ queryKey: [k] }),
        ),
      );
      await navigate({ to: "/w/$slug/lists", params: { slug } });
    },
  });

  return (
    <div className="mt-4 flex flex-wrap gap-2">
      <Button onClick={() => setEditing(true)}>Edit list</Button>
      <Button variant="danger-ghost" onClick={() => setDeleting(true)}>
        Delete list
      </Button>

      <Dialog open={editing} onClose={() => setEditing(false)} title="Edit list">
        <ListForm
          ws={ws}
          existing={list}
          onCancel={() => setEditing(false)}
          onDone={() => setEditing(false)}
        />
      </Dialog>

      <ConfirmDialog
        open={deleting}
        onClose={() => {
          setDeleting(false);
          remove.reset();
        }}
        title="Delete this list"
        confirmLabel="Delete list"
        busy={remove.isPending}
        error={remove.error?.message}
        onConfirm={() => remove.mutate()}
      >
        <p>
          <strong className="wrap-anywhere">{list.name}</strong> is deleted, with every membership
          of it. No contact is deleted and nothing else they are on changes.
        </p>
        <p>
          That includes the <strong>unsubscribed</strong> memberships: the record that someone
          opted out of this list goes with the list, so it cannot say later that they did. An
          opt-out from the whole workspace is a suppression, not a membership, and is not touched.
        </p>
        <p>This cannot be undone. To stop mailing a list without losing any of that, leave it in place and do not send to it.</p>
      </ConfirmDialog>
    </div>
  );
}

function Settings({ list }: { list: List }) {
  return (
    <section aria-labelledby="list-settings" className="mt-6">
      <h3 id="list-settings" className="text-lg font-semibold">
        Settings
      </h3>
      <dl className="mt-3 grid grid-cols-[max-content_1fr] gap-x-6 gap-y-1">
        <dt className="text-fg-muted">Description</dt>
        <dd>{list.description || "—"}</dd>
        <dt className="text-fg-muted">Opt-in</dt>
        <dd>{optIn(list)}</dd>
        <dt className="text-fg-muted">Visibility</dt>
        <dd className="capitalize">{list.visibility}</dd>
        <dt className="text-fg-muted">Created</dt>
        <dd>{day.format(new Date(list.created_at))}</dd>
      </dl>
      {list.double_optin ? (
        <p className="mt-3 text-fg-muted">
          New members start as pending until they confirm. Nothing sends the confirmation yet, so
          they stay pending and cannot be mailed.
        </p>
      ) : null}
    </section>
  );
}

/**
 * Counted live over the one definition of "may be emailed", and never kept as a number on the
 * list: a hand-maintained counter drifts. The count is slow on a large list, which is why it has
 * its own query and a skeleton and nothing else waits for it.
 */
function MailableCount({ ws, listId }: { ws: string; listId: string }) {
  const count = useQuery({
    queryKey: ["mailable-count", ws, listId],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/lists/{id}/mailable-count", {
        params: { path: { workspace_id: ws, id: listId } },
      });
      if (!data) throw new Error("The mailable count could not be loaded.");
      return data.count;
    },
    retry: false,
    refetchOnWindowFocus: false,
  });
  return (
    <section aria-labelledby="mailable" className="mt-6">
      <h3 id="mailable" className="text-lg font-semibold">
        Mailable members
      </h3>
      <div className="mt-3">
        {count.isPending ? (
          <div className="w-40">
            <Skeleton label="Counting mailable members" />
          </div>
        ) : count.isError ? (
          <p role="alert">
            {count.error.message}{" "}
            <Button variant="ghost" onClick={() => void count.refetch()}>
              Retry
            </Button>
          </p>
        ) : (
          <p className="text-2xl font-semibold tabular-nums" data-testid="mailable-count">
            {count.data.toLocaleString()}
          </p>
        )}
        <p className="mt-1 text-fg-muted">
          Subscribed members whose address is not suppressed, in this workspace or instance-wide.
          Counted live, so it can take a few seconds on a large list.
        </p>
      </div>
    </section>
  );
}

const PAGE = 100;

function Members({ ws, slug, listId }: { ws: string; slug: string; listId: string }) {
  const members = useInfiniteQuery({
    queryKey: ["list-members", ws, listId],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/lists/{id}/members", {
        params: {
          path: { workspace_id: ws, id: listId },
          query: { limit: PAGE, cursor: pageParam },
        },
      });
      if (!data) throw new Error(problemMessage(error, "The members could not be loaded."));
      return data;
    },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    retry: false,
  });
  const rows = members.data?.pages.flatMap((p) => p.data) ?? [];
  return (
    <section aria-labelledby="list-members" className="mt-6">
      <h3 id="list-members" className="text-lg font-semibold">
        Members
      </h3>
      <div className="mt-3">
        {members.isPending ? (
          <Skeleton label="Loading members" rows={5} />
        ) : members.isError && rows.length === 0 ? (
          <ErrorState title="The members could not be loaded." onRetry={() => void members.refetch()}>
            {members.error.message}
          </ErrorState>
        ) : rows.length === 0 ? (
          <p className="text-fg-muted">Nobody is on this list yet.</p>
        ) : (
          <>
            <TableScroller label="List members table"><table className="w-full">
              <caption className="sr-only">Members</caption>
              <thead>
                <tr className="border-b border-border">
                  <th scope="col" className={th}>
                    Email
                  </th>
                  <th scope="col" className={th}>
                    Status
                  </th>
                </tr>
              </thead>
              <tbody>
                {rows.map((m) => (
                  <tr key={m.contact_id} className="border-b border-border">
                    <td className="py-2 pe-3 wrap-anywhere">
                      <Link
                        to="/w/$slug/contacts/$contactId"
                        params={{ slug, contactId: m.contact_id }}
                        className={link}
                      >
                        {m.email}
                      </Link>
                    </td>
                    <td className="py-2 pe-3">
                      <MembershipBadge status={m.status} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table></TableScroller>
            {members.isFetchNextPageError ? (
              <p role="alert" className="mt-3">
                More members could not be loaded.
              </p>
            ) : null}
            {members.hasNextPage ? (
              <Button
                className="mt-3"
                busy={members.isFetchingNextPage}
                onClick={() => void members.fetchNextPage()}
              >
                Load more members
              </Button>
            ) : null}
          </>
        )}
      </div>
    </section>
  );
}
