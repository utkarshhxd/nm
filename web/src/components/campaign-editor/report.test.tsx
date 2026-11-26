// SPDX-License-Identifier: AGPL-3.0-only

// The report section in a real browser, reached the way a person reaches it: open a launched
// campaign. The screen makes real HTTP calls; `campaign-stub.ts` answers them from the in-memory
// source, which holds each campaign's report.

import { type Page, expect } from "@playwright/test";

import { test } from "../../../e2e/support/app";
import type { components } from "../../api/schema";
import type { Campaign, Report } from "../../routes/campaign-model";
import { blankCampaign, mockCampaignSource, type MockState } from "../../routes/campaign-mock";
import { type Call, stubCampaignApi } from "../../routes/campaign-stub";

type Me = components["schemas"]["MeResponse"];

const ACME = "11111111-1111-4111-8111-111111111111";
const BETA = "33333333-3333-4333-8333-333333333333";
const T1 = "aaaaaaaa-0000-4000-8000-000000000001";
const NOW = "2026-01-01T00:00:00Z";
const TEMPLATE = { id: T1, name: "Welcome", kind: "blocks" as const, created_at: NOW, updated_at: NOW };
const OPTIONS: MockState["options"] = { lists: [], segments: [], sendingDomains: [], templates: [TEMPLATE] };
const ONE = [{ subject: "Spring is here", template_id: T1, from_name: null }];
const TWO = [...ONE, { subject: "Spring has sprung", template_id: T1, from_name: null }];

const launched = (over: Partial<Campaign> = {}) =>
  blankCampaign(ACME, {
    name: "Spring sale",
    state: "sent",
    launched_at: "2027-03-01T09:30:00Z",
    variants: ONE,
    ...over,
  });

const REPORT: Report = {
  sent: 90,
  failed: 4,
  bounced: 6,
  unique_opens: 41,
  unique_clicks: 12,
  unsubscribed: 3,
  links: [
    { variant: 0, link: 0, url: "https://example.test/sale", clicks: 15, unique_clicks: 9 },
    { variant: 0, link: 1, url: "https://example.test/shoes?utm=spring", clicks: 4, unique_clicks: 3 },
    { variant: 1, link: 0, url: "https://example.test/sale", clicks: 2, unique_clicks: 2 },
  ],
};

async function open(
  page: Page,
  app: string,
  src: ReturnType<typeof mockCampaignSource>,
  c: Campaign,
): Promise<Call[]> {
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
  const log = await stubCampaignApi(page, src);
  await page.goto(`${app}/w/acme/campaigns/${c.id}`);
  await expect(page.getByRole("heading", { name: c.name, level: 2 })).toBeVisible();
  return log;
}

const reports = (log: Call[]) => log.filter((x) => x.method === "GET" && x.path.endsWith("/report"));
const region = (page: Page) => page.getByRole("region", { name: "Report", exact: true });
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

test("report_shows_sent_failed_bounced_opened_clicked_and_unsubscribed_from_the_source", async ({ page, app }) => {
  const c = launched();
  const src = mockCampaignSource({ campaigns: [c], options: OPTIONS, reports: { [c.id]: REPORT } });
  const log = await open(page, app, src, c);
  const report = region(page);
  const row = (label: string, n: string) =>
    expect(report.locator("dt", { hasText: label }).locator("xpath=following-sibling::dd[1]")).toHaveText(n);
  await row("Sent", "90");
  await row("Failed", "4");
  await row("Bounced", "6");
  await row("Opened (approximate)", "41");
  await row("Clicked", "12");
  await row("Unsubscribed", "3");
  await expect(report).toContainText("Opens are approximate: some mail apps load every image, others load none.");
  // Asked for under the workspace that owns the campaign, and only once on opening.
  expect(reports(log).map((x) => x.path)).toEqual([`/api/v1/workspaces/${ACME}/campaigns/${c.id}/report`]);
});

test("report_lists_clicks_per_link_with_a_variant_column_only_for_two_or_more_variants", async ({ page, app }) => {
  const c = launched({ variants: TWO });
  const src = mockCampaignSource({ campaigns: [c], options: OPTIONS, reports: { [c.id]: REPORT } });
  await open(page, app, src, c);
  const table = region(page).getByRole("table", { name: "Clicks by link" });
  await expect(table.getByRole("columnheader")).toHaveText(["Variant", "Link", "Clicks", "Unique clicks"]);
  const rows = table.getByRole("row");
  await expect(rows).toHaveCount(4);
  // Variant is 1-based on screen; the wire's is 0-based.
  await expect(rows.nth(1).getByRole("cell")).toHaveText(["1", "https://example.test/sale", "15", "9"]);
  await expect(rows.nth(2).getByRole("cell")).toHaveText(["1", "https://example.test/shoes?utm=spring", "4", "3"]);
  await expect(rows.nth(3).getByRole("cell")).toHaveText(["2", "https://example.test/sale", "2", "2"]);

  const one = launched({ variants: ONE });
  await page.unrouteAll();
  await open(
    page,
    app,
    mockCampaignSource({
      campaigns: [one],
      options: OPTIONS,
      reports: { [one.id]: { ...REPORT, links: REPORT.links.slice(0, 2) } },
    }),
    one,
  );
  const single = region(page).getByRole("table", { name: "Clicks by link" });
  await expect(single.getByRole("columnheader")).toHaveText(["Link", "Clicks", "Unique clicks"]);
  await expect(single.getByRole("row").nth(1).getByRole("cell")).toHaveText(["https://example.test/sale", "15", "9"]);
});

