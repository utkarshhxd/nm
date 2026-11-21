// SPDX-License-Identifier: AGPL-3.0-only

import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "@tanstack/react-router";
import { Fragment, type ReactNode, useState } from "react";

import { api, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Badge } from "../components/badge";
import { Button } from "../components/button";
import { ConfirmDialog } from "../components/confirm-dialog";
import { ContactForm } from "../components/contact-form";
import { Dialog } from "../components/dialog";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { MembershipBadge } from "../components/membership-badge";
import { Select } from "../components/select";
import { Skeleton } from "../components/skeleton";
import { TableScroller } from "../components/table-scroller";
import { type Role, atLeast } from "../lib/roles";
import { REASON } from "../lib/suppressions";
import { Alert, ConfirmButton } from "../lib/ui";
import { useWorkspace } from "./workspace-layout";

type Contact = components["schemas"]["Contact"];
type Definition = components["schemas"]["Definition"];

const KNOWN = {
  "insufficient-role": "Your role cannot change contacts. It takes editor or above.",
};

const day = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });

/** What a failed section says, so no section is ever left blank. */
const failed = (what: string) => new Error(`${what} could not be loaded.`);

export default function ContactDetail() {
  const { workspace } = useWorkspace();
  const { contactId } = useParams({ from: "/w/$slug/contacts/$contactId" });
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return (
    <Detail ws={workspace.id} slug={workspace.slug} contactId={contactId} role={workspace.role} />
  );
}

function Detail({
  ws,
  slug,
  contactId,
  role,
}: {
  ws: string;
  slug: string;
  contactId: string;
  role: Role;
}) {
  const canEdit = atLeast(role, "editor");
  // Erasure is admin-only on the server, and a control a viewer or editor cannot use is not shown.
  const canErase = atLeast(role, "admin");
  // Each section has its own query, so a slow or failed one never blanks the others.
  const contact = useQuery({
    queryKey: ["contact", ws, contactId],
    queryFn: async () => {
      const { data, response } = await api.GET("/api/v1/workspaces/{workspace_id}/contacts/{id}", {
        params: { path: { workspace_id: ws, id: contactId } },
      });
      if (response.status === 404) return null;
      if (!data) throw failed("The contact");
      return data;
    },
    retry: false,
  });

  return (
    <section className="mt-8">
      <Link to="/w/$slug/contacts" params={{ slug }} className="text-accent-text underline">
        All contacts
      </Link>
      {contact.isPending ? (
        <div className="mt-4">
          <Skeleton label="Loading contact" rows={3} />
        </div>
      ) : contact.isError ? (
        <div className="mt-4">
          <ErrorState title="The contact could not be loaded." onRetry={() => void contact.refetch()}>
            {contact.error.message}
          </ErrorState>
        </div>
      ) : contact.data === null ? (
        <EmptyState title="Contact not found">
          It may have been deleted, or it belongs to another workspace.
        </EmptyState>
      ) : (
        <>
          <Identity contact={contact.data} />
          {canEdit ? (
            <Actions ws={ws} slug={slug} contact={contact.data} canErase={canErase} />
          ) : null}
          <MailingStatus ws={ws} contactId={contactId} />
          <Lists ws={ws} slug={slug} contactId={contactId} canEdit={canEdit} />
          <Tags ws={ws} contactId={contactId} canEdit={canEdit} />
          <CustomFields ws={ws} contact={contact.data} />
          <ConsentHistory ws={ws} contactId={contactId} />
        </>
      )}
    </section>
  );
}

/**
 * Edit, delete and erase. Delete and erase are not two words for one thing: delete removes the
 * contact and leaves the address usable, erase destroys it and blocks the address for ever. Each
 * dialog says which one it is, because the two are one click apart.
 */
