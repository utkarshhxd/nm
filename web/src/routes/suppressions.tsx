// SPDX-License-Identifier: AGPL-3.0-only

import { useInfiniteQuery, useMutation, useQueryClient } from "@tanstack/react-query";
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
import { useWorkspace } from "./workspace-layout";

type Suppression = components["schemas"]["Suppression"];
type Reason = components["schemas"]["Reason"];
type Scope = components["schemas"]["Scope"];

const REASON: Record<Reason, string> = {
  unsubscribed_all: "Unsubscribed from everything",
  bounced: "Bounced",
  complained: "Complained",
  manual: "Blocked manually",
  gdpr_erasure: "Erased on request (GDPR)",
};
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

function Page({ ws, canAdd, canLift }: { ws: string; canAdd: boolean; canLift: boolean }) {
  const qc = useQueryClient();
  const [showLifted, setShowLifted] = useState(false);
  const list = useInfiniteQuery({
    queryKey: ["suppressions", ws, showLifted],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/suppressions", {
        params: {
          path: { workspace_id: ws },
          query: { limit: PAGE, cursor: pageParam, include_lifted: showLifted },
        },
      });
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
      const { data, error } = await api.POST("/api/v1/workspaces/{workspace_id}/suppressions", {
        params: { path: { workspace_id: ws } },
        body: {
          scope,
          reason,
          ...(scope === "address" ? { email: value.trim() } : { domain: value.trim() }),
          ...(note.trim() ? { note: note.trim() } : {}),
        },
      });
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
      const { data, error } = await api.POST(
        "/api/v1/workspaces/{workspace_id}/suppressions/{id}/lift",
        { params: { path: { workspace_id: ws, id: s.id } } },
      );
      if (!data) throw new Error(problemText(error, KNOWN, "The suppression could not be lifted."));
    },
    onSuccess: async () => {
      setLifting(null);
      await refresh();
    },
  });

  return (
    <section className="mt-8">
      <h2 className="text-xl font-semibold">Suppressions</h2>
      <p className="mt-2 text-fg-muted">
        Addresses and domains this workspace will not mail. Instance-wide blocks are not shown here
        and a workspace admin cannot see them, so an empty list does not mean an address can be
        mailed. To check one contact, open its Mailing status.
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
          <EmptyState title="No suppressions in this workspace">
            Instance-wide blocks are not listed here.
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
              Lifting it makes {lifting.scope === "domain" ? "every address on this domain" : "this address"}{" "}
              mailable again in this workspace, unless it is also blocked instance-wide, which a workspace admin cannot see or lift here. Someone who opted out, whose mail bounced or who
              complained did so for a reason: mailing them again can damage your sending reputation
              and ignores what they asked for. Lift it only if the block was a mistake or you have
              their fresh consent.
            </p>
            <p>The row stays as history and the lift is recorded in the audit log.</p>
          </>
        ) : null}
      </ConfirmDialog>
    </section>
  );
}
