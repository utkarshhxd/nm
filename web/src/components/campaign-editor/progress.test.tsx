// SPDX-License-Identifier: AGPL-3.0-only

// The progress view in a real browser. The screen makes real HTTP calls; `campaign-stub.ts`
// answers them from the in-memory source, which the test can also change from outside to play the
// part of the send moving on without the screen.

import { type Page, expect } from "@playwright/test";

import { test } from "../../../e2e/support/app";
import type { components } from "../../api/schema";
import type { Campaign } from "../../routes/campaign-model";
import { blankCampaign, mockCampaignSource, type MockState } from "../../routes/campaign-mock";
import { type Call, stubCampaignApi } from "../../routes/campaign-stub";

type Me = components["schemas"]["MeResponse"];
type Role = Me["workspaces"][number]["role"];

const ACME = "11111111-1111-4111-8111-111111111111";
const BETA = "33333333-3333-4333-8333-333333333333";
const T1 = "aaaaaaaa-0000-4000-8000-000000000001";
const NOW = "2026-01-01T00:00:00Z";
const TEMPLATE = { id: T1, name: "Welcome", kind: "blocks" as const, created_at: NOW, updated_at: NOW };
const OPTIONS: MockState["options"] = { lists: [], segments: [], sendingDomains: [], templates: [TEMPLATE] };
const VARIANTS = [
  { subject: "Spring is here", template_id: T1, from_name: null },
  { subject: "Spring has sprung", template_id: T1, from_name: null },
];
const counts = (c: Partial<Campaign["counts"]>) => ({
  total: 100,
  queued: 0,
  sent: 0,
  failed: 0,
  unknown: 0,
  skipped: 0,
  ...c,
});

const live = (over: Partial<Campaign> = {}) =>
  blankCampaign(ACME, {
    name: "Spring sale",
    state: "sending",
    schedule: { mode: "instant", zone: "UTC", local: null, send_at: "2027-03-01T09:30:00Z" },
    counts: counts({ sent: 40, failed: 3, unknown: 2, skipped: 5, queued: 50 }),
    ...over,
  });

const source = (c: Campaign) => mockCampaignSource({ campaigns: [c], options: OPTIONS });

async function open(
  page: Page,
  app: string,
  src: ReturnType<typeof mockCampaignSource>,
  c: Campaign,
  role: Role = "admin",
  zones: Record<string, string> = {},
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
      { id: ACME, name: "Acme", role, slug: "acme" },
      { id: BETA, name: "Beta", role, slug: "beta" },
    ],
  };
  await page.route("**/api/v1/me", (route) => route.fulfill({ json: me }));
  const log = await stubCampaignApi(page, src, zones);
  await page.goto(`${app}/w/acme/campaigns/${c.id}`);
  await expect(page.getByRole("heading", { name: c.name, level: 2 })).toBeVisible();
  return log;
}

const detailGets = (log: Call[], c: Campaign) =>
  log.filter((x) => x.method === "GET" && x.path.endsWith(`/campaigns/${c.id}`)).length;
const button = (page: Page, name: string) => page.getByRole("button", { name, exact: true });
/** The state badge: a pill, so "Sending" never matches the Resume sending button. */
const badge = (page: Page, name: string) => page.locator("span.rounded-full").filter({ hasText: name });
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

test("the_progress_view_shows_every_count_with_the_uncertain_one_named_for_what_it_means", async ({ page, app }) => {
  const c = live();
  await open(page, app, source(c), c);
  const progress = page.getByRole("region", { name: "Progress", exact: true });
  await expect(progress).toContainText("40 of 100 sent");
  const row = (label: string, n: string) =>
    expect(progress.locator("dt", { hasText: label }).locator("xpath=following-sibling::dd[1]")).toHaveText(n);
  await row("Sent", "40");
  await row("Failed", "3");
  await row("Uncertain — never retried", "2");
  await row("Skipped (unsubscribed or suppressed since launch)", "5");
  await row("Queued", "50");
});