function Actions({
  ws,
  slug,
  contact,
  canErase,
}: {
  ws: string;
  slug: string;
  contact: Contact;
  canErase: boolean;
}) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [erasing, setErasing] = useState(false);

  const gone = async () => {
    // The grid, and for an erasure the suppression list and every "can this be mailed" answer.
    await Promise.all(
      ["contacts", "suppressions", "contact-eligibility", "mailable-count"].map((k) =>
        qc.invalidateQueries({ queryKey: [k] }),
      ),
    );
    await navigate({ to: "/w/$slug/contacts", params: { slug }, search: {} });
  };

  const remove = useMutation({
    mutationFn: async () => {
      const { response, error } = await api.DELETE(
        "/api/v1/workspaces/{workspace_id}/contacts/{id}",
        { params: { path: { workspace_id: ws, id: contact.id } } },
      );
      if (!response.ok) throw new Error(problemText(error, KNOWN, "The contact could not be deleted."));
    },
    onSuccess: gone,
  });

  const erase = useMutation({
    mutationFn: async () => {
      const { response, error } = await api.POST(
        "/api/v1/workspaces/{workspace_id}/contacts/{id}/erase",
        { params: { path: { workspace_id: ws, id: contact.id } } },
      );
      if (!response.ok) throw new Error(problemText(error, KNOWN, "The contact could not be erased."));
    },
    onSuccess: gone,
  });

  const address = <strong className="wrap-anywhere">{contact.email}</strong>;

  return (
    <div className="mt-4 flex flex-wrap gap-2">
      <Button onClick={() => setEditing(true)}>Edit contact</Button>
      <Button variant="danger-ghost" onClick={() => setDeleting(true)}>
        Delete contact
      </Button>
      {canErase ? (
        <Button variant="danger-ghost" onClick={() => setErasing(true)}>
          Erase contact
        </Button>
      ) : null}

      <Dialog open={editing} onClose={() => setEditing(false)} title="Edit contact">
        <ContactForm
          ws={ws}
          existing={contact}
          onCancel={() => setEditing(false)}
          onDone={async () => {
            setEditing(false);
            await Promise.all([
              qc.invalidateQueries({ queryKey: ["contact", ws, contact.id] }),
              qc.invalidateQueries({ queryKey: ["contacts", ws] }),
            ]);
          }}
        />
      </Dialog>

      <ConfirmDialog
        open={deleting}
        onClose={() => {
          setDeleting(false);
          remove.reset();
        }}
        title="Delete this contact"
        confirmLabel="Delete contact"
        busy={remove.isPending}
        error={remove.error?.message}
        onConfirm={() => remove.mutate()}
      >
        <p>{address} is deleted, with its list memberships and its tags.</p>
        <p>
          The address is <strong>not suppressed</strong> by this: it can be added again, by hand
          or by the next import. Deleting does not unblock it either — a suppression already on
          this address outlives the contact and still stops this workspace mailing it.
        </p>
        <p>
          This cannot be undone. To honour an erasure request, an admin uses Erase contact
          instead, which also blocks the address for good.
        </p>
      </ConfirmDialog>

      <ConfirmDialog
        open={erasing}
        onClose={() => {
          setErasing(false);
          erase.reset();
        }}
        title="Erase this contact"
        confirmLabel="Erase contact permanently"
        busy={erase.isPending}
        error={erase.error?.message}
        onConfirm={() => erase.mutate()}
      >
        <p>
          This is an erasure on request (GDPR). {address}, its list memberships and its tags are
          destroyed.
        </p>
        <p>
          The address is then <strong>permanently blocked</strong>: it can never be mailed, added
          or imported into this workspace again, and no one can unblock it — no route lifts an
          erasure, for any role.
        </p>
        <p>
          This cannot be undone. If you only want this contact gone, use Delete contact: it blocks
          nothing and the address stays usable.
        </p>
      </ConfirmDialog>
    </div>
  );
}

function Panel({ id, title, children }: { id: string; title: string; children: ReactNode }) {
  return (
    <section aria-labelledby={id} className="mt-8">
      <h3 id={id} className="text-lg font-semibold">
        {title}
      </h3>
      <div className="mt-3">{children}</div>
    </section>
  );
}

