// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import { Button } from "../button";
import { ConfirmDialog } from "../confirm-dialog";
import type { Role } from "../../lib/roles";
import { type Campaign, actionsFor, awaitingWinner } from "../../routes/campaign-model";
import type { CampaignSource, Res } from "../../routes/campaign-source";
import { StateBadge, campaignProblem, campaignsKey, showInstant } from "../../routes/campaigns";
import { Preview } from "./preview";
import { CampaignReport } from "./report";
import { Problem, Section } from "./section";
import { TestSend } from "./test-send";

/** The counts in the order a person reads a send: what went, what did not, what is unknown, what is left. */
const COUNTS = [
  ["sent", "Sent", "bg-success"],
  ["failed", "Failed", "bg-danger"],
  ["bounced", "Bounced (no such mailbox; now suppressed)", "bg-danger opacity-60"],
  ["unknown", "Uncertain — never retried", "bg-warning"],
  ["skipped", "Skipped (unsubscribed or suppressed since launch)", "bg-fg-muted"],
  ["queued", "Queued", "bg-border-strong"],
] as const;

/**
 * A campaign that has left draft: where it is, how far it got, and the verbs its state and the
 * caller's role allow. The screen above it polls the campaign while it is sending; this shows
 * what it is handed and asks `source` to change it. The server is the boundary on every verb.
 */