test("report_says_there_are_no_tracked_links_when_the_table_is_empty", async ({ page, app }) => {
  // No report seeded: the source answers zeros and no links, as for a campaign launched before M2d.
  const c = launched();
  await open(page, app, mockCampaignSource({ campaigns: [c], options: OPTIONS }), c);
  const report = region(page);
  await expect(report.getByText("No tracked links in this campaign.")).toBeVisible();
  await expect(report.getByRole("table")).toHaveCount(0);
  await expect(report.locator("dt", { hasText: "Sent" }).locator("xpath=following-sibling::dd[1]")).toHaveText("0");
});

test("report_asks_the_source_again_on_refresh_and_never_on_a_timer", async ({ page, app }) => {
  const c = launched({ state: "sending" });
  const held = { [c.id]: REPORT };
  const src = mockCampaignSource({ campaigns: [c], options: OPTIONS, reports: held });
  const log = await open(page, app, src, c);
  const sent = region(page).locator("dt", { hasText: "Sent" }).locator("xpath=following-sibling::dd[1]");
  await expect(sent).toHaveText("90");
  expect(reports(log)).toHaveLength(1);

  // The campaign itself is polled every 2 s while it sends; the report must not be.
  await sleep(5000);
  expect(reports(log)).toHaveLength(1);

  // The numbers move on at the server; the screen shows them only after Refresh.
  held[c.id] = { ...REPORT, sent: 95, unique_clicks: 20 };
  await region(page).getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(sent).toHaveText("95");
  expect(reports(log)).toHaveLength(2);
});

test("report_says_when_it_was_read_and_asks_again_once_when_the_campaign_changes_state", async ({ page, app }) => {
  const c = launched({ state: "sending" });
  const held = { [c.id]: REPORT };
  const src = mockCampaignSource({ campaigns: [c], options: OPTIONS, reports: held });
  const log = await open(page, app, src, c);
  const sent = region(page).locator("dt", { hasText: "Sent" }).locator("xpath=following-sibling::dd[1]");
  await expect(sent).toHaveText("90");
  await expect(region(page)).toContainText(/As of \d\d:\d\d:\d\d/);
  expect(reports(log)).toHaveLength(1);

  // The send moves on and the campaign pauses on the server. The progress poll sees the new state,
  // and the report asks once, so it does not sit beside the progress with older numbers.
  held[c.id] = { ...REPORT, sent: 95 };
  await src.pause(ACME, c.id);
  await expect(sent).toHaveText("95");
  expect(reports(log)).toHaveLength(2);
  // Once, not once per poll: nothing asks again while the state stays put.
  await sleep(2500);
  expect(reports(log)).toHaveLength(2);
});

test("report_shows_the_problem_not_numbers_when_the_source_answers_404", async ({ page, app }) => {
  const c = launched();
  const src = mockCampaignSource({ campaigns: [c], options: OPTIONS, reports: { [c.id]: REPORT } });
  await open(page, app, src, c);
  await expect(region(page).locator("dt", { hasText: "Sent" })).toBeVisible();

  // The campaign is gone for this workspace: refreshing answers the server's 404, in its words.
  await page.route(/\/campaigns\/[^/]+\/report$/, (route) =>
    route.fulfill({
      status: 404,
      contentType: "application/problem+json",
      body: JSON.stringify({ type: "urn:nm:problem:not-found", title: "Error", status: 404, detail: "resource not found" }),
    }),
  );
  await region(page).getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(region(page).getByRole("alert")).toContainText("resource not found");
  // No numbers: not the last good ones either.
  await expect(region(page).locator("dt")).toHaveCount(0);
  await expect(region(page).getByRole("table")).toHaveCount(0);
});

test("another_workspaces_campaign_has_no_report_to_read", async () => {
  const c = launched();
  const src = mockCampaignSource({ campaigns: [c], options: OPTIONS, reports: { [c.id]: REPORT } });
  expect((await src.report(ACME, c.id)).ok).toBe(true);
  const other = await src.report(BETA, c.id);
  expect(other.ok).toBe(false);
  expect((other as { error: { status: number } }).error.status).toBe(404);
});

test("a_campaign_not_yet_launched_asks_for_no_report", async ({ page, app }) => {
  const c = launched({ state: "scheduled", launched_at: null });
  const log = await open(page, app, mockCampaignSource({ campaigns: [c], options: OPTIONS }), c);
  await expect(page.getByRole("region", { name: "Progress", exact: true })).toBeVisible();
  await expect(region(page)).toHaveCount(0);
  expect(reports(log)).toHaveLength(0);
});
