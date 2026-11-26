// SPDX-License-Identifier: AGPL-3.0-only

import { useQuery } from "@tanstack/react-query";

import { campaignsKey } from "../../routes/campaigns";
import type { CampaignSource } from "../../routes/campaign-source";
import { Button } from "../button";
import { Problem, Section } from "./section";

/**
 * What the send did, as the server counts it: asked for when the view opens and again on Refresh,
 * never on a timer. A refusal replaces the numbers; the last good ones are not shown beside it.
 */
export function CampaignReport({
  ws,
  id,
  variantCount,
  source,
  slug,
}: {
  ws: string;
  id: string;
  variantCount: number;
  source: CampaignSource;
  slug: string;
}) {
  const report = useQuery({
    // The workspace is in the key: one workspace's report never answers for another's.
    queryKey: [...campaignsKey(ws), id, "report"],
    queryFn: async () => {
      const r = await source.report(ws, id);
      if (!r.ok) throw r.error;
      return r.data;
    },
    retry: false,
  });
  const r = report.data;
  const many = variantCount > 1;
  return (
    <Section title="Report">
      <div className="mt-4">
        {report.isError ? (
          <Problem error={report.error} slug={slug} templates={[]} />
        ) : r === undefined ? (
          <p className="text-fg-muted">Loading the report…</p>
        ) : (
          <>
            <dl className="grid grid-cols-[1fr_auto] gap-x-6 gap-y-1">
              {(
                [
                  ["Sent", r.sent],
                  ["Failed", r.failed],
                  ["Bounced", r.bounced],
                  ["Opened (approximate)", r.unique_opens],
                  ["Clicked", r.unique_clicks],
                  ["Unsubscribed", r.unsubscribed],
                ] as const
              ).map(([label, n]) => (
                <div key={label} className="contents">
                  <dt>{label}</dt>
                  <dd className="m-0 tabular-nums">{n}</dd>
                </div>
              ))}
            </dl>
            <p className="mt-2 text-fg-muted">
              Opens are approximate: some mail apps load every image, others load none.
            </p>
            {r.links.length === 0 ? (
              <p className="mt-4">No tracked links in this campaign.</p>
            ) : (
              <div className="mt-4 overflow-x-auto">
                <table className="w-full text-left">
                  <caption className="pb-2 text-left font-medium">Clicks by link</caption>
                  <thead>
                    <tr>
                      {many ? <th scope="col" className="pr-4 font-medium">Variant</th> : null}
                      <th scope="col" className="pr-4 font-medium">Link</th>
                      <th scope="col" className="pr-4 text-right font-medium">Clicks</th>
                      <th scope="col" className="text-right font-medium">Unique clicks</th>
                    </tr>
                  </thead>
                  <tbody>
                    {r.links.map((l) => (
                      <tr key={`${l.variant}-${l.link}`} className="border-t border-border">
                        {many ? <td className="py-1 pr-4 tabular-nums">{l.variant + 1}</td> : null}
                        <td className="py-1 pr-4 wrap-anywhere">{l.url}</td>
                        <td className="py-1 pr-4 text-right tabular-nums">{l.clicks}</td>
                        <td className="py-1 text-right tabular-nums">{l.unique_clicks}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </>
        )}
      </div>
      <div className="mt-4">
        <Button busy={report.isFetching} onClick={() => (report.isFetching ? undefined : void report.refetch())}>
          Refresh
        </Button>
      </div>
    </Section>
  );
}
