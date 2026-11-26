// SPDX-License-Identifier: AGPL-3.0-only

/**
 * An in-memory `CampaignSource` for the component tests. Production code never imports this file:
 * the tests serve it over HTTP (`campaign-stub.ts`), so the screens run against the same wire as
 * the real server and the built bundle carries no mock.
 *
 * It follows contract sections 3 to 5: it enforces the state machine and answers a refusal with the
 * same problem body the server writes, so `problemText` reads both alike. Role is not its business:
 * the server is the boundary, and the screens hide what a role may not do.
 */

import type {
  Campaign,
  CampaignState,
  CampaignSummary,
  Estimate,
  Options,
} from "./campaign-model";
import type { CampaignSource, Res } from "./campaign-source";

export type MockState = {
  /** Every workspace's campaigns; each carries its own `workspace_id`. */
  campaigns: Campaign[];
  /** shortcut: one set of options for every workspace; give it per-workspace the day a test needs two. */
  options: Options;
  estimate: Estimate;
  /** What `update` answers instead of saving, e.g. a 409 `campaign-state`. */
  updateRefusal?: unknown;
  /** What `test` answers instead of queueing. */
  testRefusal?: unknown;
  /** What `schedule` answers instead of scheduling, e.g. a 409 `postal-address-required`. */
  scheduleRefusal?: unknown;
  /** Wait this long before answering, so a test can look at the screen between ask and answer. */
  delayMs: number;
};

const problem = (status: number, code: string, detail: string) => ({
  type: `urn:nm:problem:${code}`,
  title: "Error",
  status,
  detail,
});
const notFound = () => problem(404, "not-found", "resource not found");
const wrongState = (id: string, state: CampaignState, wanted: string) =>
  problem(409, "campaign-state", `campaign ${id} is ${state}; it must be ${wanted}`);

const invalid = (errors: { field: string; message: string }[]) => ({
  ...problem(422, "validation", "request validation failed"),
  errors,
});

/** A variant as the server returns it: `from_name` is always there, as `null`. */
const present = (vs: { subject: string; template_id: string; from_name?: string | null }[]) =>
  vs.map((v) => ({ ...v, from_name: v.from_name ?? null }));

const NONE = { total: 0, queued: 0, sent: 0, failed: 0, unknown: 0, skipped: 0 };

/** A fresh draft, as `POST /campaigns` makes one. Tests use it to build seeds. */
export function blankCampaign(ws: string, over: Partial<Campaign> & { name: string }): Campaign {
  const now = new Date().toISOString();
  return {
    id: crypto.randomUUID(),
    workspace_id: ws,
    state: "draft",
    list_ids: [],
    segment_id: null,
    sending_domain_id: null,
    from_local_part: null,
    from_name: null,
    reply_to: null,
    variants: [],
    ab_test_percent: null,
    winner: null,
    awaiting_winner: false,
    schedule: null,
    counts: { ...NONE },
    failure_reason: null,
    pause_reason: null,
    created_at: now,
    updated_at: now,
    launched_at: null,
    finished_at: null,
    ...over,
  };
}

const summary = (c: Campaign): CampaignSummary => ({
  id: c.id,
  name: c.name,
  state: c.state,
  send_at: c.schedule?.send_at ?? null,
  counts: c.counts,
  updated_at: c.updated_at,
});

