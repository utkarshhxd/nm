// SPDX-License-Identifier: AGPL-3.0-only

import { useInfiniteQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";

import { problemDetail, problemMessage, problemType } from "../api/client";
import { Badge, type BadgeGlyph, type BadgeVariant } from "../components/badge";
import { Button } from "../components/button";
import { Dialog } from "../components/dialog";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Alert, Field } from "../components/field";
import { Skeleton } from "../components/skeleton";
import { TableScroller } from "../components/table-scroller";
import { atLeast, type Role } from "../lib/roles";
import { type CampaignState, type CampaignSummary, actionsFor } from "./campaign-model";
import { campaignSource } from "./campaign-source";
import { useWorkspace } from "./workspace-layout";

const link = "text-accent-text underline";
const th = "py-2 pe-3 text-start font-medium";

/** Every cache entry on these screens is keyed by the workspace: a list of one tenant is never another's. */
export const campaignsKey = (ws: string) => ["campaigns", ws] as const;

const STATE: Record<CampaignState, [string, BadgeVariant, BadgeGlyph]> = {
  draft: ["Draft", "neutral", "ring"],
  scheduled: ["Scheduled", "info", "ring"],
  sending: ["Sending", "accent", "dot"],
  paused: ["Paused", "warning", "slash"],
  sent: ["Sent", "success", "dot"],
  cancelled: ["Cancelled", "neutral", "slash"],
  failed: ["Failed", "danger", "cross"],
};

export const stateName = (s: CampaignState) => STATE[s][0];

export function StateBadge({ state }: { state: CampaignState }) {
  const [name, variant, glyph] = STATE[state];
  return (
    <Badge variant={variant} glyph={glyph}>
      {name}
    </Badge>
  );
}

// UTC and said so: an instant shown in whatever zone the browser has would read differently on
// two screens, and the schedule's zone is the campaign's, not the viewer's.
const instant = new Intl.DateTimeFormat("en-GB", {
  dateStyle: "medium",
  timeStyle: "short",
  timeZone: "UTC",
});
export const showInstant = (iso: string | null) => (iso ? `${instant.format(new Date(iso))} UTC` : "—");

const CAMPAIGN_PROBLEMS: Record<string, string> = {
  "insufficient-role": "Your role cannot change campaigns. It takes editor or above.",
  "not-found": "This campaign no longer exists. It may have been deleted in another window.",
};

/**
 * The sentence for a refusal. The server's own `detail` wins when it says something about this
 * request; when it is only the status in words ("resource not found", "insufficient role") the
 * screen's sentence is the fallback, because it says what to do.
 */
export function campaignProblem(error: unknown, fallback: string): string {
  const code = problemType(error).replace("urn:nm:problem:", "");
  const detail = problemDetail(error, "").toLowerCase();
  const plain = detail === "" || detail === code.replaceAll("-", " ") || detail === "resource not found";
  return (plain ? CAMPAIGN_PROBLEMS[code] : undefined) ?? problemMessage(error, fallback);
}

const PAGE = 50;

export default function Campaigns() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return <AllCampaigns ws={workspace.id} slug={workspace.slug} role={workspace.role} />;
}

