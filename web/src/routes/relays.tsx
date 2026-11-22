// SPDX-License-Identifier: AGPL-3.0-only

import { useInfiniteQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { useRef, useState } from "react";

import { api, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Badge } from "../components/badge";
import { Button } from "../components/button";
import { Checkbox } from "../components/checkbox";
import { ConfirmDialog } from "../components/confirm-dialog";
import { Dialog } from "../components/dialog";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Alert, Field } from "../components/field";
import { Select } from "../components/select";
import { Skeleton } from "../components/skeleton";
import { TableScroller } from "../components/table-scroller";
import { atLeast } from "../lib/roles";
import { useWorkspace } from "./workspace-layout";

type Relay = components["schemas"]["RelayView"];
type Security = components["schemas"]["Security"];

const SECURITY: Record<Security, string> = {
  implicit_tls: "Implicit TLS (usually port 465)",
  starttls: "STARTTLS (usually port 587)",
};
const PAGE = 100;
const th = "py-2 pe-3 text-start font-medium";
const KNOWN = {
  "relay-name-taken": "A relay with that name already exists in this workspace.",
  "insufficient-role": "Your role cannot change relays. It takes admin.",
};

type PasswordMode = "keep" | "replace" | "clear";

export default function Relays() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return <Page ws={workspace.id} canEdit={atLeast(workspace.role, "admin")} />;
}

function Page({ ws, canEdit }: { ws: string; canEdit: boolean }) {
  const qc = useQueryClient();
  const list = useInfiniteQuery({
    queryKey: ["relays", ws],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/relays", {
        params: { path: { workspace_id: ws }, query: { limit: PAGE, cursor: pageParam } },
      });
      if (!data) throw new Error(problemText(error, KNOWN, "The relays could not be loaded."));
      return data;
    },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    retry: false,
  });
  const rows = list.data?.pages.flatMap((p) => p.data) ?? [];

  // `null` is closed, "new" is the create form, a relay is the edit form for it.
  const [editing, setEditing] = useState<Relay | "new" | null>(null);
  const [deleting, setDeleting] = useState<Relay | null>(null);
  const remove = useMutation({
    mutationFn: async (relay: Relay) => {
      const { response, error } = await api.DELETE("/api/v1/workspaces/{workspace_id}/relays/{id}", {
        params: { path: { workspace_id: ws, id: relay.id } },
      });
      if (!response.ok) throw new Error(problemText(error, KNOWN, "The relay could not be deleted."));
    },
    onSuccess: async () => {
      setDeleting(null);
      await qc.invalidateQueries({ queryKey: ["relays", ws] });
    },
  });

  const add = (
    <Button variant="solid" onClick={() => setEditing("new")}>
      Add relay
    </Button>
  );

  return (
    <section className="mt-8">
      <h2 className="text-xl font-semibold">Relays</h2>
      <p className="mt-2 text-fg-muted">
        The SMTP services this workspace&apos;s mail leaves through. Mail goes out through the
        enabled relays in failover order, lowest first, and falls to the next when one is down.
        A relay&apos;s password is stored encrypted and is never shown again.
      </p>
      {canEdit && rows.length > 0 ? <div className="mt-4">{add}</div> : null}

      <div className="mt-4">
        {list.isPending ? (
          <Skeleton label="Loading relays" rows={3} />
        ) : list.isError && rows.length === 0 ? (
          <ErrorState title="The relays could not be loaded." onRetry={() => void list.refetch()}>
            {list.error.message}
          </ErrorState>
        ) : rows.length === 0 ? (
          <EmptyState
            title="No relays configured"
            actions={canEdit ? add : undefined}
          >
            Until a relay is added, nothing this workspace sends can leave. A relay is the SMTP
            service you send through, such as Oracle Email Delivery or your own server.
            {canEdit ? "" : " An admin can add one."}
          </EmptyState>
        ) : (
          <>
            <TableScroller label="Relays table">
              <table className="w-full">
                <caption className="sr-only">Relays</caption>
                <thead>
                  <tr className="border-b border-border">
                    <th scope="col" className={th}>Name</th>
                    <th scope="col" className={th}>Server</th>
                    <th scope="col" className={th}>Username</th>
                    <th scope="col" className={th}>Password</th>
                    <th scope="col" className={th}>Limits</th>
                    <th scope="col" className={th}>Order</th>
                    <th scope="col" className={th}>Status</th>
                    {canEdit ? (
                      <th scope="col" className={th}>
                        <span className="sr-only">Actions</span>
                      </th>
                    ) : null}
                  </tr>
                </thead>
                <tbody>
                  {rows.map((r) => (
                    <tr key={r.id} className="border-b border-border align-top">
                      <td className="py-2 pe-3 wrap-anywhere">{r.name}</td>
                      <td className="py-2 pe-3 wrap-anywhere">
                        {r.host}:{r.port}
                        <div className="text-sm text-fg-muted">{SECURITY[r.security]}</div>
                      </td>
                      <td className="py-2 pe-3 wrap-anywhere">{r.username ?? "—"}</td>
                      <td className="py-2 pe-3">
                        {r.has_password ? (
                          <Badge variant="success" glyph="dot">Set</Badge>
                        ) : (
                          <Badge glyph="ring">Not set</Badge>
                        )}
                      </td>
                      <td className="py-2 pe-3 text-sm">
                        {r.rate_limit_per_minute} a minute
                        <div className="text-fg-muted">{r.max_connections} connections</div>
                      </td>
                      <td className="py-2 pe-3 tabular-nums">{r.failover_order}</td>
                      <td className="py-2 pe-3">
                        {r.enabled ? (
                          <Badge variant="success" glyph="dot">Enabled</Badge>
                        ) : (
                          <Badge glyph="slash">Disabled</Badge>
                        )}
                      </td>
                      {canEdit ? (
                        <td className="py-2">
                          <Button aria-label={`Edit relay ${r.name}`} onClick={() => setEditing(r)}>
                            Edit
                          </Button>
                          <Button
                            variant="danger-ghost"
                            aria-label={`Delete relay ${r.name}`}
                            onClick={() => setDeleting(r)}
                          >
                            Delete
                          </Button>
                        </td>
                      ) : null}
                    </tr>
                  ))}
                </tbody>
              </table>
            </TableScroller>
            {list.isFetchNextPageError ? (
              <p role="alert" className="mt-3">More relays could not be loaded.</p>
            ) : null}
            {list.hasNextPage ? (
              <Button className="mt-3" busy={list.isFetchingNextPage} onClick={() => void list.fetchNextPage()}>
                Load more relays
              </Button>
            ) : null}
          </>
        )}
      </div>

      <RelayDialog ws={ws} editing={editing} onClose={() => setEditing(null)} />

      <ConfirmDialog
        open={deleting !== null}
        onClose={() => {
          setDeleting(null);
          remove.reset();
        }}
        title={`Delete the relay ${deleting?.name ?? ""}`}
        confirmLabel="Delete relay"
        busy={remove.isPending}
        error={remove.error?.message}
        onConfirm={() => deleting && remove.mutate(deleting)}
      >
        <p>
          Mail will stop going out through <strong>{deleting?.name}</strong>, and its stored
          password and rate-limit state are destroyed. To pause it instead, edit it and untick
          Enabled.
        </p>
        <p>This cannot be undone.</p>
      </ConfirmDialog>
    </section>
  );
}