export function CampaignProgress({
  campaign: c,
  source,
  ws,
  slug,
  role,
}: {
  campaign: Campaign;
  source: CampaignSource;
  ws: string;
  slug: string;
  role: Role;
}) {
  const qc = useQueryClient();
  const can = actionsFor(c.state, role, c);
  const [asking, setAsking] = useState<"resume" | "cancel" | null>(null);

  // One mutation for every verb: only one is ever in flight, and its refusal shows in one place.
  const act = useMutation({
    mutationFn: async (verb: "pause" | "resume" | "cancel" | "unschedule" | { winner: number }) => {
      const r: Res<Campaign> =
        typeof verb === "object" ? await source.winner(ws, c.id, { variant: verb.winner }) : await source[verb](ws, c.id);
      if (!r.ok) throw r.error;
      return r.data;
    },
    onSuccess: async (data) => {
      setAsking(null);
      qc.setQueryData([...campaignsKey(ws), c.id], data);
      await qc.invalidateQueries({ queryKey: campaignsKey(ws) });
    },
  });
  // A refusal belongs to the verb that was refused: opening or closing a dialog starts clean.
  const ask = (verb: "resume" | "cancel" | null) => {
    act.reset();
    setAsking(verb);
  };
  const run = (verb: Parameters<typeof act.mutate>[0]) => {
    if (!act.isPending) act.mutate(verb);
  };

  const { counts: n } = c;
  const s = c.schedule;
  const picking = awaitingWinner(c);

  return (
    <>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 className="text-xl font-semibold wrap-anywhere">{c.name}</h2>
        <StateBadge state={c.state} />
      </div>
      <p className="mt-2 text-fg-muted">
        {s?.local
          ? `Set for ${s.local.replace("T", " ")} ${s.zone}${s.mode === "recipient_local" ? ", each recipient's own local time" : ""}. `
          : ""}
        Send at {showInstant(s?.send_at ?? null)}
      </p>
      {c.pause_reason ? <p className="mt-2 wrap-anywhere">{c.pause_reason}</p> : null}
      {c.failure_reason ? <p className="mt-2 wrap-anywhere">{c.failure_reason}</p> : null}

      <Section title="Progress">
        <p className="mt-4 tabular-nums">
          {n.sent} of {n.total} sent
        </p>
        {/* The words below carry every number; the bar is a picture of them. */}
        <div aria-hidden="true" className="mt-2 flex h-3 w-full overflow-hidden rounded-sm bg-sunken">
          {COUNTS.map(([key, , colour]) => (
            <div key={key} className={colour} style={{ width: `${n.total > 0 ? (n[key] / n.total) * 100 : 0}%` }} />
          ))}
        </div>
        <dl className="mt-4 grid grid-cols-[1fr_auto] gap-x-6 gap-y-1">
          {COUNTS.map(([key, label]) => (
            <div key={key} className="contents">
              {/* A cancelled campaign's queue is never claimed, so "Queued" would read as still going out. */}
              <dt className="wrap-anywhere">{key === "queued" && c.state === "cancelled" ? "Not sent (cancelled)" : label}</dt>
              <dd className="m-0 tabular-nums">{n[key]}</dd>
            </div>
          ))}
        </dl>
      </Section>

      {picking ? (
        <Section title="Pick the winner">
          <p className="mt-4">Each variant&apos;s link clicks are in the report below.</p>
          <ul className="mt-4 space-y-3">
            {c.variants.map((v, i) => (
              <li key={i} className="flex flex-wrap items-center justify-between gap-3">
                <span className="wrap-anywhere">
                  <strong>Variant {i + 1}</strong>: {v.subject}
                </span>
                {can.includes("winner") ? (
                  <Button variant="solid" busy={act.isPending} onClick={() => run({ winner: i })}>
                    {`Pick variant ${i + 1}`}
                  </Button>
                ) : null}
              </li>
            ))}
          </ul>
        </Section>
      ) : c.winner !== null ? (
        <p className="mt-4">Winner: variant {c.winner + 1}.</p>
      ) : null}

      <div className="mt-6 flex flex-wrap gap-3">
        {can.includes("pause") ? (
          <Button busy={act.isPending} onClick={() => run("pause")}>
            Pause
          </Button>
        ) : null}
        {can.includes("resume") ? (
          <Button
            variant="solid"
            busy={act.isPending}
            onClick={() => (s?.mode === "recipient_local" ? ask("resume") : run("resume"))}
          >
            Resume
          </Button>
        ) : null}
        {can.includes("unschedule") ? (
          <Button busy={act.isPending} onClick={() => run("unschedule")}>
            Unschedule
          </Button>
        ) : null}
        {can.includes("cancel") ? <Button onClick={() => ask("cancel")}>Cancel campaign</Button> : null}
      </div>
      {act.isError && asking === null ? <Problem error={act.error} slug={slug} templates={[]} /> : null}

      <ConfirmDialog
        open={asking === "resume"}
        onClose={() => ask(null)}
        title="Resume campaign"
        confirmLabel="Resume sending"
        variant="solid"
        busy={act.isPending}
        error={act.isError ? campaignProblem(act.error, "The campaign could not be resumed.") : undefined}
        onConfirm={() => run("resume")}
      >
        <p>
          This campaign sends at each recipient&apos;s own local time. Recipients whose time has already
          passed are sent the email at once.
        </p>
      </ConfirmDialog>
      <ConfirmDialog
        open={asking === "cancel"}
        onClose={() => ask(null)}
        title="Cancel campaign"
        confirmLabel="Cancel campaign"
        dismissLabel="Keep campaign"
        busy={act.isPending}
        error={act.isError ? campaignProblem(act.error, "The campaign could not be cancelled.") : undefined}
        onConfirm={() => run("cancel")}
      >
        <p>Nothing more will be sent. Emails already sent stay sent. This cannot be undone.</p>
      </ConfirmDialog>

      {/* Below the picker and the verbs, so the picker's "report below" is true. */}
      {c.launched_at !== null ? (
        <CampaignReport
          ws={ws}
          id={c.id}
          state={c.state}
          variantCount={c.variants.length}
          source={source}
          slug={slug}
        />
      ) : null}

      {can.includes("test") ? (
        <Section title="Test send">
          <TestSend ws={ws} id={c.id} variantCount={c.variants.length} slug={slug} templates={[]} />
        </Section>
      ) : null}

      <Section title="Preview">
        <Preview ws={ws} id={c.id} variantCount={c.variants.length} savedAt={c.updated_at} />
      </Section>
    </>
  );
}
