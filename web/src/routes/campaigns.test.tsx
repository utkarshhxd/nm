// SPDX-License-Identifier: AGPL-3.0-only

// The campaign list in a real browser. The screen makes real HTTP calls; `campaign-stub.ts` answers
// them from the in-memory source, so the wire is the production one and the app carries no mock.

import { type Page, expect } from "@playwright/test";

import { test } from "../../e2e/support/app";
import type { components } from "../api/schema";
import { blankCampaign, mockCampaignSource, type MockState } from "./campaign-mock";
import { stubCampaignApi } from "./campaign-stub";

type Me = components["schemas"]["MeResponse"];
type Role = Me["workspaces"][number]["role"];

const ACME = "11111111-1111-4111-8111-111111111111";
const BETA = "33333333-3333-4333-8333-333333333333";

async function open(
  page: Page,
  app: string,
  seed: Partial<MockState>,
  role: Role = "editor",
  path = "campaigns",
): Promise<void> {
  const me: Me = {
    csrf_token: "t",
    display_name: "Ada",
    email: "ada@example.com",
    id: "22222222-2222-4222-8222-222222222222",
    is_instance_admin: false,
    passkey_count: 0,
    totp_enabled: false,
    workspaces: [
      { id: ACME, name: "Acme", role, slug: "acme" },
      { id: BETA, name: "Beta", role, slug: "beta" },
    ],
  };
  await page.route("**/api/v1/me", (route) => route.fulfill({ json: me }));
  await stubCampaignApi(page, mockCampaignSource(seed));
  await page.goto(`${app}/w/acme/${path}`);
}

const counts = (sent: number, total: number) => ({
  total,
  queued: total - sent,
  sent,
  failed: 0,
  bounced: 0,
  unknown: 0,
  skipped: 0,
});

test("the_list_shows_name_state_send_at_and_sent_over_total", async ({ page, app }) => {
  const sent = blankCampaign(ACME, {
    name: "Spring sale",
    state: "sent",
    counts: counts(40, 50),
    schedule: { mode: "instant", zone: "UTC", local: "2027-03-01T09:30", send_at: "2027-03-01T09:30:00Z" },
  });
  await open(page, app, { campaigns: [sent, blankCampaign(ACME, { name: "Next idea" })] });
  const row = page.getByRole("row", { name: /Spring sale/ });
  await expect(row.getByRole("link", { name: "Spring sale" })).toBeVisible();
  await expect(row.getByText("Sent").first()).toBeVisible(); // the state badge
  await expect(row).toContainText("1 Mar 2027, 09:30 UTC");
  await expect(row).toContainText("40 / 50");
  // A draft has no send time yet.
  await expect(page.getByRole("row", { name: /Next idea/ })).toContainText("—");
});

test("new_campaign_creates_a_draft_and_opens_it", async ({ page, app }) => {
  await open(page, app, { campaigns: [] });
  await expect(page.getByText("No campaigns yet")).toBeVisible();
  await page.getByRole("button", { name: "New campaign" }).click();
  await page.getByLabel("Name").fill("Autumn push");
  await page.getByRole("button", { name: "Create campaign" }).click();
  await expect(page).toHaveURL(/\/w\/acme\/campaigns\/[0-9a-f-]{36}$/);
  await expect(page.getByRole("heading", { name: "Autumn push", level: 2 })).toBeVisible();
  await expect(page.getByText("Draft")).toBeVisible();
  // Back on the list the new draft is there: the create invalidated the cache.
  await page.getByRole("link", { name: "Campaigns", exact: true }).click();
  await expect(page.getByRole("link", { name: "Autumn push" })).toBeVisible();
});

test("an_empty_name_shows_the_servers_sentence_and_stays_on_the_list", async ({ page, app }) => {
  await open(page, app, { campaigns: [] });
  await page.getByRole("button", { name: "New campaign" }).click();
  await page.getByRole("button", { name: "Create campaign" }).click();
  await expect(page.getByRole("alert")).toContainText("A campaign needs a name.");
  await expect(page).toHaveURL(/\/campaigns$/);
});