function Identity({ contact }: { contact: Contact }) {
  const name = `${contact.first_name} ${contact.last_name}`.trim();
  const facts = [name, contact.timezone, `Added ${day.format(new Date(contact.created_at))}`];
  return (
    <div className="mt-4">
      <h2 className="text-xl font-semibold wrap-anywhere">{contact.email}</h2>
      <p className="mt-1 text-fg-muted wrap-anywhere">{facts.filter(Boolean).join(" · ")}</p>
    </div>
  );
}

/**
 * Why this address is, or is not, mailed. The server answers it, because the block may be an
 * instance-wide one that the workspace's own suppression list does not contain and its admins
 * cannot read: built from that list, this panel would say nothing while mail silently failed.
 */
function MailingStatus({ ws, contactId }: { ws: string; contactId: string }) {
  const eligibility = useQuery({
    queryKey: ["contact-eligibility", ws, contactId],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/contacts/{id}/eligibility", {
        params: { path: { workspace_id: ws, id: contactId } },
      });
      if (!data) throw failed("Whether this contact can be mailed");
      return data;
    },
    retry: false,
  });
  const block = eligibility.data?.blocked_by;
  return (
    <Panel id="mailing-status" title="Mailing status">
      {eligibility.isPending ? (
        <Skeleton label="Checking whether this contact can be mailed" />
      ) : eligibility.isError ? (
        <Alert>{eligibility.error.message} Do not assume this contact can be mailed.</Alert>
      ) : block ? (
        <>
          <Badge variant="danger" glyph="cross">
            Not mailable
          </Badge>
          <dl className="mt-3 grid grid-cols-[max-content_1fr] gap-x-6 gap-y-1">
            <dt className="text-fg-muted">Blocked by</dt>
            <dd>{block.level === "instance" ? "Instance-wide suppression" : "Workspace suppression"}</dd>
            <dt className="text-fg-muted">Applies to</dt>
            <dd>{block.scope === "domain" ? "Every address on this domain" : "This address"}</dd>
            <dt className="text-fg-muted">Reason</dt>
            <dd>{REASON[block.reason]}</dd>
          </dl>
          {block.level === "instance" ? (
            <p className="mt-3 text-fg-muted">
              This block was set for the whole instance, not by this workspace, so it is not in this
              workspace&rsquo;s suppression list. An instance administrator can lift it.
            </p>
          ) : null}
          {block.reason === "gdpr_erasure" ? (
            <p className="mt-3 text-fg-muted">An erasure block cannot be lifted.</p>
          ) : null}
        </>
      ) : (
        <>
          <Badge variant="success" glyph="dot">
            Mailable
          </Badge>
          <p className="mt-3 text-fg-muted">
            No suppression blocks this address. Whether a message reaches it also depends on its
            status on each list below.
          </p>
        </>
      )}
    </Panel>
  );
}

