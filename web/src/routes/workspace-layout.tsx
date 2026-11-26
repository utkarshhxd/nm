// SPDX-License-Identifier: AGPL-3.0-only

import { Link, Outlet, useNavigate, useParams } from "@tanstack/react-router";
import type { ReactNode } from "react";

import { atLeast } from "../lib/roles";
import { signOut, useMe } from "../lib/session";
import { ROW_BUTTON, SELECT } from "../lib/ui";

/**
 * The workspace the `$slug` in the URL names, resolved from `/me.workspaces` — the same list the
 * switcher is built from. It is `null` while `/me` is loading and for any slug the signed-in
 * person is not a member of.
 *
 * Membership is deliberately the only thing consulted: the API answers 404 for another tenant's
 * workspace and never 403, so asking the server about a slug we have no standing in could only
 * tell us something the server refuses to say. The screens behind this are still every one of
 * them authorised server-side on each request.
 */
export function useWorkspace() {
  const { slug } = useParams({ from: "/w/$slug" });
  const me = useMe();
  return { me, slug, workspace: me.data?.workspaces.find((w) => w.slug === slug) ?? null };
}

export default function WorkspaceLayout() {
  const navigate = useNavigate();
  const { me, workspace } = useWorkspace();

  if (me.isPending || !me.data) return <Shell>Loading…</Shell>;
  if (!workspace) return <Shell>Workspace not found</Shell>;

  const admin = atLeast(workspace.role, "admin");

  return (
    <Shell>
      <div className="mt-4 flex flex-wrap items-center gap-3">
        <label className="font-medium" htmlFor="workspace">
          Workspace
        </label>
        <select
          id="workspace"
          className={SELECT}
          value={workspace.slug}
          onChange={(event) =>
            void navigate({ to: "/w/$slug/members", params: { slug: event.target.value } })
          }
        >
          {me.data.workspaces.map((w) => (
            <option key={w.id} value={w.slug}>
              {w.name}
            </option>
          ))}
        </select>
        {me.data.is_instance_admin ? (
          <>
            <Link className={ROW_BUTTON} to="/workspaces/new">
              New workspace
            </Link>
            {/* Instance-wide, not this workspace's: a workspace admin never sees this. */}
            <Link className={ROW_BUTTON} to="/admin/suppressions">
              Instance suppressions
            </Link>
          </>
        ) : null}
      </div>

      <nav className="mt-4 flex flex-wrap gap-4" aria-label="Workspace">
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/contacts" label="Contacts" />
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/lists" label="Lists" />
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/forms" label="Forms" />
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/imports" label="Import" />
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/segments" label="Segments" />
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/tags" label="Tags" />
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/custom-fields" label="Custom fields" />
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/suppressions" label="Suppressions" />
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/relays" label="Relays" />
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/sending-domains" label="Sending domains" />
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/templates" label="Templates" />
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/campaigns" label="Campaigns" />
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/brand-kit" label="Brand kit" />
        <WorkspaceLink slug={workspace.slug} to="/w/$slug/members" label="Members" />
        {admin ? (
          <>
            <WorkspaceLink slug={workspace.slug} to="/w/$slug/api-keys" label="API keys" />
            <WorkspaceLink slug={workspace.slug} to="/w/$slug/audit-log" label="Audit log" />
          </>
        ) : null}
        <Link className={LINK} to="/account/security">
          Account security
        </Link>
      </nav>

      <Outlet />
    </Shell>
  );
}

const LINK = "font-medium text-blue-800 underline dark:text-blue-200";

function WorkspaceLink({
  slug,
  to,
  label,
}: {
  slug: string;
  to:
    | "/w/$slug/contacts"
    | "/w/$slug/lists"
    | "/w/$slug/forms"
    | "/w/$slug/imports"
    | "/w/$slug/segments"
    | "/w/$slug/tags"
    | "/w/$slug/custom-fields"
    | "/w/$slug/suppressions"
    | "/w/$slug/relays"
    | "/w/$slug/sending-domains"
    | "/w/$slug/templates"
    | "/w/$slug/campaigns"
    | "/w/$slug/brand-kit"
    | "/w/$slug/members"
    | "/w/$slug/api-keys"
    | "/w/$slug/audit-log";
  label: string;
}) {
  return (
    <Link className={LINK} to={to} params={{ slug }} activeProps={{ "aria-current": "page" }}>
      {label}
    </Link>
  );
}

/** The chrome every state shares, so "Loading…" and "Workspace not found" are not bare text. */
function Shell({ children }: { children: ReactNode }) {
  const me = useMe();

  return (
    <main className="mx-auto max-w-4xl p-8">
      <div className="flex flex-wrap items-baseline justify-between gap-3">
        <h1 className="text-2xl font-semibold">NM</h1>
        {me.data ? (
          <p className="flex items-center gap-3">
            <span>Signed in as {me.data.email}</span>
            <button className={ROW_BUTTON} type="button" onClick={() => void signOut()}>
              Sign out
            </button>
          </p>
        ) : null}
      </div>
      {children}
    </main>
  );
}
