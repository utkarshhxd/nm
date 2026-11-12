// SPDX-License-Identifier: AGPL-3.0-only

import { useInfiniteQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";

import { api, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Badge } from "../components/badge";
import { Button } from "../components/button";
import { Checkbox } from "../components/checkbox";
import { ConfirmDialog } from "../components/confirm-dialog";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Alert, Field } from "../components/field";
import { CONTROL } from "../components/input";
import { Select } from "../components/select";
import { Skeleton } from "../components/skeleton";
import { TableScroller } from "../components/table-scroller";
import { atLeast } from "../lib/roles";
import { useMe } from "../lib/session";
import { REASON } from "../lib/suppressions";
import { useWorkspace } from "./workspace-layout";

type Suppression = components["schemas"]["Suppression"];
type Reason = components["schemas"]["Reason"];
type Scope = components["schemas"]["Scope"];

/** What a person may add: an erasure is only ever written by erasing a contact. */
const ADDABLE = (Object.keys(REASON) as Reason[]).filter((r) => r !== "gdpr_erasure");

const day = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });
const th = "py-2 pe-3 text-start font-medium";
const PAGE = 100;
const KNOWN = {
  "insufficient-role": "Your role cannot do that. Adding takes editor; lifting takes admin.",
  "suppression-exists": "That address or domain is already blocked in this workspace.",
  "erasure-not-liftable":
    "An erasure cannot be lifted, by anyone. The address was erased on request and can never be mailed again.",
  "suppression-already-lifted": "This suppression was already lifted.",
  "instance-admin-required":
    "Only an instance admin can see or change instance-wide suppressions. A workspace admin is not one.",
};

/** What the row blocks, in words: an erasure tombstone has no address left to show. */
const target = (s: Suppression) =>
  s.scope === "domain" ? `${s.domain ?? ""} (every address on this domain)` : (s.email ?? "An erased address");

export default function Suppressions() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return (
    <Page
      ws={workspace.id}
      canAdd={atLeast(workspace.role, "editor")}
      canLift={atLeast(workspace.role, "admin")}
    />
  );
}

/**
 * The instance-wide list, at `/admin/suppressions`: the same screen against
 * `/api/v1/admin/suppressions`, which blocks every workspace at once.
 *
 * Gated on `is_instance_admin`, which a workspace admin is not — these rows are not any one
 * workspace's to see or lift, so a workspace admin reaching this URL is told so rather than
 * shown an empty table. The gate is a courtesy: the routes answer 403 either way.
 *
 * It lives outside the workspace layout, so it brings its own page chrome.
 */
export function InstanceSuppressions() {
  const me = useMe();
  return (
    <main className="mx-auto max-w-4xl p-8">
      <div className="flex flex-wrap items-baseline justify-between gap-3">
        <h1 className="text-2xl font-semibold">NM</h1>
        {me.data ? <p>Signed in as {me.data.email}</p> : null}
      </div>
      <p className="mt-4">
        <Link className="text-accent-text underline" to="/">
          Back to your workspaces
        </Link>
      </p>
      {!me.data ? (
        <p className="mt-4">Loading…</p>
      ) : !me.data.is_instance_admin ? (
        <p className="mt-4">
          Only an instance admin can see instance-wide suppressions. Being an admin of a workspace
          is not the same thing.
        </p>
      ) : (
        <Page ws={null} canAdd canLift />
      )}
    </main>
  );
}