function Lists({
  ws,
  slug,
  contactId,
  canEdit,
}: {
  ws: string;
  slug: string;
  contactId: string;
  canEdit: boolean;
}) {
  const qc = useQueryClient();
  const lists = useQuery({
    queryKey: ["contact-lists", ws, contactId],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/contacts/{id}/lists", {
        params: { path: { workspace_id: ws, id: contactId }, query: { limit: 200 } },
      });
      if (!data) throw failed("The lists");
      // shortcut: the first 200 lists only. A contact on more is not a case yet; add "Load more"
      // the way the contacts grid pages when it is.
      return data.data;
    },
    retry: false,
  });

  // Every list in the workspace, for the picker. Only an editor sees one, so only they fetch it.
  const all = useQuery({
    // The same key and the same request as the contacts grid's filter, deliberately; the lists
    // screen's own paged query is ["lists", ws, "all"] so that it is not a third shape here.
    queryKey: ["lists", ws],
    enabled: canEdit,
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/lists", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      return data?.data ?? [];
    },
  });
  /** A membership change moves a list's mailable count, which is counted live elsewhere. */
  const refresh = async () => {
    await Promise.all([
      qc.invalidateQueries({ queryKey: ["contact-lists", ws, contactId] }),
      qc.invalidateQueries({ queryKey: ["mailable-count"] }),
      qc.invalidateQueries({ queryKey: ["list-members"] }),
    ]);
  };

  const [picked, setPicked] = useState("");
  const add = useMutation({
    mutationFn: async (listId: string) => {
      const { data, error } = await api.POST(
        "/api/v1/workspaces/{workspace_id}/contacts/{id}/lists",
        {
          params: { path: { workspace_id: ws, id: contactId } },
          body: { list_id: listId },
        },
      );
      if (!data) throw new Error(problemText(error, KNOWN, "The contact could not be added to that list."));
    },
    onSuccess: async () => {
      setPicked("");
      await refresh();
    },
  });
  const unsubscribe = useMutation({
    mutationFn: async (listId: string) => {
      const { response, error } = await api.POST(
        "/api/v1/workspaces/{workspace_id}/contacts/{id}/lists/{list_id}/unsubscribe",
        { params: { path: { workspace_id: ws, id: contactId, list_id: listId } } },
      );
      if (!response.ok) throw new Error(problemText(error, KNOWN, "The contact could not be unsubscribed."));
    },
    onSuccess: refresh,
  });

  // A list the contact already has a membership on is not offered, whatever it says: adding again
  // does nothing, and an unsubscribed one is not reopened by an operator at all — only by the
  // subscriber, through the link in their own mail. Offering it would be a button that silently
  // does nothing.
  const joinable = (all.data ?? []).filter(
    (l) => !lists.data?.some((m) => m.list_id === l.id),
  );

  return (
    <Panel id="lists" title="Lists">
      {lists.isPending ? (
        <Skeleton label="Loading lists" />
      ) : lists.isError ? (
        <Alert>{lists.error.message}</Alert>
      ) : (
        <>
          {lists.data.length === 0 ? (
            <p className="text-fg-muted">Not on any list.</p>
          ) : (
            <TableScroller label="Memberships table"><table className="w-full">
              <thead>
                <tr className="border-b border-border">
                  <th scope="col" className="py-2 pe-3 text-start font-medium">
                    List
                  </th>
                  <th scope="col" className="py-2 pe-3 text-start font-medium">
                    Status
                  </th>
                  <th scope="col" className="py-2 text-start font-medium">
                    Since
                  </th>
                  {canEdit ? (
                    <th scope="col" className="py-2 text-start font-medium">
                      <span className="sr-only">Actions</span>
                    </th>
                  ) : null}
                </tr>
              </thead>
              <tbody>
                {lists.data.map((l) => {
                  const since = l.status === "unsubscribed" ? l.unsubscribed_at : l.subscribed_at;
                  return (
                    <tr key={l.list_id} className="border-b border-border">
                      <td className="py-2 pe-3 wrap-anywhere">
                        <Link
                          to="/w/$slug/lists/$listId"
                          params={{ slug, listId: l.list_id }}
                          className="text-accent-text underline"
                        >
                          {l.name}
                        </Link>
                      </td>
                      <td className="py-2 pe-3">
                        <MembershipBadge status={l.status} />
                        {l.status === "pending" ? (
                          <span className="ms-2 text-fg-muted">not confirmed, not mailable</span>
                        ) : null}
                      </td>
                      <td className="py-2 pe-3 tabular-nums text-fg-muted">
                        {since ? day.format(new Date(since)) : "—"}
                      </td>
                      {canEdit ? (
                        <td className="py-2">
                          {l.status === "unsubscribed" ? null : (
                            <ConfirmButton
                              action="Unsubscribe"
                              subject={l.name}
                              disabled={unsubscribe.isPending}
                              onConfirm={() => unsubscribe.mutate(l.list_id)}
                            />
                          )}
                        </td>
                      ) : null}
                    </tr>
                  );
                })}
              </tbody>
            </table></TableScroller>
          )}
          {canEdit ? (
            <form
              className="mt-3 flex flex-wrap items-center gap-2"
              onSubmit={(e) => {
                e.preventDefault();
                if (picked) add.mutate(picked);
              }}
            >
              <Select
                aria-label="List to add"
                value={picked}
                onChange={(e) => setPicked(e.target.value)}
              >
                <option value="">Choose a list…</option>
                {joinable.map((l) => (
                  <option key={l.id} value={l.id}>
                    {l.name}
                  </option>
                ))}
              </Select>
              <Button type="submit" disabled={!picked} busy={add.isPending}>
                Add to list
              </Button>
            </form>
          ) : null}
          {add.error ? <Alert>{add.error.message}</Alert> : null}
          {unsubscribe.error ? <Alert>{unsubscribe.error.message}</Alert> : null}
          {canEdit ? (
            <p className="mt-3 text-fg-muted">
              Unsubscribing sets the membership to unsubscribed and keeps it: the row stays here
              and is not deleted, so the opt-out is not lost the next time this contact is
              imported. Adding a list this contact already has a membership on does nothing,
              whatever it says — an unsubscribe can only be undone by the contact themselves,
              through the link in their own mail.
            </p>
          ) : null}
          {lists.data.some((l) => l.status === "pending") ? (
            <p className="mt-3 text-fg-muted">
              Pending means the contact has not confirmed. A pending contact cannot be mailed, and
              nothing sends the confirmation yet, so it stays pending.
            </p>
          ) : null}
        </>
      )}
    </Panel>
  );
}