/**
 * The create and edit form. Its fields are uncontrolled and read from the form when it is sent,
 * so a password typed here lives only in its own input: not in component state, not in the
 * mutation's `variables`, and not in a URL. The dialog unmounts its children on close, which
 * drops the input with it.
 */
function RelayDialog({
  ws,
  editing,
  onClose,
}: {
  ws: string;
  editing: Relay | "new" | null;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const form = useRef<HTMLFormElement>(null);
  const relay = editing !== null && editing !== "new" ? editing : null;
  const [mode, setMode] = useState<PasswordMode>("keep");

  const save = useMutation({
    mutationFn: async () => {
      if (!form.current) return;
      const f = new FormData(form.current);
      const str = (k: string) => String(f.get(k) ?? "").trim();
      const password = String(f.get("password") ?? "");
      const common = {
        name: str("name"),
        host: str("host"),
        port: Number(str("port")),
        security: str("security") as Security,
        // Blank clears it on an edit, and on a create there is nothing to clear.
        username: str("username") || null,
        rate_limit_per_minute: Number(str("rate_limit_per_minute")),
        max_connections: Number(str("max_connections")),
        failover_order: Number(str("failover_order")),
        enabled: f.get("enabled") === "on",
      };
      if (relay) {
        if (mode === "replace" && password === "") {
          throw new Error("Type the new password, or choose to keep or remove the stored one.");
        }
        const { data, error } = await api.PATCH("/api/v1/workspaces/{workspace_id}/relays/{id}", {
          params: { path: { workspace_id: ws, id: relay.id } },
          // Three different bodies, on purpose: no `password` key keeps the stored one,
          // `null` removes it and a string replaces it.
          body:
            mode === "keep"
              ? common
              : { ...common, password: mode === "clear" ? null : password },
        });
        if (!data) throw new Error(problemText(error, KNOWN, "The relay could not be saved."));
      } else {
        const { data, error } = await api.POST("/api/v1/workspaces/{workspace_id}/relays", {
          params: { path: { workspace_id: ws } },
          body: password === "" ? common : { ...common, password },
        });
        if (!data) throw new Error(problemText(error, KNOWN, "The relay could not be added."));
      }
    },
    onSuccess: async () => {
      setMode("keep");
      onClose();
      await qc.invalidateQueries({ queryKey: ["relays", ws] });
    },
  });

  const close = () => {
    setMode("keep");
    save.reset();
    onClose();
  };

  return (
    <Dialog
      open={editing !== null}
      onClose={close}
      title={relay ? `Edit the relay ${relay.name}` : "Add a relay"}
    >
      <form
        ref={form}
        onSubmit={(e) => {
          e.preventDefault();
          save.mutate();
        }}
      >
        <Field id="relay-name" label="Name" name="name" required maxLength={100} defaultValue={relay?.name ?? ""} />
        <Field id="relay-host" label="Host" name="host" required autoComplete="off" defaultValue={relay?.host ?? ""} />
        <Field
          id="relay-port"
          label="Port"
          name="port"
          type="number"
          required
          min={1}
          max={65535}
          defaultValue={relay?.port ?? 587}
        />
        <div className="mt-4">
          <label className="block font-medium" htmlFor="relay-security">
            Connection security
          </label>
          <Select
            id="relay-security"
            name="security"
            className="mt-1 w-full"
            defaultValue={relay?.security ?? "starttls"}
          >
            {(Object.keys(SECURITY) as Security[]).map((s) => (
              <option key={s} value={s}>{SECURITY[s]}</option>
            ))}
          </Select>
        </div>
        <Field
          id="relay-username"
          label="Username (optional)"
          name="username"
          autoComplete="off"
          defaultValue={relay?.username ?? ""}
        />
        {relay ? (
          <div className="mt-4">
            <label className="block font-medium" htmlFor="relay-password-mode">
              Password: {relay.has_password ? "set" : "not set"}
            </label>
            <Select
              id="relay-password-mode"
              className="mt-1 w-full"
              value={mode}
              onChange={(e) => setMode(e.target.value as PasswordMode)}
            >
              <option value="keep">Keep the stored password</option>
              <option value="replace">Replace it</option>
              {relay.has_password ? <option value="clear">Remove it</option> : null}
            </Select>
          </div>
        ) : null}
        {!relay || mode === "replace" ? (
          <Field
            id="relay-password"
            label={relay ? "New password" : "Password (optional)"}
            name="password"
            type="password"
            autoComplete="new-password"
            hint="Stored encrypted. It cannot be read back, here or through the API."
          />
        ) : null}
        <Field
          id="relay-rate"
          label="Messages a minute"
          name="rate_limit_per_minute"
          type="number"
          required
          min={1}
          defaultValue={relay?.rate_limit_per_minute ?? 60}
          hint="What the relay's operator says it allows. NM keeps to it so the relay does not throttle you."
        />
        <Field
          id="relay-connections"
          label="Connections"
          name="max_connections"
          type="number"
          required
          min={1}
          defaultValue={relay?.max_connections ?? 4}
          hint="Open at once, per sender process."
        />
        <Field
          id="relay-order"
          label="Failover order"
          name="failover_order"
          type="number"
          required
          defaultValue={relay?.failover_order ?? 0}
          hint="Lower is tried first."
        />
        <label className="mt-4 flex w-fit items-center gap-2">
          <Checkbox name="enabled" defaultChecked={relay?.enabled ?? true} />
          Enabled
        </label>
        {save.error ? <Alert>{save.error.message}</Alert> : null}
        <div className="mt-6 flex flex-wrap justify-end gap-3">
          <Button onClick={close}>Cancel</Button>
          <Button type="submit" variant="solid" busy={save.isPending}>
            {relay ? "Save relay" : "Add relay"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
