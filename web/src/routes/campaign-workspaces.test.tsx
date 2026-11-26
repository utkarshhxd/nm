// SPDX-License-Identifier: AGPL-3.0-only

// Two workspaces, ONE campaign id, answered over stubbed HTTP. Every cache entry on the campaign
// screens (list, detail, zone, options, estimate, preview) must be keyed by the workspace: a key
// without it would show workspace A's answer in workspace B for as long as B's own is in flight.
//
// B's answers are held until the test lets each go, so a stale entry has nothing to be replaced by
// and stays on screen: the assertions below then keep failing instead of passing once the real
// answer lands (a `toHaveCount(0)` that retries would otherwise hide exactly this).

import { type Page, expect } from "@playwright/test";

import { test } from "../../e2e/support/app";
import type { components } from "../api/schema";
import { blankCampaign } from "./campaign-mock";

type Me = components["schemas"]["MeResponse"];

const ACME = "11111111-1111-4111-8111-111111111111";
const BETA = "33333333-3333-4333-8333-333333333333";
const ID = "99999999-9999-4999-8999-999999999999";
const LIST = "bbbbbbbb-0000-4000-8000-000000000001";
const DOMAIN = "dddddddd-0000-4000-8000-000000000001";
const T1 = "aaaaaaaa-0000-4000-8000-000000000001";
const SAME_TIME = "2026-02-02T10:00:00Z";

/** What each workspace says, under the same ids. */
const WS = {
  [ACME]: { name: "Acme", campaign: "Acme spring", list: "Acme list", count: 111, subject: "Acme subject", zone: "Asia/Tokyo" },
  [BETA]: { name: "Beta", campaign: "Beta spring", list: "Beta list", count: 222, subject: "Beta subject", zone: "Europe/Paris" },
} as const;

type Kind = "list" | "detail" | "zone" | "options" | "estimate" | "preview";

function serve(page: Page) {
  const held = new Map<Kind, { open: Promise<void>; release: () => void }>();
  const arrived = new Map<Kind, { seen: Promise<void>; mark: () => void }>();
  const gate = <T,>(m: Map<Kind, T>, k: Kind, make: () => T) => {
    if (!m.has(k)) m.set(k, make());
    return m.get(k)!;
  };
  const gateOf = (k: Kind) =>
    gate(held, k, () => {
      let release!: () => void;
      const open = new Promise<void>((r) => (release = r));
      return { open, release };
    });
  const seenOf = (k: Kind) =>
    gate(arrived, k, () => {
      let mark!: () => void;
      const seen = new Promise<void>((r) => (mark = r));
      return { seen, mark };
    });
  /** B's answer to `k` waits until `release(k)`. */
  const holding = new Set<Kind>();

  const campaign = (ws: typeof ACME | typeof BETA) => {
    const w = WS[ws];
    return blankCampaign(ws, {
      id: ID,
      name: w.campaign,
      list_ids: [LIST],
      sending_domain_id: DOMAIN,
      from_local_part: "news",
      variants: [{ subject: w.subject, template_id: T1, from_name: null }],
      created_at: SAME_TIME,
      updated_at: SAME_TIME,
    });
  };

  return {
    hold: (...kinds: Kind[]) => kinds.forEach((k) => holding.add(k)),
    release: (k: Kind) => gateOf(k).release(),
    /** B's request for `k` has reached the server stub. */
    arrived: (k: Kind) => seenOf(k).seen,
    install: () =>
      page.route(/\/api\/v1\/workspaces\//, async (route) => {
        const url = new URL(route.request().url());
        const [, , , , ws = "", area, id, verb] = url.pathname.split("/");
        const w = WS[ws as keyof typeof WS];
        const method = route.request().method();
        const kind: Kind | null =
          area === undefined
            ? "zone"
            : area === "campaigns"
              ? id === undefined
                ? "list"
                : verb === "estimate"
                  ? "estimate"
                  : verb === "preview"
                    ? "preview"
                    : "detail"
              : "options";
        if (ws === BETA && kind !== null) {
          seenOf(kind).mark();
          if (holding.has(kind)) await gateOf(kind).open;
        }
        if (area === undefined) {
          return route.fulfill({ json: { id: ws, name: w.name, role: "admin", slug: ws, timezone: w.zone } });
        }
        const page_ = (data: unknown[]) => route.fulfill({ json: { data, next_cursor: null } });
        if (area === "lists") return page_([{ id: LIST, name: w.list }]);
        if (area === "segments") return page_([]);
        if (area === "sending-domains") return page_([{ id: DOMAIN, domain: `mail.${w.name.toLowerCase()}.test` }]);
        if (area === "templates") {
          return page_([{ id: T1, name: "Welcome", kind: "blocks", created_at: SAME_TIME, updated_at: SAME_TIME }]);
        }
        const c = campaign(ws as typeof ACME);
        if (id === undefined && method === "GET") {
          return page_([{ id: ID, name: c.name, state: c.state, send_at: null, counts: c.counts, updated_at: c.updated_at }]);
        }
        if (verb === "estimate") return route.fulfill({ json: { count: w.count, exact: true } });
        if (verb === "preview") {
          return route.fulfill({ json: { subject: w.subject, html: "<p>hi</p>", text: "hi" } });
        }
        return route.fulfill({ json: c });
      }),
  };
}

