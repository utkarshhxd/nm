// SPDX-License-Identifier: AGPL-3.0-only

// The relay screen against stubbed API answers, in a real browser. The point is the password:
// the API never returns one, and a PATCH tells "leave it", "replace it" and "remove it" apart
// only by the key being absent, a string, or null. A "keep" that sent null would delete the
// stored credential on an ordinary save and nothing else would notice.

import { type Page, expect } from "@playwright/test";

import type { components } from "../api/schema";
import { test } from "../../e2e/support/app";

type Relay = components["schemas"]["RelayView"];
type Me = components["schemas"]["MeResponse"];

const WS = "11111111-1111-4111-8111-111111111111";
const RELAY: Relay = {
  id: "b0000000-0000-4000-8000-000000000001",
  workspace_id: WS,
  name: "oci-primary",
  host: "smtp.example.com",
  port: 587,
  security: "starttls",
  username: "relay-user",
  has_password: true,
  max_connections: 4,
  rate_limit_per_minute: 60,
  enabled: true,
  failover_order: 0,
};
const ROUTE = /\/api\/v1\/workspaces\/[^/]+\/relays(\/[^/?]+)?(\?.*)?$/;

/** Opens the screen and returns the bodies of every write it sends. */
async function open(page: Page, app: string, rows: Relay[], role: "admin" | "viewer" = "admin") {
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
  const bodies: Record<string, unknown>[] = [];
  await page.route(ROUTE, (route) => {
    const req = route.request();
    if (req.method() === "GET") return route.fulfill({ json: { data: rows } });
    bodies.push(req.postDataJSON() as Record<string, unknown>);
    return route.fulfill({ status: req.method() === "POST" ? 201 : 200, json: RELAY });
  });
  await page.goto(`${app}/w/acme/relays`);
  await expect(
    page.getByRole("row").filter({ hasText: "oci-primary" }).or(
      page.getByRole("heading", { name: "No relays configured" }),
    ),
  ).toBeVisible();
  return bodies;
}

const save = async (page: Page) => {
  await page.getByRole("button", { name: "Save relay" }).click();
  await expect(page.getByRole("dialog")).toBeHidden();
};

test("the list says set or not set and never offers a password to read", async ({ page, app }) => {
  await open(page, app, [
    RELAY,
    { ...RELAY, id: "b0000000-0000-4000-8000-000000000002", name: "bare", has_password: false },
  ]);
  await expect(
    page.getByRole("row").filter({ hasText: "oci-primary" }).getByText("Set"),
  ).toBeVisible();
  await expect(page.getByRole("row").filter({ hasText: "bare" }).getByText("Not set")).toBeVisible();
  await page.getByRole("button", { name: "Edit relay oci-primary" }).click();
  // No password input exists until the operator chooses to replace it.
  await expect(page.locator("input[type=password]")).toHaveCount(0);
});

test("saving an untouched relay sends no password key, so the stored one stays", async ({ page, app }) => {
  const bodies = await open(page, app, [RELAY]);
  await page.getByRole("button", { name: "Edit relay oci-primary" }).click();
  await save(page);
  expect(bodies).toHaveLength(1);
  expect(bodies[0]).not.toHaveProperty("password");
  expect(bodies[0]).toMatchObject({ name: "oci-primary", host: "smtp.example.com", port: 587 });
});

test("replacing the password sends the new value", async ({ page, app }) => {
  const bodies = await open(page, app, [RELAY]);
  await page.getByRole("button", { name: "Edit relay oci-primary" }).click();
  await page.getByLabel(/^Password:/).selectOption("replace");
  await page.getByLabel("New password").fill("s3cret-new");
  await save(page);
  expect(bodies).toHaveLength(1);
  expect(bodies[0]).toHaveProperty("password", "s3cret-new");
});

test("removing the password sends null", async ({ page, app }) => {
  const bodies = await open(page, app, [RELAY]);
  await page.getByRole("button", { name: "Edit relay oci-primary" }).click();
  await page.getByLabel(/^Password:/).selectOption("clear");
  await save(page);
  expect(bodies).toHaveLength(1);
  expect(bodies[0]).toHaveProperty("password", null);
});

test("choosing to replace without typing one is refused and sends nothing", async ({ page, app }) => {
  const bodies = await open(page, app, [RELAY]);
  await page.getByRole("button", { name: "Edit relay oci-primary" }).click();
  await page.getByLabel(/^Password:/).selectOption("replace");
  await page.getByRole("button", { name: "Save relay" }).click();
  await expect(page.getByRole("alert").filter({ hasText: "Type the new password" })).toBeVisible();
  expect(bodies).toEqual([]);
});

test("a new relay with no password sends no password key", async ({ page, app }) => {
  const bodies = await open(page, app, []);
  await page.getByRole("button", { name: "Add relay" }).click();
  await page.getByLabel("Name", { exact: true }).fill("fresh");
  await page.getByLabel("Host").fill("smtp.fresh.example");
  await page.getByRole("dialog").getByRole("button", { name: "Add relay" }).click();
  await expect(page.getByRole("dialog")).toBeHidden();
  expect(bodies).toHaveLength(1);
  expect(bodies[0]).not.toHaveProperty("password");
  expect(bodies[0]).toMatchObject({ name: "fresh", host: "smtp.fresh.example" });
});

test("a viewer sees the list and no controls", async ({ page, app }) => {
  await open(page, app, [RELAY], "viewer");
  await expect(page.getByRole("button", { name: /Edit|Delete|Add relay/ })).toHaveCount(0);
});
