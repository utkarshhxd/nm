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
  bounced: 0,
  unknown: 0,
  skipped: 0,
  ...c,
});

const live = (over: Partial<Campaign> = {}) =>
  blankCampaign(ACME, {
    name: "Spring sale",
    state: "sending",
    schedule: { mode: "instant", zone: "UTC", local: null, send_at: "2027-03-01T09:30:00Z" },
    counts: counts({ sent: 40, failed: 3, bounced: 7, unknown: 2, skipped: 5, queued: 43 }),
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
  await row("Bounced (no such mailbox; now suppressed)", "7");
  await row("Uncertain — never retried", "2");
  await row("Skipped (unsubscribed or suppressed since launch)", "5");
  await row("Queued", "43");
});

test("progress_lists_bounced_as_its_own_count_and_queued_leaves_it_out", async ({ page, app }) => {
  // 100 = 40 sent + 3 failed + 7 bounced + 2 unknown + 5 skipped + 43 queued: bounced is not queued.
  const c = live();
  await open(page, app, source(c), c);
  const progress = page.getByRole("region", { name: "Progress", exact: true });
  const labels = progress.locator("dt");
  await expect(labels).toHaveText([
    "Sent",
    "Failed",
    "Bounced (no such mailbox; now suppressed)",
    "Uncertain — never retried",
    "Skipped (unsubscribed or suppressed since launch)",
    "Queued",
  ]);
  const n = async (label: string) =>
    Number(await progress.locator("dt", { hasText: label }).locator("xpath=following-sibling::dd[1]").textContent());
  const parts = await Promise.all(["Sent", "Failed", "Bounced", "Uncertain", "Skipped", "Queued"].map(n));
  expect(parts.reduce((a, b) => a + b, 0)).toBe(100);
});

test("a_cancelled_campaign_calls_its_unsent_messages_not_sent_rather_than_queued", async ({ page, app }) => {
  // They never go: only a sending campaign's messages are claimed, so "Queued" reads as still going out.
  const c = live({ state: "cancelled", counts: counts({ sent: 6500, total: 10000, queued: 3500 }) });
  await open(page, app, source(c), c);
  const progress = page.getByRole("region", { name: "Progress", exact: true });
  await expect(
    progress.locator("dt", { hasText: "Not sent (cancelled)" }).locator("xpath=following-sibling::dd[1]"),
  ).toHaveText("3500");
  await expect(progress.locator("dt", { hasText: "Queued" })).toHaveCount(0);
});

test("progress_shows_no_report_before_launch", async ({ page, app }) => {
  const c = live({ state: "scheduled", launched_at: null, counts: counts({}) });
  const log = await open(page, app, source(c), c);
  await expect(page.getByRole("region", { name: "Progress", exact: true })).toBeVisible();
  await expect(page.getByRole("region", { name: "Report", exact: true })).toHaveCount(0);
  expect(log.filter((x) => x.path.endsWith("/report"))).toHaveLength(0);

  // Launched: the report is there, below Progress.
  const sent = live({ launched_at: "2027-03-01T09:30:00Z" });
  await page.unrouteAll();
  await open(page, app, source(sent), sent);
  await expect(page.getByRole("region", { name: "Report", exact: true })).toBeVisible();
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
  const picker = page.getByRole("region", { name: "Pick the winner", exact: true });
  await expect(picker).toContainText("Each variant's link clicks are in the report below.");
  await expect(picker).toContainText("Variant 1: Spring is here");
  await expect(picker).toContainText("Variant 2: Spring has sprung");

  const posted = page.waitForRequest((r) => r.url().endsWith("/winner"));
  await button(page, "Pick variant 2").click();
  expect((await posted).postDataJSON()).toEqual({ variant: 1 });
  await expect(page.getByText("Winner: variant 2.")).toBeVisible();
  await expect(button(page, "Pick variant 2")).toHaveCount(0);
});

test("progress_winner_picker_points_at_the_report", async ({ page, app }) => {
  const c = live({
    variants: VARIANTS,
    ab_test_percent: 20,
    awaiting_winner: true,
    state: "paused",
    launched_at: "2027-03-01T09:30:00Z",
  });
  await open(page, app, source(c), c);
  const picker = page.getByRole("region", { name: "Pick the winner", exact: true });
  await expect(picker).toContainText("Each variant's link clicks are in the report below.");
  await expect(page.getByText("No engagement data")).toHaveCount(0);
  // "Below" is true: the report follows the picker on the page.
  const [pickerBox, reportBox] = await Promise.all([
    picker.boundingBox(),
    page.getByRole("region", { name: "Report", exact: true }).boundingBox(),
  ]);
  expect(reportBox!.y).toBeGreaterThan(pickerBox!.y);
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
  await expect(page.getByText("link clicks are in the report below")).toHaveCount(0);
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
  await test_.getByRole("button", { name: "Send test", exact: true }).click();
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

test("one_failed_poll_keeps_the_sending_view_and_an_open_dialog_and_says_the_refresh_failed", async ({ page, app }) => {
  const c = live();
  const log = await open(page, app, source(c), c);
  await button(page, "Cancel campaign").click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("Nothing more will be sent");

  // The next poll fails once; the ones after it succeed.
  await page.route(
    new RegExp(`/campaigns/${c.id}$`),
    (route) => route.fulfill({ status: 503, contentType: "application/problem+json", body: "{}" }),
    { times: 1 },
  );
  const failed = page.waitForResponse((r) => r.status() === 503 && r.url().endsWith(`/campaigns/${c.id}`));
  await failed;
  // Wait for the screen to have shown that the refresh failed, so what follows is read after the
  // failure has been drawn and not before it.
  const line = page.getByText("The latest refresh failed. Showing the last loaded view.");
  await expect(line).toBeVisible();
  await expect(dialog).toBeVisible();
  expect(await page.getByText("The campaign could not be loaded.").count()).toBe(0);
  await expect(page.getByRole("region", { name: "Progress", exact: true })).toBeAttached();

  // The next poll succeeds: the line goes and the dialog was never closed.
  await expect(line).toHaveCount(0);
  await expect(dialog).toBeVisible();
  expect(detailGets(log, c)).toBeGreaterThanOrEqual(2); // the load, then a poll that worked (the failed one never reached the stub)
});

test("a_refusal_from_one_verb_is_not_shown_in_the_next_dialog_or_after_it_closes", async ({ page, app }) => {
  const c = live({ state: "paused" });
  const src = source(c);
  await open(page, app, src, c);
  // Cancelled elsewhere, so Resume is refused; the screen still shows the campaign paused.
  await src.cancel(ACME, c.id);
  await button(page, "Resume").click();
  await expect(page.getByRole("alert")).toContainText(`campaign ${c.id} is cancelled`);

  await button(page, "Cancel campaign").click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("Nothing more will be sent");
  expect(await dialog.getByRole("alert").count()).toBe(0);

  await dialog.getByRole("button", { name: "Keep campaign", exact: true }).click();
  await expect(dialog).toHaveCount(0);
  expect(await page.getByRole("alert").count()).toBe(0);
});
