// SPDX-License-Identifier: AGPL-3.0-only

// The sending-domain screen against stubbed API answers, in a real browser: what an operator
// sees for an acknowledged domain, an unacknowledged one with a failing check, and a clean one,
// and what the acknowledgement checkbox sends. No database and no `nm` server.

import { type Page, expect } from "@playwright/test";

import type { components } from "../api/schema";
import { test } from "../../e2e/support/app";

type Domain = components["schemas"]["SendingDomainView"];
type Me = components["schemas"]["MeResponse"];

const WS = "11111111-1111-4111-8111-111111111111";
const pass = { status: "pass" } as const;
const unchecked = { status: "not_checked" } as const;

const domain = (over: Partial<Domain> & Pick<Domain, "id" | "domain">): Domain => ({
  workspace_id: WS,
  dkim_selector: "nm1",
  has_dkim_key: false,
  signing: "relay",
  checked_at: "2026-11-20T10:00:00Z",
  checks: { spf: pass, dkim: pass, dmarc: pass, return_path: pass },
  ...over,
});

const FAILING = domain({
  id: "a0000000-0000-4000-8000-000000000001",
  domain: "failing.example",
  signing: "app",
  has_dkim_key: true,
  dkim_public_txt: "v=DKIM1; k=rsa; p=QUJDREVGRw==",
  checks: {
    spf: pass,
    dkim: { status: "fail", reason: "the published p= does not match the key this domain signs with" },
    dmarc: unchecked,
    return_path: pass,
  },
});
const ACKED = domain({
  id: "a0000000-0000-4000-8000-000000000002",
  domain: "acked.example",
  checks: { spf: { status: "fail", reason: "no SPF record" }, dkim: pass, dmarc: pass, return_path: pass },
  send_unverified_acknowledged_at: "2026-11-21T09:30:00Z",
});
const CLEAN = domain({ id: "a0000000-0000-4000-8000-000000000003", domain: "clean.example" });

async function open(page: Page, app: string, rows: Domain[], role: "admin" | "viewer" = "admin") {
  const me: Me = {
    csrf_token: "t",
    display_name: "Ada",
    email: "ada@example.com",
    id: "22222222-2222-4222-8222-222222222222",
    is_instance_admin: false,
    passkey_count: 0,
    totp_enabled: false,
    workspaces: [{ id: WS, name: "Acme", role, slug: "acme" }],
  };
  await page.route("**/api/v1/me", (route) => route.fulfill({ json: me }));
  await page.route(/\/api\/v1\/workspaces\/[^/]+\/sending-domains(\?.*)?$/, (route) => route.fulfill({ json: { data: rows } }));
  await page.goto(`${app}/w/acme/sending-domains`);
  await expect(page.getByRole("heading", { name: "Sending domains", level: 2 })).toBeVisible();
}

const row = (page: Page, name: string) => page.getByRole("row").filter({ hasText: name });

test("an unacknowledged domain with a failing check says it will not send, and why it fails", async ({ page, app }) => {
  await open(page, app, [FAILING]);
  const r = row(page, "failing.example");
  await expect(r.getByText("Not allowed to send")).toBeVisible();
  await expect(r.getByText("the published p= does not match the key this domain signs with")).toBeVisible();
  await expect(r.getByText("Not checked")).toBeVisible();
  // The record to publish is the API's own value, shown in full.
  await expect(r.getByText("v=DKIM1; k=rsa; p=QUJDREVGRw==")).toBeVisible();
  await expect(r.getByText("nm1._domainkey.failing.example")).toBeVisible();
});

test("an acknowledged domain shows when, and the edit form opens with the box ticked", async ({ page, app }) => {
  await open(page, app, [ACKED]);
  const r = row(page, "acked.example");
  await expect(r.getByText("Sending unverified allowed")).toBeVisible();
  await expect(r.getByText("Not allowed to send")).toHaveCount(0);
  await r.getByRole("button", { name: "Edit acked.example" }).click();
  await expect(page.getByRole("checkbox", { name: /Allow sending from this domain/ })).toBeChecked();
});

test("a domain whose checks all pass is not flagged", async ({ page, app }) => {
  await open(page, app, [CLEAN]);
  await expect(row(page, "clean.example").getByText("Not allowed to send")).toHaveCount(0);
});

test("ticking the box sends the acknowledgement alone, and an untouched form sends nothing", async ({ page, app }) => {
  await open(page, app, [FAILING]);
  const bodies: unknown[] = [];
  await page.route(/\/api\/v1\/workspaces\/[^/]+\/sending-domains\/[^/?]+$/, (route) => {
    bodies.push(route.request().postDataJSON());
    return route.fulfill({ json: { ...FAILING, send_unverified_acknowledged_at: "2026-11-22T00:00:00Z" } });
  });
  await page.getByRole("button", { name: "Edit failing.example" }).click();
  const box = page.getByRole("checkbox", { name: /Allow sending from this domain/ });
  await expect(box).not.toBeChecked();
  // Nothing changed, so there is no request: a PATCH of the same settings would reset the checks.
  await page.getByRole("button", { name: "Save domain" }).click();
  await expect(page.getByRole("dialog")).toBeHidden();
  expect(bodies).toEqual([]);

  await page.getByRole("button", { name: "Edit failing.example" }).click();
  await box.check();
  await page.getByRole("button", { name: "Save domain" }).click();
  await expect(page.getByRole("dialog")).toBeHidden();
  expect(bodies).toEqual([{ send_unverified_acknowledged: true }]);
});

test("a viewer sees the list and no controls", async ({ page, app }) => {
  await open(page, app, [FAILING], "viewer");
  await expect(row(page, "failing.example")).toBeVisible();
  await expect(page.getByRole("button", { name: /Edit|Delete|Re-check|Add sending domain/ })).toHaveCount(0);
  await expect(page.getByRole("link", { name: "Sending domains" })).toBeVisible();
});
