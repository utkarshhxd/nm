// SPDX-License-Identifier: AGPL-3.0-only

/**
 * The campaign API as the browser sees it, served from a `CampaignSource` in the test's own
 * process. The built app makes real HTTP calls through the generated client; this answers them, so
 * a component test exercises the same wire as production and the app carries no mock. Test code
 * only: nothing under `src/` that ships imports it.
 */

import type { Page } from "@playwright/test";

import type { CampaignSource, Res } from "./campaign-source";

export type Call = { method: string; path: string };

const ERROR = "application/problem+json";

/**
 * Answer every `/api/v1/workspaces/...` request of the page from `source`. `zones` gives a
 * workspace its time zone (default UTC). Returns the log of calls, in the order they arrived.
 */
export async function stubCampaignApi(
  page: Page,
  source: CampaignSource,
  zones: Record<string, string> = {},
): Promise<Call[]> {
  const log: Call[] = [];
  await page.route(/\/api\/v1\/workspaces\//, async (route) => {
    const req = route.request();
    const url = new URL(req.url());
    const [, , , , ws = "", area, id, verb] = url.pathname.split("/");
    const method = req.method();
    log.push({ method, path: url.pathname });
    const body = (): never => req.postDataJSON() as never;

    const answer = (r: Res<unknown>, status = 200) =>
      r.ok
        ? status === 204
          ? route.fulfill({ status })
          : route.fulfill({ status, json: r.data })
        : route.fulfill({
            status: (r.error as { status?: number }).status ?? 500,
            contentType: ERROR,
            body: JSON.stringify(r.error),
          });
    const page_ = (data: unknown[]) => ({ ok: true as const, data: { data, next_cursor: null } });

    if (area === undefined) {
      return route.fulfill({
        json: { id: ws, name: ws, role: "admin", slug: ws, timezone: zones[ws] ?? "UTC" },
      });
    }
    if (area === "lists" || area === "segments" || area === "sending-domains" || area === "templates") {
      const options = await source.options(ws);
      if (!options.ok) return answer(options);
      const o = options.data;
      return answer(
        page_(
          area === "lists"
            ? o.lists
            : area === "segments"
              ? o.segments
              : area === "templates"
                ? o.templates
                : o.sendingDomains.map(({ id, name }) => ({ id, domain: name })),
        ),
      );
    }
    if (area !== "campaigns") return route.fulfill({ status: 404, contentType: ERROR, body: "{}" });

    if (id === undefined) {
      if (method === "POST") return answer(await source.create(ws, body()), 201);
      const q = url.searchParams;
      const limit = q.get("limit");
      const cursor = q.get("cursor");
      return answer(
        await source.list(ws, { ...(limit ? { limit: Number(limit) } : {}), ...(cursor ? { after: cursor } : {}) }),
      );
    }
    if (verb === undefined) {
      if (method === "PATCH") return answer(await source.update(ws, id, body()));
      if (method === "DELETE") return answer(await source.remove(ws, id), 204);
      return answer(await source.get(ws, id));
    }
    switch (verb) {
      case "preview":
        return answer(await source.preview(ws, id, body()));
      case "estimate":
        return answer(await source.estimate(ws, id));
      case "test":
        return answer(await source.test(ws, id, body()), 202);
      case "schedule":
        return answer(await source.schedule(ws, id, body()));
      case "winner":
        return answer(await source.winner(ws, id, body()));
      case "unschedule":
      case "pause":
      case "resume":
      case "cancel":
        return answer(await source[verb](ws, id));
      default:
        return route.fulfill({ status: 404, contentType: ERROR, body: "{}" });
    }
  });
  return log;
}