function AllCampaigns({ ws, slug, role }: { ws: string; slug: string; role: Role }) {
  const [creating, setCreating] = useState(false);
  const canCreate = atLeast(role, "editor");

  const campaigns = useInfiniteQuery({
    queryKey: campaignsKey(ws),
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) => {
      const r = await campaignSource.list(ws, { limit: PAGE, ...(pageParam ? { after: pageParam } : {}) });
      if (!r.ok) {
        throw new Error(
          campaignProblem(r.error, "The campaigns could not be loaded."),
        );
      }
      return r.data;
    },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    retry: false,
  });
  const rows = campaigns.data?.pages.flatMap((p) => p.data) ?? [];

  return (
    <section className="mt-8">
      <div className="flex flex-wrap items-baseline justify-between gap-3">
        <h2 className="text-xl font-semibold">Campaigns</h2>
        {canCreate ? (
          <Button variant="solid" onClick={() => setCreating(true)}>
            New campaign
          </Button>
        ) : null}
      </div>
      <div className="mt-4">
        {campaigns.isPending ? (
          <Skeleton label="Loading campaigns" rows={5} />
        ) : campaigns.isError ? (
          <ErrorState
            title="The campaigns could not be loaded."
            onRetry={() => void campaigns.refetch()}
          >
            {campaigns.error.message}
          </ErrorState>
        ) : rows.length === 0 ? (
          <EmptyState title="No campaigns yet">
            A campaign sends one email, or a split test of up to four, to the lists and segment you
            choose.
          </EmptyState>
        ) : (
          <>
            <TableScroller label="Campaigns table">
              <table className="w-full">
                <caption className="sr-only">Campaigns</caption>
                <thead>
                  <tr className="border-b border-border">
                    <th scope="col" className={th}>
                      Name
                    </th>
                    <th scope="col" className={th}>
                      State
                    </th>
                    <th scope="col" className={th}>
                      Send at
                    </th>
                    <th scope="col" className={th}>
                      Sent
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((c) => (
                    <Row key={c.id} c={c} slug={slug} role={role} />
                  ))}
                </tbody>
              </table>
            </TableScroller>
            {campaigns.hasNextPage ? (
              <div className="mt-4">
                <Button busy={campaigns.isFetchingNextPage} onClick={() => void campaigns.fetchNextPage()}>
                  Load more
                </Button>
              </div>
            ) : null}
          </>
        )}
      </div>

      <Dialog open={creating} onClose={() => setCreating(false)} title="New campaign">
        <NewCampaignForm ws={ws} slug={slug} onCancel={() => setCreating(false)} />
      </Dialog>
    </section>
  );
}

function Row({ c, slug, role }: { c: CampaignSummary; slug: string; role: Role }) {
  // Only the table's `send` is asked here; the confirmation itself lives on the campaign's screen.
  const canSend = actionsFor(c.state, role, { awaiting_winner: false, winner: null }).includes("send");
  const to = { slug, campaignId: c.id };
  return (
    <tr className="border-b border-border">
      <td className="py-2 pe-3 wrap-anywhere">
        <Link to="/w/$slug/campaigns/$campaignId" params={to} className={link}>
          {c.name}
        </Link>
        {canSend ? (
          <Link
            to="/w/$slug/campaigns/$campaignId"
            params={to}
            className={`${link} ms-3`}
            aria-label={`Send ${c.name}`}
          >
            Send
          </Link>
        ) : null}
      </td>
      <td className="py-2 pe-3">
        <StateBadge state={c.state} />
      </td>
      <td className="py-2 pe-3 tabular-nums text-fg-muted">{showInstant(c.send_at)}</td>
      <td className="py-2 pe-3 tabular-nums">
        {c.counts.sent} / {c.counts.total}
      </td>
    </tr>
  );
}

/** The create form: a draft needs only a name; everything else is filled in on its own screen. */
function NewCampaignForm({ ws, slug, onCancel }: { ws: string; slug: string; onCancel: () => void }) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [name, setName] = useState("");

  const create = useMutation({
    mutationFn: async () => {
      const r = await campaignSource.create(ws, { name });
      if (!r.ok) {
        throw new Error(
          campaignProblem(r.error, "The campaign could not be created."),
        );
      }
      return r.data;
    },
    onSuccess: async (campaign) => {
      await qc.invalidateQueries({ queryKey: campaignsKey(ws) });
      await navigate({
        to: "/w/$slug/campaigns/$campaignId",
        params: { slug, campaignId: campaign.id },
      });
    },
  });

  return (
    <form
      className="mt-3"
      onSubmit={(e: FormEvent) => {
        e.preventDefault();
        // `busy` does not disable the button, so a second press must be ignored here.
        if (!create.isPending) create.mutate();
      }}
    >
      <Field
        id="campaign-name"
        label="Name"
        type="text"
        autoComplete="off"
        value={name}
        onChange={(e) => setName(e.target.value)}
      />
      {create.error ? <Alert>{create.error.message}</Alert> : null}
      <div className="mt-6 flex flex-wrap justify-end gap-3">
        <Button onClick={onCancel}>Cancel</Button>
        <Button type="submit" variant="solid" busy={create.isPending}>
          Create campaign
        </Button>
      </div>
    </form>
  );
}
