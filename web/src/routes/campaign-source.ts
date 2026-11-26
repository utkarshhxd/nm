// SPDX-License-Identifier: AGPL-3.0-only

/**
 * THE campaign screens' one view of the server: the `CampaignSource` type and the `campaignSource`
 * the screens call, which is the generated client. Nothing else on those screens knows an API path.
 *
 * Every method takes the workspace first. Another workspace's campaign is a 404 on every route,
 * never a 403 and never the data. A refusal comes back as `{ ok: false, error }` with the problem
 * body the server wrote, which `problemText` reads.
 *
 * The in-memory implementation the component tests serve over HTTP is `campaign-mock.ts`; nothing
 * here imports it, so it is not in the production bundle.
 */

import { api } from "../api/client";
import type {
  Campaign,
  CampaignPreview,
  CampaignSummary,
  Estimate,
  NewCampaign,
  Options,
  Page,
  Present,
  PreviewRequest,
  Report,
  ScheduleRequest,
  TestSendRequest,
  TestSendResult,
  UpdateCampaign,
  WinnerRequest,
} from "./campaign-model";

/** A call's answer. A refusal carries the problem body as the server would write it. */
export type Res<T> = { ok: true; data: T } | { ok: false; error: unknown };

export type CampaignSource = {
  list: (ws: string, q?: { after?: string; limit?: number }) => Promise<Res<Page<CampaignSummary>>>;
  get: (ws: string, id: string) => Promise<Res<Campaign>>;
  create: (ws: string, body: NewCampaign) => Promise<Res<Campaign>>;
  update: (ws: string, id: string, body: UpdateCampaign) => Promise<Res<Campaign>>;
  remove: (ws: string, id: string) => Promise<Res<null>>;
  preview: (ws: string, id: string, body: PreviewRequest) => Promise<Res<CampaignPreview>>;
  estimate: (ws: string, id: string) => Promise<Res<Estimate>>;
  test: (ws: string, id: string, body: TestSendRequest) => Promise<Res<TestSendResult>>;
  schedule: (ws: string, id: string, body: ScheduleRequest) => Promise<Res<Campaign>>;
  unschedule: (ws: string, id: string) => Promise<Res<Campaign>>;
  pause: (ws: string, id: string) => Promise<Res<Campaign>>;
  resume: (ws: string, id: string) => Promise<Res<Campaign>>;
  cancel: (ws: string, id: string) => Promise<Res<Campaign>>;
  winner: (ws: string, id: string, body: WinnerRequest) => Promise<Res<Campaign>>;
  report: (ws: string, id: string) => Promise<Res<Report>>;
  options: (ws: string) => Promise<Res<Options>>;
};

/**
 * What `openapi-fetch` answers, as a `Res`. The cast is the one place a generated response becomes
 * the screens' type (`Present`: a nullable key is always sent, as `null`).
 */
async function res<W>(
  call: Promise<{ data?: W; error?: unknown; response: Response }>,
): Promise<Res<Present<W>>> {
  const { data, error, response } = await call;
  return response.ok ? { ok: true, data: data as unknown as Present<W> } : { ok: false, error };
}

const C = "/api/v1/workspaces/{workspace_id}/campaigns/{id}" as const;
const at = (ws: string, id: string) => ({ params: { path: { workspace_id: ws, id } } });

// shortcut: the first 200 of each pick-list, the largest page the server allows; a workspace with
// more lists, segments or templates than that needs a searchable picker, not a longer list.
const FIRST = { limit: 200 };

export const campaignSource: CampaignSource = {
  list: (ws, q = {}) =>
    res(
      api.GET("/api/v1/workspaces/{workspace_id}/campaigns", {
        params: {
          path: { workspace_id: ws },
          query: { ...(q.limit ? { limit: q.limit } : {}), ...(q.after ? { cursor: q.after } : {}) },
        },
      }),
    ),
  get: (ws, id) => res(api.GET(C, at(ws, id))),
  create: (ws, body) =>
    res(api.POST("/api/v1/workspaces/{workspace_id}/campaigns", { params: { path: { workspace_id: ws } }, body })),
  update: (ws, id, body) => res(api.PATCH(C, { ...at(ws, id), body })),
  remove: async (ws, id) => {
    const r = await res(api.DELETE(C, at(ws, id)));
    return r.ok ? { ok: true, data: null } : r;
  },
  preview: (ws, id, body) => res(api.POST(`${C}/preview`, { ...at(ws, id), body })),
  estimate: (ws, id) => res(api.POST(`${C}/estimate`, at(ws, id))),
  test: (ws, id, body) => res(api.POST(`${C}/test`, { ...at(ws, id), body })),
  schedule: (ws, id, body) => res(api.POST(`${C}/schedule`, { ...at(ws, id), body })),
  unschedule: (ws, id) => res(api.POST(`${C}/unschedule`, at(ws, id))),
  pause: (ws, id) => res(api.POST(`${C}/pause`, at(ws, id))),
  resume: (ws, id) => res(api.POST(`${C}/resume`, at(ws, id))),
  cancel: (ws, id) => res(api.POST(`${C}/cancel`, at(ws, id))),
  winner: (ws, id, body) => res(api.POST(`${C}/winner`, { ...at(ws, id), body })),
  report: (ws, id) => res(api.GET(`${C}/report`, at(ws, id))),
  options: async (ws) => {
    const params = { params: { path: { workspace_id: ws }, query: FIRST } };
    const [lists, segments, domains, templates] = await Promise.all([
      res(api.GET("/api/v1/workspaces/{workspace_id}/lists", params)),
      res(api.GET("/api/v1/workspaces/{workspace_id}/segments", params)),
      res(api.GET("/api/v1/workspaces/{workspace_id}/sending-domains", params)),
      res(api.GET("/api/v1/workspaces/{workspace_id}/templates", params)),
    ]);
    if (!lists.ok) return lists;
    if (!segments.ok) return segments;
    if (!domains.ok) return domains;
    if (!templates.ok) return templates;
    return {
      ok: true,
      data: {
        lists: lists.data.data.map(({ id, name }) => ({ id, name })),
        segments: segments.data.data.map(({ id, name }) => ({ id, name })),
        sendingDomains: domains.data.data.map((d) => ({ id: d.id, name: d.domain })),
        templates: templates.data.data,
      },
    };
  },
};