test("a_pause_or_failure_reason_is_shown_word_for_word", async ({ page, app }) => {
  const why = "The relay refused the connection: 421 too many connections, so sending paused itself.";
  const paused = live({ state: "paused", pause_reason: why });
  await open(page, app, source(paused), paused);
  await expect(page.getByText(why, { exact: true })).toBeVisible();

  const reason = "The template was changed after this was scheduled and no longer renders.";
  const failed = live({ state: "failed", failure_reason: reason });
  await open(page, app, source(failed), failed);
  await expect(page.getByText(reason, { exact: true })).toBeVisible();
});

test("it_polls_the_campaign_while_it_is_sending", async ({ page, app }) => {
  const c = live();
  const log = await open(page, app, source(c), c);
  const first = detailGets(log, c);
  // Two seconds apart: two more asks are due inside five seconds.
  await expect.poll(() => detailGets(log, c) - first, { timeout: 7000 }).toBeGreaterThanOrEqual(2);
});

test("it_stops_polling_when_the_campaign_pauses_and_never_polls_one_that_is_not_sending", async ({ page, app }) => {
  const c = live();
  const src = source(c);
  const log = await open(page, app, src, c);
  await expect.poll(() => detailGets(log, c), { timeout: 7000 }).toBeGreaterThanOrEqual(2);

  // The send pauses on the server; the next poll brings the screen up to date.
  await src.pause(ACME, c.id);
  await expect(badge(page, "Paused")).toBeVisible();
  const settled = detailGets(log, c);
  await sleep(4500);
  expect(detailGets(log, c)).toBe(settled);
});

test("a_winner_pick_posts_the_variant_index_and_the_picker_goes_away", async ({ page, app }) => {
  const c = live({ variants: VARIANTS, ab_test_percent: 20, awaiting_winner: true, state: "paused" });
  await open(page, app, source(c), c);
  await expect(page.getByText("No engagement data until tracking ships — pick on what you know.")).toBeVisible();
  const picker = page.getByRole("region", { name: "Pick the winner", exact: true });
  await expect(picker).toContainText("Variant 1: Spring is here");
  await expect(picker).toContainText("Variant 2: Spring has sprung");

  const posted = page.waitForRequest((r) => r.url().endsWith("/winner"));
  await button(page, "Pick variant 2").click();
  expect((await posted).postDataJSON()).toEqual({ variant: 1 });
  await expect(page.getByText("Winner: variant 2.")).toBeVisible();
  await expect(button(page, "Pick variant 2")).toHaveCount(0);
});

test("only_an_admin_picks_and_only_while_a_winner_is_awaited", async ({ page, app }) => {
  const c = live({ variants: VARIANTS, ab_test_percent: 20, awaiting_winner: true, state: "paused" });
  await open(page, app, source(c), c, "editor");
  // An editor still sees what is being chosen between, but no way to choose.
  await expect(page.getByText("Variant 1: Spring is here")).toBeVisible();
  await expect(button(page, "Pick variant 1")).toHaveCount(0);

  // Not awaiting: no picker at all, even for an admin.
  const done = live({ variants: VARIANTS, ab_test_percent: 20, awaiting_winner: false, state: "paused" });
  await open(page, app, source(done), done, "admin");
  await expect(page.getByText("Progress", { exact: true })).toBeVisible();
  await expect(page.getByText("No engagement data until tracking ships")).toHaveCount(0);
  await expect(button(page, "Pick variant 1")).toHaveCount(0);
});

test("each_role_is_offered_only_the_verbs_the_contract_gives_it_in_each_state", async ({ page, app }) => {
  const verbs = ["Pause", "Resume", "Unschedule", "Cancel campaign"];
  const shown = async () => {
    // One look, taken once the screen has settled on this campaign.
    await expect(page.getByRole("region", { name: "Progress", exact: true })).toBeVisible();
    return (await Promise.all(verbs.map((v) => button(page, v).count()))).map((n, i) => (n > 0 ? verbs[i] : ""))
      .filter(Boolean);
  };
  const cases: [Campaign["state"], Role, string[]][] = [
    ["sending", "editor", ["Pause"]],
    ["sending", "admin", ["Pause", "Cancel campaign"]],
    ["sending", "viewer", []],
    ["paused", "editor", []],
    ["paused", "admin", ["Resume", "Cancel campaign"]],
    ["scheduled", "admin", ["Unschedule", "Cancel campaign"]],
    ["scheduled", "editor", []],
    ["sent", "admin", []],
    ["cancelled", "admin", []],
    ["failed", "admin", []],
  ];
  for (const [state, role, want] of cases) {
    const c = live({ state });
    await page.unrouteAll();
    await open(page, app, source(c), c, role);
    expect(await shown(), `${role} on a ${state} campaign`).toEqual(want);
  }
});