function Tags({ ws, contactId, canEdit }: { ws: string; contactId: string; canEdit: boolean }) {
  const qc = useQueryClient();
  const tags = useQuery({
    queryKey: ["contact-tags", ws, contactId],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/contacts/{id}/tags", {
        params: { path: { workspace_id: ws, id: contactId }, query: { limit: 200 } },
      });
      if (!data) throw failed("The tags");
      // shortcut: the first 200 tags only (the server maximum). A contact with more is not a case
      // yet; add "Load more" the way the contacts grid pages when it is.
      return data.data;
    },
    retry: false,
  });

  // Every tag in the workspace, for the picker; only an editor sees one, so only they fetch it.
  const all = useQuery({
    queryKey: ["tags", ws],
    enabled: canEdit,
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/tags", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      return data?.data ?? [];
    },
  });
  const refresh = () => qc.invalidateQueries({ queryKey: ["contact-tags", ws, contactId] });

  const [picked, setPicked] = useState("");
  const apply = useMutation({
    mutationFn: async (tagId: string) => {
      const { response, error } = await api.PUT(
        "/api/v1/workspaces/{workspace_id}/contacts/{id}/tags/{tag_id}",
        { params: { path: { workspace_id: ws, id: contactId, tag_id: tagId } } },
      );
      if (!response.ok) throw new Error(problemText(error, KNOWN, "The tag could not be applied."));
    },
    onSuccess: async () => {
      setPicked("");
      await refresh();
    },
  });
  const remove = useMutation({
    mutationFn: async (tagId: string) => {
      const { response, error } = await api.DELETE(
        "/api/v1/workspaces/{workspace_id}/contacts/{id}/tags/{tag_id}",
        { params: { path: { workspace_id: ws, id: contactId, tag_id: tagId } } },
      );
      if (!response.ok) throw new Error(problemText(error, KNOWN, "The tag could not be removed."));
    },
    onSuccess: refresh,
  });

  const untaken = (all.data ?? []).filter((t) => !tags.data?.some((had) => had.id === t.id));

  return (
    <Panel id="tags" title="Tags">
      {tags.isPending ? (
        <Skeleton label="Loading tags" />
      ) : tags.isError ? (
        <Alert>{tags.error.message}</Alert>
      ) : (
        <>
          {tags.data.length === 0 ? (
            <p className="text-fg-muted">No tags.</p>
          ) : (
            <ul className="flex flex-wrap items-center gap-2">
              {tags.data.map((t) => (
                <li key={t.id} className="flex max-w-full items-center gap-1">
                  <Badge variant="accent" wrap>
                    {t.name}
                  </Badge>
                  {canEdit ? (
                    <Button
                      variant="ghost"
                      aria-label={`Remove ${t.name}`}
                      disabled={remove.isPending}
                      onClick={() => remove.mutate(t.id)}
                    >
                      Remove
                    </Button>
                  ) : null}
                </li>
              ))}
            </ul>
          )}
          {canEdit ? (
            <form
              className="mt-3 flex flex-wrap items-center gap-2"
              onSubmit={(e) => {
                e.preventDefault();
                if (picked) apply.mutate(picked);
              }}
            >
              <Select
                aria-label="Tag to apply"
                value={picked}
                onChange={(e) => setPicked(e.target.value)}
              >
                <option value="">Choose a tag…</option>
                {untaken.map((t) => (
                  <option key={t.id} value={t.id}>
                    {t.name}
                  </option>
                ))}
              </Select>
              <Button type="submit" disabled={!picked} busy={apply.isPending}>
                Apply tag
              </Button>
            </form>
          ) : null}
          {apply.error ? <Alert>{apply.error.message}</Alert> : null}
          {remove.error ? <Alert>{remove.error.message}</Alert> : null}
        </>
      )}
    </Panel>
  );
}