test("every_cache_key_carries_the_workspace_so_the_other_workspaces_answer_never_shows", async ({ page, app }) => {
  const me: Me = {
    csrf_token: "t",
    display_name: "Ada",
    email: "ada@example.com",
    id: "22222222-2222-4222-8222-222222222222",
    is_instance_admin: false,
    passkey_count: 0,
    totp_enabled: false,
    workspaces: [
      { id: ACME, name: "Acme", role: "admin", slug: "acme" },
      { id: BETA, name: "Beta", role: "admin", slug: "beta" },
    ],
  };
  await page.route("**/api/v1/me", (route) => route.fulfill({ json: me }));
  const api = serve(page);
  await api.install();

  // Workspace A, fully loaded: list, then the campaign's screen with everything it fetches.
  await page.goto(`${app}/w/acme/campaigns`);
  await page.getByRole("link", { name: "Acme spring", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Acme spring", level: 2 })).toBeVisible();
  await expect(page.getByLabel("Acme list")).toBeVisible();
  await expect(page.getByText("111 recipients")).toBeVisible();
  await expect(page.getByText("Subject: Acme subject")).toBeVisible();

  // Over to B, whose answers all wait. Each step first sees the screen say it is waiting (so the
  // assertion that A's answer is absent cannot pass before the screen has even reacted), then
  // that A's answer is not there in its place.
  const waiting = (label: string) => expect(page.getByText(label, { exact: true })).toHaveCount(1);
  api.hold("list", "detail", "options", "estimate", "preview");
  await page.locator("#workspace").selectOption({ label: "Beta" });
  await page.getByRole("link", { name: "Campaigns", exact: true }).click();

  await api.arrived("list");
  await waiting("Loading campaigns");
  await expect(page.getByRole("link", { name: "Acme spring" })).toHaveCount(0);
  api.release("list");
  await page.getByRole("link", { name: "Beta spring", exact: true }).click();

  await api.arrived("detail");
  await waiting("Loading campaign");
  await expect(page.getByRole("heading", { name: "Acme spring" })).toHaveCount(0);
  // The editor waits for the workspace's zone; B's has not come, so no schedule may show A's.
  api.hold("zone");
  api.release("detail");
  await api.arrived("zone");
  await waiting("Loading the time zone");
  await expect(page.getByRole("region", { name: "Schedule", exact: true })).toHaveCount(0);
  api.release("zone");

  await api.arrived("options");
  await waiting("Loading the editor");
  await expect(page.getByLabel("Acme list")).toHaveCount(0);
  api.release("options");

  await api.arrived("estimate");
  await waiting("Counting…");
  await expect(page.getByText("111 recipients")).toHaveCount(0);
  api.release("estimate");

  await api.arrived("preview");
  await waiting("Loading the preview…");
  await expect(page.getByText("Subject: Acme subject")).toHaveCount(0);
  api.release("preview");

  // And B's own, once it has come.
  await expect(page.getByRole("heading", { name: "Beta spring", level: 2 })).toBeVisible();
  await expect(page.getByLabel("Beta list")).toBeVisible();
  await expect(page.getByText("222 recipients")).toBeVisible();
  await expect(page.getByText("Subject: Beta subject")).toBeVisible();
  await page.getByRole("region", { name: "Schedule", exact: true }).getByLabel("Send at").check();
  await expect(page.getByLabel("Zone")).toHaveValue("Europe/Paris");
});