test("pause_resume_and_cancel_move_the_campaign_and_the_screen_follows", async ({ page, app }) => {
  const c = live();
  await open(page, app, source(c), c);
  await button(page, "Pause").click();
  await expect(badge(page, "Paused")).toBeVisible();
  await button(page, "Resume").click();
  await expect(badge(page, "Sending")).toBeVisible();

  await button(page, "Cancel campaign").click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("Nothing more will be sent");
  await dialog.getByRole("button", { name: "Cancel campaign", exact: true }).click();
  await expect(badge(page, "Cancelled")).toBeVisible();
  await expect(button(page, "Pause")).toHaveCount(0);
});

test("resuming_a_recipient_local_campaign_warns_that_overdue_recipients_are_sent_at_once", async ({ page, app }) => {
  const c = live({
    state: "paused",
    schedule: { mode: "recipient_local", zone: "Europe/London", local: "2027-03-01T09:30", send_at: "2027-03-01T09:30:00Z" },
  });
  await open(page, app, source(c), c);
  await button(page, "Resume").click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("Recipients whose time has already passed are sent the email at once");
  // Asking did not resume it.
  await expect(badge(page, "Paused")).toBeVisible();
  await dialog.getByRole("button", { name: "Resume sending" }).click();
  await expect(badge(page, "Sending")).toBeVisible();

  // An instant campaign has nothing overdue to warn about: Resume acts at once.
  const plain = live({ state: "paused" });
  await page.unrouteAll();
  await open(page, app, source(plain), plain);
  await button(page, "Resume").click();
  await expect(badge(page, "Sending")).toBeVisible();
});

test("a_refused_verb_shows_the_servers_sentence_and_is_not_shown_as_done", async ({ page, app }) => {
  const c = live({ state: "paused" });
  const src = source(c);
  await open(page, app, src, c);
  // The campaign is cancelled elsewhere; this screen still shows it paused.
  await src.cancel(ACME, c.id);
  await button(page, "Resume").click();
  await expect(page.getByRole("alert")).toContainText(`campaign ${c.id} is cancelled`);
  await expect(badge(page, "Sending")).toHaveCount(0);
  await expect(badge(page, "Paused")).toBeVisible();
});

test("unscheduling_a_scheduled_campaign_returns_it_to_the_editor", async ({ page, app }) => {
  const c = live({
    state: "scheduled",
    counts: counts({}),
    schedule: { mode: "instant", zone: "Europe/London", local: "2027-03-01T09:30", send_at: "2027-03-01T09:30:00Z" },
  });
  await open(page, app, source(c), c);
  await expect(page.getByText("Set for 2027-03-01 09:30 Europe/London")).toBeVisible();
  await button(page, "Unschedule").click();
  await expect(button(page, "Save audience")).toBeVisible();
});

test("a_scheduled_campaign_can_be_test_sent_by_an_editor_and_no_other_state_can", async ({ page, app }) => {
  const c = live({ state: "scheduled", counts: counts({}), variants: [VARIANTS[0]!] });
  const log = await open(page, app, source(c), c, "editor");
  const test_ = page.getByRole("region", { name: "Test send", exact: true });
  await test_.getByLabel("Addresses").fill("qa@example.test");
  await button(test_, "Send test").click();
  await expect(test_.getByRole("status")).toHaveText("Test queued to 1 address.");
  expect(log.filter((x) => x.method === "POST" && x.path.endsWith(`/campaigns/${c.id}/test`))).toHaveLength(1);

  // The contract gives a test send to draft and scheduled only; one look per state.
  for (const state of ["sending", "paused", "sent", "cancelled", "failed"] as const) {
    const other = live({ state });
    await page.unrouteAll();
    await open(page, app, source(other), other, "admin");
    await expect(page.getByRole("region", { name: "Progress", exact: true })).toBeVisible();
    expect(await page.getByRole("region", { name: "Test send", exact: true }).count(), state).toBe(0);
  }
});