/**
 * A value in the words of its field's kind, never as raw JSON. A date has no time zone, so it is
 * read and shown as UTC; a datetime is shown in the contact's own zone, named.
 */
function show(def: Definition, value: unknown, timezone: string | null | undefined): ReactNode {
  const zone = timezone ?? "UTC";
  switch (def.kind) {
    case "number":
      return typeof value === "number" ? value.toLocaleString() : String(value);
    case "boolean":
      return value === true ? "Yes" : value === false ? "No" : String(value);
    case "date":
      return typeof value === "string"
        ? new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeZone: "UTC" }).format(
            new Date(`${value}T00:00:00Z`),
          )
        : String(value);
    case "datetime":
      // `dateStyle` and `timeStyle` cannot be combined with `timeZoneName`, hence the long form.
      return typeof value === "string"
        ? new Intl.DateTimeFormat(undefined, {
            year: "numeric",
            month: "short",
            day: "numeric",
            hour: "numeric",
            minute: "2-digit",
            timeZone: zone,
            timeZoneName: "short",
          }).format(new Date(value))
        : String(value);
    case "multi_select":
      return Array.isArray(value) ? (
        <ul className="flex flex-wrap gap-2">
          {value.map((v) => (
            <li key={String(v)}>
              <Badge wrap>{String(v)}</Badge>
            </li>
          ))}
        </ul>
      ) : (
        String(value)
      );
    default:
      return String(value);
  }
}

function CustomFields({ ws, contact }: { ws: string; contact: Contact }) {
  const defs = useQuery({
    queryKey: ["custom-fields", ws],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/custom-fields", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      if (!data) throw failed("The custom fields");
      // shortcut: the first 200 definitions only (the server maximum). A workspace with more is
      // not a case yet; add "Load more" when it is.
      return data.data;
    },
    retry: false,
  });
  const values = (contact.attributes ?? {}) as Record<string, unknown>;
  // Oldest field first (the API lists newest first). An archived field shows only while this
  // contact still holds a value for it: the value is kept until the field is purged.
  const shown = [...(defs.data ?? [])]
    .reverse()
    .filter((d) => d.archived_at == null || values[d.key] != null);
  return (
    <Panel id="custom-fields" title="Custom fields">
      {defs.isPending ? (
        <Skeleton label="Loading custom fields" />
      ) : defs.isError ? (
        <Alert>{defs.error.message}</Alert>
      ) : shown.length === 0 ? (
        <p className="text-fg-muted">This workspace has no custom fields.</p>
      ) : (
        <dl className="grid grid-cols-[minmax(0,max-content)_minmax(0,1fr)] gap-x-6 gap-y-1">
          {shown.map((d) => {
            const value = values[d.key];
            return (
              <FieldRow key={d.id} def={d}>
                {value == null ? "—" : show(d, value, contact.timezone)}
              </FieldRow>
            );
          })}
        </dl>
      )}
    </Panel>
  );
}