test("an_editor_sees_no_send_and_an_admin_sees_it_on_drafts_only", async ({ page, app }) => {
  const seed = {
    campaigns: [
      blankCampaign(ACME, { name: "A draft" }),
      blankCampaign(ACME, { name: "Gone out", state: "sent" }),
    ],
  };
  await open(page, app, seed, "editor");
  await expect(page.getByRole("link", { name: "A draft", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "New campaign" })).toBeVisible();
  await expect(page.getByText("Send", { exact: true })).toHaveCount(0);

  await open(page, app, seed, "admin");
  await expect(page.getByRole("link", { name: "Send A draft" })).toBeVisible();
  await expect(page.getByRole("link", { name: /^Send / })).toHaveCount(1);
});

test("a_viewer_can_read_the_list_but_has_no_new_campaign", async ({ page, app }) => {
  await open(page, app, { campaigns: [blankCampaign(ACME, { name: "A draft" })] }, "viewer");
  await expect(page.getByRole("link", { name: "A draft", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "New campaign" })).toHaveCount(0);
  await expect(page.getByText("Send", { exact: true })).toHaveCount(0);
});

test("another_workspaces_campaigns_never_show_and_its_id_is_a_not_found", async ({ page, app }) => {
  const theirs = blankCampaign(BETA, { name: "Beta secret" });
  await open(page, app, { campaigns: [blankCampaign(ACME, { name: "Acme plan" }), theirs] });
  await expect(page.getByRole("link", { name: "Acme plan" })).toBeVisible();
  await expect(page.getByText("Beta secret")).toHaveCount(0);

  // The same id under acme's address: a 404 sentence, not the campaign.
  await page.goto(`${app}/w/acme/campaigns/${theirs.id}`);
  await expect(page.getByText("This campaign no longer exists", { exact: false })).toBeVisible();
  await expect(page.getByText("Beta secret")).toHaveCount(0);
});

test("switching_workspace_never_shows_the_other_workspaces_cached_list", async ({ page, app }) => {
  // Slow answers, so a list cached under the wrong key would be on screen while the real one loads.
  await open(page, app, {
    delayMs: 400,
    campaigns: [blankCampaign(ACME, { name: "Acme plan" }), blankCampaign(BETA, { name: "Beta plan" })],
  });
  await expect(page.getByRole("link", { name: "Acme plan" })).toBeVisible();
  await page.locator("#workspace").selectOption("beta"); // lands on Beta's members page
  await expect(page).toHaveURL(/\/w\/beta\/members$/);
  await page.getByRole("link", { name: "Campaigns", exact: true }).click();
  // The heading and any cached rows render in one commit, so a count taken the moment the heading
  // shows is a look at what the screen showed first. `toHaveCount(0)` would retry until Beta's
  // answer arrived and pass for the wrong reason.
  await expect(page.getByRole("heading", { name: "Campaigns", level: 2 })).toBeVisible();
  expect(await page.getByText("Acme plan").count()).toBe(0);
  await expect(page.getByRole("link", { name: "Beta plan" })).toBeVisible();
  await expect(page.getByText("Acme plan")).toHaveCount(0);
});

test("a_long_list_loads_more_on_request", async ({ page, app }) => {
  const campaigns = Array.from({ length: 51 }, (_, i) =>
    blankCampaign(ACME, {
      name: `Campaign ${String(i).padStart(2, "0")}`,
      created_at: new Date(Date.UTC(2026, 0, 1, 0, i)).toISOString(),
    }),
  );
  await open(page, app, { campaigns });
  await expect(page.getByRole("link", { name: /^Campaign / })).toHaveCount(50);
  await page.getByRole("button", { name: "Load more" }).click();
  await expect(page.getByRole("link", { name: /^Campaign / })).toHaveCount(51);
  await expect(page.getByRole("button", { name: "Load more" })).toHaveCount(0);
});
