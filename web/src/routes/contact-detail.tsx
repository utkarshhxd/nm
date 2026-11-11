// SPDX-License-Identifier: AGPL-3.0-only

import { useQuery } from "@tanstack/react-query";
import { Link, useParams } from "@tanstack/react-router";
import type { ReactNode } from "react";

import { api } from "../api/client";
import type { components } from "../api/schema";
import { Badge } from "../components/badge";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { MembershipBadge } from "../components/membership-badge";
import { Skeleton } from "../components/skeleton";
import { Alert } from "../lib/ui";
import { useWorkspace } from "./workspace-layout";

type Contact = components["schemas"]["Contact"];
type Definition = components["schemas"]["Definition"];
type Reason = components["schemas"]["Reason"];

const REASON: Record<Reason, string> = {
  unsubscribed_all: "Unsubscribed from everything",
  bounced: "Bounced",
  complained: "Complained",
  manual: "Blocked manually",
  gdpr_erasure: "Erased on request (GDPR)",
};

const day = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });

/** What a failed section says, so no section is ever left blank. */
const failed = (what: string) => new Error(`${what} could not be loaded.`);

export default function ContactDetail() {
  const { workspace } = useWorkspace();
  const { contactId } = useParams({ from: "/w/$slug/contacts/$contactId" });
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return <Detail ws={workspace.id} slug={workspace.slug} contactId={contactId} />;
}

function Detail({ ws, slug, contactId }: { ws: string; slug: string; contactId: string }) {
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
          <MailingStatus ws={ws} contactId={contactId} />
          <Lists ws={ws} slug={slug} contactId={contactId} />
          <Tags ws={ws} contactId={contactId} />
          <CustomFields ws={ws} contact={contact.data} />
        </>
      )}
    </section>
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
      <h2 className="text-xl font-semibold">{contact.email}</h2>
      <p className="mt-1 text-fg-muted">{facts.filter(Boolean).join(" · ")}</p>
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

function Lists({ ws, slug, contactId }: { ws: string; slug: string; contactId: string }) {
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
  return (
    <Panel id="lists" title="Lists">
      {lists.isPending ? (
        <Skeleton label="Loading lists" />
      ) : lists.isError ? (
        <Alert>{lists.error.message}</Alert>
      ) : lists.data.length === 0 ? (
        <p className="text-fg-muted">Not on any list.</p>
      ) : (
        <>
          <table className="w-full">
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
              </tr>
            </thead>
            <tbody>
              {lists.data.map((l) => {
                const since = l.status === "unsubscribed" ? l.unsubscribed_at : l.subscribed_at;
                return (
                  <tr key={l.list_id} className="border-b border-border">
                    <td className="py-2 pe-3">
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
                    <td className="py-2 tabular-nums text-fg-muted">
                      {since ? day.format(new Date(since)) : "—"}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
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

function Tags({ ws, contactId }: { ws: string; contactId: string }) {
  const tags = useQuery({
    queryKey: ["contact-tags", ws, contactId],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/contacts/{id}/tags", {
        params: { path: { workspace_id: ws, id: contactId }, query: { limit: 200 } },
      });
      if (!data) throw failed("The tags");
      return data.data;
    },
    retry: false,
  });
  return (
    <Panel id="tags" title="Tags">
      {tags.isPending ? (
        <Skeleton label="Loading tags" />
      ) : tags.isError ? (
        <Alert>{tags.error.message}</Alert>
      ) : tags.data.length === 0 ? (
        <p className="text-fg-muted">No tags.</p>
      ) : (
        <ul className="flex flex-wrap gap-2">
          {tags.data.map((t) => (
            <li key={t.id}>
              <Badge variant="accent">{t.name}</Badge>
            </li>
          ))}
        </ul>
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
              <Badge>{String(v)}</Badge>
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
        <dl className="grid grid-cols-[max-content_1fr] gap-x-6 gap-y-1">
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
      <dt className="text-fg-muted">{def.label}</dt>
      <dd>
        {children}
        {def.archived_at ? <span className="ms-2 text-fg-muted">(archived field)</span> : null}
      </dd>
    </>
  );
}