function FieldRow({ def, children }: { def: Definition; children: ReactNode }) {
  return (
    <>
      <dt className="text-fg-muted wrap-anywhere">{def.label}</dt>
      <dd className="wrap-anywhere">
        {children}
        {def.archived_at ? <span className="ms-2 text-fg-muted">(archived field)</span> : null}
      </dd>
    </>
  );
}

const ACTION: Record<string, string> = {
  // `requested` is NOT a softer "subscribed": it says the person was asked and has not agreed.
  // Do not relabel it into something that asserts more than the row does.
  requested: "Requested",
  subscribed: "Subscribed",
  confirmed: "Confirmed",
  unsubscribed: "Unsubscribed",
  resubscribed: "Resubscribed",
  suppressed: "Suppressed",
};

/** The code path a change arrived by, in words. It is never who made it. */
const SOURCE: Record<string, string> = {
  form: "Signup form",
  import: "Import",
  api: "API",
  operator: "Operator",
  one_click: "One-click unsubscribe",
  preference_center: "Preference centre",
  bounce: "Bounce",
  complaint: "Complaint",
};

const moment = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });

const CONSENT_PAGE = 50;

/**
 * The consent log for this contact: the answer to "did this person agree, and how?".
 *
 * **Not an activity feed.** Every row says what changed, how the change arrived, when, and —
 * where the row has them — which list, the address it came from, the browser that sent it and
 * the link that authorised it. A row saying "subscribed" without saying how is the thing this
 * record exists to prevent, so the evidence is shown beside the action rather than hidden behind
 * a disclosure.
 *
 * It is read-only for every role, including an admin: `consent_log` is append-only and nothing in
 * the product can edit or delete a row. The database enforces both halves of that —
 * `0020_consent_log.sql` refuses the update, `0026` refuses the delete and the truncate — rather
 * than this screen being the only thing that declines to offer it. An erasure takes the rows with
 * the contact, which is why there is no "clear history" here and must never be one.
 */