/** `ws` is the workspace, or `null` for the instance-wide list. */
function Page({ ws, canAdd, canLift }: { ws: string | null; canAdd: boolean; canLift: boolean }) {
  const qc = useQueryClient();
  const [showLifted, setShowLifted] = useState(false);
  const list = useInfiniteQuery({
    queryKey: ["suppressions", ws ?? "instance", showLifted],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) => {
      const query = { limit: PAGE, cursor: pageParam, include_lifted: showLifted };
      const { data, error } = ws
        ? await api.GET("/api/v1/workspaces/{workspace_id}/suppressions", {
            params: { path: { workspace_id: ws }, query },
          })
        : await api.GET("/api/v1/admin/suppressions", { params: { query } });
      if (!data) throw new Error(problemText(error, KNOWN, "The suppressions could not be loaded."));
      return data;
    },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    retry: false,
  });
  const rows = list.data?.pages.flatMap((p) => p.data) ?? [];

  /** A change here changes whether a contact may be mailed, so those answers are stale too. */
  const refresh = async () => {
    await Promise.all(
      ["suppressions", "contact-eligibility", "mailable-count"].map((k) =>
        qc.invalidateQueries({ queryKey: [k] }),
      ),
    );
  };

  const [scope, setScope] = useState<Scope>("address");
  const [value, setValue] = useState("");
  const [reason, setReason] = useState<Reason>("manual");
  const [note, setNote] = useState("");
  const add = useMutation({
    mutationFn: async () => {
      const body = {
        scope,
        reason,
        ...(scope === "address" ? { email: value.trim() } : { domain: value.trim() }),
        ...(note.trim() ? { note: note.trim() } : {}),
      };
      const { data, error } = ws
        ? await api.POST("/api/v1/workspaces/{workspace_id}/suppressions", {
            params: { path: { workspace_id: ws } },
            body,
          })
        : await api.POST("/api/v1/admin/suppressions", { body });
      if (!data) throw new Error(problemText(error, KNOWN, "The suppression could not be added."));
    },
    onSuccess: async () => {
      setValue("");
      setNote("");
      await refresh();
    },
  });
  function submit(event: FormEvent) {
    event.preventDefault();
    add.mutate();
  }

  const [lifting, setLifting] = useState<Suppression | null>(null);
  const lift = useMutation({
    mutationFn: async (s: Suppression) => {
      const { data, error } = ws
        ? await api.POST("/api/v1/workspaces/{workspace_id}/suppressions/{id}/lift", {
            params: { path: { workspace_id: ws, id: s.id } },
          })
        : await api.POST("/api/v1/admin/suppressions/{id}/lift", { params: { path: { id: s.id } } });
      if (!data) throw new Error(problemText(error, KNOWN, "The suppression could not be lifted."));
    },
    onSuccess: async () => {
      setLifting(null);
      await refresh();
    },
  });

  return (
    <section className="mt-8">
      <h2 className="text-xl font-semibold">{ws ? "Suppressions" : "Instance suppressions"}</h2>
      <p className="mt-2 text-fg-muted">
        {ws
          ? "Addresses and domains this workspace will not mail. Instance-wide blocks are not shown here and a workspace admin cannot see them, so an empty list does not mean an address can be mailed. To check one contact, open its Mailing status."
          : "Addresses and domains no workspace on this instance will mail. They block in every workspace, and the people who run those workspaces can neither see nor lift them here."}
      </p>

      {canAdd ? (
        <form onSubmit={submit} className="mt-4 max-w-md">
          <div className="mt-4">
            <label className="block font-medium" htmlFor="sup-scope">
              Block
            </label>
            <Select
              id="sup-scope"
              className="mt-1 w-full"
              value={scope}
              onChange={(e) => setScope(e.target.value as Scope)}
            >
              <option value="address">One address</option>
              <option value="domain">A whole domain</option>
            </Select>
          </div>
          <Field
            id="sup-value"
            label={scope === "address" ? "Email address" : "Domain"}
            value={value}
            autoComplete="off"
            onChange={(e) => setValue(e.target.value)}
            hint={
              scope === "domain"
                ? "That exact domain only. Subdomains are not covered: spam.example does not block mail.spam.example."
                : undefined
            }
          />
          <div className="mt-4">
            <label className="block font-medium" htmlFor="sup-reason">
              Reason
            </label>
            <Select
              id="sup-reason"
              className="mt-1 w-full"
              value={reason}
              onChange={(e) => setReason(e.target.value as Reason)}
            >
              {ADDABLE.map((r) => (
                <option key={r} value={r}>
                  {REASON[r]}
                </option>
              ))}
            </Select>
          </div>
          <div className="mt-4">
            <label className="block font-medium" htmlFor="sup-note">
              Note (optional)
            </label>
            <textarea
              id="sup-note"
              rows={2}
              maxLength={500}
              className={`${CONTROL} mt-1 w-full py-2`}
              value={note}
              onChange={(e) => setNote(e.target.value)}
            />
          </div>
          {add.error ? <Alert>{add.error.message}</Alert> : null}
          <Button type="submit" variant="solid" className="mt-4" busy={add.isPending}>
            Add suppression
          </Button>
        </form>
      ) : null}

      <label className="mt-8 flex w-fit items-center gap-2">
        <Checkbox checked={showLifted} onChange={(e) => setShowLifted(e.target.checked)} />
        Show lifted
      </label>

      <div className="mt-4">
        {list.isPending ? (
          <Skeleton label="Loading suppressions" rows={5} />
        ) : list.isError && rows.length === 0 ? (
          <ErrorState title="The suppressions could not be loaded." onRetry={() => void list.refetch()}>
            {list.error.message}
          </ErrorState>
        ) : rows.length === 0 ? (
          <EmptyState
            title={ws ? "No suppressions in this workspace" : "No instance-wide suppressions"}
          >
            {ws
              ? "Instance-wide blocks are not listed here."
              : "Each workspace keeps its own list as well, which is not listed here."}
          </EmptyState>
        ) : (
          <>
            <TableScroller label="Suppressions table"><table className="w-full">
              <caption className="sr-only">Suppressions</caption>
              <thead>
                <tr className="border-b border-border">
                  <th scope="col" className={th}>
                    Blocks
                  </th>
                  <th scope="col" className={th}>
                    Reason
                  </th>
                  <th scope="col" className={th}>
                    Note
                  </th>
                  <th scope="col" className={th}>
                    Added
                  </th>
                  <th scope="col" className={th}>
                    Status
                  </th>
                </tr>
              </thead>
              <tbody>
                {rows.map((s) => (
                  <tr key={s.id} className="border-b border-border align-top">
                    <td className="py-2 pe-3 break-all">{target(s)}</td>
                    <td className="py-2 pe-3">{REASON[s.reason]}</td>
                    <td className="py-2 pe-3 whitespace-pre-line wrap-anywhere">{s.note ?? "—"}</td>
                    <td className="py-2 pe-3 tabular-nums text-fg-muted">
                      {day.format(new Date(s.created_at))}
                    </td>
                    <td className="py-2">
                      {s.lifted_at ? (
                        <Badge glyph="ring">Lifted {day.format(new Date(s.lifted_at))}</Badge>
                      ) : s.reason === "gdpr_erasure" ? (
                        // No control at all: no route lifts an erasure, for any role.
                        <span className="text-fg-muted">In force. An erasure cannot be lifted.</span>
                      ) : (
                        <>
                          <Badge variant="danger" glyph="cross">
                            In force
                          </Badge>
                          {canLift ? (
                            <Button
                              variant="danger-ghost"
                              aria-label={`Lift suppression for ${s.email ?? s.domain ?? "this address"}`}
                              onClick={() => setLifting(s)}
                            >
                              Lift
                            </Button>
                          ) : null}
                        </>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table></TableScroller>
            {list.isFetchNextPageError ? (
              <p role="alert" className="mt-3">
                More suppressions could not be loaded.
              </p>
            ) : null}
            {list.hasNextPage ? (
              <Button
                className="mt-3"
                busy={list.isFetchingNextPage}
                onClick={() => void list.fetchNextPage()}
              >
                Load more suppressions
              </Button>
            ) : null}
          </>
        )}
      </div>

      <ConfirmDialog
        open={lifting !== null}
        onClose={() => {
          setLifting(null);
          lift.reset();
        }}
        title={`Lift the suppression on ${lifting?.email ?? lifting?.domain ?? ""}`}
        confirmLabel="Lift suppression"
        busy={lift.isPending}
        error={lift.error?.message}
        onConfirm={() => lifting && lift.mutate(lifting)}
      >
        {lifting ? (
          <>
            <p>
              <strong>{lifting.email ?? lifting.domain}</strong> is blocked because of:{" "}
              <strong>{REASON[lifting.reason]}</strong> (added {day.format(new Date(lifting.created_at))}
              {lifting.note ? `, note: “${lifting.note}”` : ""}).
            </p>
            <p>
              Lifting it makes{" "}
              {lifting.scope === "domain" ? "every address on this domain" : "this address"}{" "}
              mailable again{" "}
              {ws
                ? "in this workspace, unless it is also blocked instance-wide, which a workspace admin cannot see or lift here"
                : "in every workspace on this instance, each of which may still block it in its own list"}
              . Someone who opted out, whose mail bounced or who complained did so for a reason:
              mailing them again can damage your sending reputation and ignores what they asked
              for. Lift it only if the block was a mistake or you have their fresh consent.
            </p>
            <p>The row stays as history and the lift is recorded in the audit log.</p>
          </>
        ) : null}
      </ConfirmDialog>
    </section>
  );
}