export function mockCampaignSource(seed: Partial<MockState> = {}): CampaignSource {
  const s: MockState = {
    campaigns: [],
    options: { lists: [], segments: [], sendingDomains: [], templates: [] },
    estimate: { count: 1234, exact: true },
    delayMs: 0,
    ...seed,
  };
  // Cloned, so two sources never share a campaign and a test's seed is not mutated behind its back.
  const rows = structuredClone(s.campaigns);

  const wait = () => (s.delayMs > 0 ? new Promise((r) => setTimeout(r, s.delayMs)) : undefined);
  const ok = <T,>(data: T): Res<T> => ({ ok: true, data });
  const no = (error: unknown): Res<never> => ({ ok: false, error });
  const find = (ws: string, id: string) => rows.find((c) => c.id === id && c.workspace_id === ws);

  /** Run `f` on the workspace's campaign, or answer 404. */
  const on = async <T,>(ws: string, id: string, f: (c: Campaign) => Res<T>): Promise<Res<T>> => {
    await wait();
    const c = find(ws, id);
    return c ? f(c) : no(notFound());
  };

  /** A transition: `from` may move to `to`; already in `to` answers 200 untouched (section 3). */
  const move = (ws: string, id: string, from: CampaignState[], to: CampaignState, then?: (c: Campaign) => void) =>
    on(ws, id, (c) => {
      if (c.state !== to) {
        if (!from.includes(c.state)) return no(wrongState(id, c.state, from.join(" or ")));
        c.state = to;
        c.updated_at = new Date().toISOString();
        then?.(c);
      }
      return ok(structuredClone(c));
    });

  return {
    list: async (ws, q = {}) => {
      await wait();
      // Newest first, as the server lists. The cursor is an offset: opaque to the screen.
      const mine = rows
        .filter((c) => c.workspace_id === ws)
        .sort((a, b) => b.created_at.localeCompare(a.created_at));
      const from = Number(q.after ?? 0) || 0;
      const to = from + (q.limit ?? 50);
      return ok({
        data: mine.slice(from, to).map(summary),
        next_cursor: to < mine.length ? String(to) : null,
      });
    },
    get: (ws, id) => on(ws, id, (c) => ok(structuredClone(c))),
    create: async (ws, body) => {
      await wait();
      if (body.name.trim() === "") {
        return no({
          ...problem(422, "validation", "request validation failed"),
          errors: [{ field: "name", message: "A campaign needs a name." }],
        });
      }
      const { list_ids, variants, ...rest } = body;
      const c = blankCampaign(ws, { ...rest, ...(list_ids ? { list_ids } : {}), ...(variants ? { variants: present(variants) } : {}) });
      rows.push(c);
      return ok(structuredClone(c));
    },
    update: (ws, id, body) =>
      on(ws, id, (c) => {
        if (s.updateRefusal !== undefined) return no(s.updateRefusal);
        if (c.state !== "draft") return no(wrongState(id, c.state, "draft"));
        // The server checks each variant's template against the workspace; the mock against its options.
        const bad = (body.variants ?? []).flatMap((v, i) =>
          s.options.templates.some((t) => t.id === v.template_id)
            ? []
            : [{ field: `variants[${i}].template_id`, message: "That template does not exist." }],
        );
        if (bad.length > 0) return no(invalid(bad));
        // `null` on a key that cannot hold it is "leave alone" here; the server answers 422.
        const { name, list_ids, variants, ...rest } = body;
        Object.assign(c, rest, {
          ...(name ? { name } : {}),
          ...(list_ids ? { list_ids } : {}),
          ...(variants ? { variants: present(variants) } : {}),
          updated_at: new Date().toISOString(),
        });
        return ok(structuredClone(c));
      }),
    remove: (ws, id) =>
      on(ws, id, (c) => {
        if (c.state !== "draft") return no(wrongState(id, c.state, "draft"));
        rows.splice(rows.indexOf(c), 1);
        return ok(null);
      }),
    preview: (ws, id, body) =>
      on(ws, id, (c) => {
        const v = c.variants[body.variant ?? 0];
        const subject = v?.subject ?? "(no subject yet)";
        return ok({ subject, html: `<!doctype html><html><body><p>${subject}</p></body></html>`, text: subject });
      }),
    estimate: (ws, id) => on(ws, id, () => ok(s.estimate)),
    test: (ws, id, body) =>
      on(ws, id, (c) => {
        if (s.testRefusal !== undefined) return no(s.testRefusal);
        if (c.state !== "draft" && c.state !== "scheduled") {
          return no(wrongState(id, c.state, "draft or scheduled"));
        }
        const n = body.addresses.length;
        if (n < 1 || n > 5) {
          return no(invalid([{ field: "addresses", message: "Send a test to between 1 and 5 addresses." }]));
        }
        return ok({ queued: n });
      }),
    schedule: (ws, id, body) =>
      s.scheduleRefusal !== undefined
        ? on(ws, id, () => no(s.scheduleRefusal))
        : move(ws, id, ["draft"], "scheduled", (c) => {
            // Naive: `local` is read as UTC. The real rule (RFC 5545 DST handling) is the server's.
            c.schedule = {
              mode: body.mode,
              zone: body.zone,
              local: body.local ?? null,
              send_at: body.local == null ? new Date().toISOString() : `${body.local}:00.000Z`,
            };
          }),
    unschedule: (ws, id) => move(ws, id, ["scheduled"], "draft", (c) => (c.schedule = null)),
    pause: (ws, id) => move(ws, id, ["sending"], "paused"),
    resume: (ws, id) => move(ws, id, ["paused"], "sending"),
    cancel: (ws, id) => move(ws, id, ["scheduled", "sending", "paused"], "cancelled"),
    winner: (ws, id, body) =>
      on(ws, id, (c) => {
        const live = c.state === "sending" || c.state === "paused";
        if (!live || !c.awaiting_winner || c.winner !== null) {
          return no(wrongState(id, c.state, "sending or paused, awaiting a winner"));
        }
        c.winner = body.variant;
        c.awaiting_winner = false;
        return ok(structuredClone(c));
      }),
    options: async () => {
      await wait();
      return ok(structuredClone(s.options));
    },
  };
}