function ConsentHistory({ ws, contactId }: { ws: string; contactId: string }) {
  const history = useInfiniteQuery({
    queryKey: ["contact-consent", ws, contactId],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) => {
      const { data } = await api.GET(
        "/api/v1/workspaces/{workspace_id}/contacts/{id}/consent",
        {
          params: {
            path: { workspace_id: ws, id: contactId },
            query: { limit: CONSENT_PAGE, cursor: pageParam },
          },
        },
      );
      // `failed`, not `problemText(…, KNOWN, …)`: reading this takes viewer, which every member
      // has, so there is no insufficient-role answer for KNOWN's wording to misdescribe.
      if (!data) throw failed("The consent history");
      return data;
    },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    retry: false,
  });
  const rows = history.data?.pages.flatMap((p) => p.data) ?? [];

  return (
    <Panel id="consent" title="Consent history">
      {history.isPending ? (
        <Skeleton label="Loading consent history" rows={3} />
      ) : history.isError && rows.length === 0 ? (
        <Alert>{history.error.message} Do not assume this contact agreed to anything.</Alert>
      ) : rows.length === 0 ? (
        <p className="text-fg-muted">
          Nothing recorded. Memberships written before this workspace kept a consent log have no
          row here.
        </p>
      ) : (
        <>
          <TableScroller label="Consent history table"><table className="w-full">
            <thead>
              <tr className="border-b border-border">
                <th scope="col" className="py-2 pe-3 text-start font-medium">
                  When
                </th>
                <th scope="col" className="py-2 pe-3 text-start font-medium">
                  What
                </th>
                <th scope="col" className="py-2 pe-3 text-start font-medium">
                  List
                </th>
                <th scope="col" className="py-2 pe-3 text-start font-medium">
                  How
                </th>
                <th scope="col" className="py-2 text-start font-medium">
                  Evidence
                </th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.id} className="border-b border-border align-top">
                  <td className="py-2 pe-3 tabular-nums text-fg-muted">
                    {moment.format(new Date(r.created_at))}
                  </td>
                  <td className="py-2 pe-3">{ACTION[r.action] ?? r.action}</td>
                  {/* ONE LABEL FOR BOTH CASES, BECAUSE THE ROW CANNOT TELL THEM APART. A
                      workspace-wide change is written with a null `list_id`, and
                      `0020_consent_log.sql`'s `on delete set null (list_id)` nulls it again when
                      the list is deleted — the row keeps its action, source and evidence and
                      loses only the pointer. `list_name` comes from a left join on that column,
                      so it is null in exactly the same rows.

                      There was a `r.list_id ? "A deleted list" : "Whole workspace"` here. It was
                      unreachable: by the time `list_id` is null there is nothing left to branch
                      on, so every deleted list's row read as a workspace-wide one — a one-list
                      unsubscribe shown as if the person had left everything. Do not re-add it in
                      any form, including one keyed on `action`: a new writer would make it wrong
                      again, silently, in the record that exists to be trusted. Telling the two
                      apart needs the list's name snapshotted at write time, by every writer. */}
                  <td className="py-2 pe-3 wrap-anywhere">
                    {r.list_name ?? "Whole workspace, or a list since deleted"}
                  </td>
                  <td className="py-2 pe-3">{SOURCE[r.source] ?? r.source}</td>
                  <td className="py-2">
                    <Evidence row={r} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table></TableScroller>
          {history.isFetchNextPageError ? (
            <p role="alert" className="mt-3">
              More of the history could not be loaded.
            </p>
          ) : null}
          {history.hasNextPage ? (
            <Button
              className="mt-3"
              busy={history.isFetchingNextPage}
              onClick={() => void history.fetchNextPage()}
            >
              Load more history
            </Button>
          ) : null}
          <p className="mt-3 text-fg-muted">
            <strong>Requested</strong> means this person was asked and has not agreed yet — a
            double opt-in nobody has confirmed. <strong>How</strong> is the code path the change
            arrived by, not who made it: an operator subscribing people in bulk is recorded as
            API, and the address and browser are only ever the subscriber&rsquo;s own, from a page
            they opened themselves.
          </p>
          <p className="mt-3 text-fg-muted">
            A row with no list named is either a change across the whole workspace or one to a
            list that has since been deleted. Deleting a list clears the pointer from these rows
            and keeps everything else, and nothing recorded at the time says which of the two it
            was, so this will not guess.
          </p>
        </>
      )}
    </Panel>
  );
}

type ConsentRow = components["schemas"]["ConsentEntry"];

/** What the row has, and nothing for what it does not: an absent IP is not an unknown one. */
function Evidence({ row }: { row: ConsentRow }) {
  const facts: [string, string][] = [
    ["IP address", row.ip ?? ""],
    ["Browser", row.user_agent ?? ""],
    ["Link", row.token_jti ?? ""],
    ["Reference", row.source_ref ?? ""],
  ];
  const shown = facts.filter(([, value]) => value !== "");
  if (shown.length === 0) return <span className="text-fg-muted">—</span>;
  return (
    <dl className="grid grid-cols-[max-content_1fr] gap-x-3">
      {shown.map(([label, value]) => (
        <Fragment key={label}>
          <dt className="text-fg-muted">{label}</dt>
          <dd className="wrap-anywhere">{value}</dd>
        </Fragment>
      ))}
    </dl>
  );
}
