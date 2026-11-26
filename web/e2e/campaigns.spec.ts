// SPDX-License-Identifier: AGPL-3.0-only

import { type BrowserContext, type Locator, type Page, expect, test } from "@playwright/test";

import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/**
 * The campaign screens against the real API (task W3).
 *
 * What these pin is what a stub cannot: the draft the screens save is the draft the server stores;
 * the schedule is resolved by the server in the workspace's zone and shown as it answers; a
 * refusal is the server's own sentence and the campaign stays what it was; a viewer is offered
 * nothing to press; and another workspace's campaign is a 404 and never its data.
 *
 * One workspace with an admin and a viewer, seeded once and shared in order.
 */
test.describe.configure({ mode: "serial" });

const SLUG = "campaignco";
const OTHER_SLUG = "othercampco";
const ZONE = "Europe/London";
const VIEWER = "viewer-campaigns@example.com";
const VIEWER_PASSWORD = "viewer-campaigns-password";
const DOMAIN = "mail.campaignco.test";
const ADDRESS = "1 Main Street, Springfield";

let admin: BrowserContext;
let viewer: BrowserContext;
let page: Page;
let viewerPage: Page;
let workspaceId = "";
let otherWorkspaceId = "";
let csrf = "";
let campaignId = "";
let templateId = "";
let listId = "";
let domainId = "";

const base = (ws = workspaceId) => `/api/v1/workspaces/${ws}`;

async function send(method: "post" | "put" | "patch", path: string, data?: unknown, ws = workspaceId) {
  const res = await admin.request[method](`${base(ws)}${path}`, { data, headers: { "X-CSRF-Token": csrf } });
  expect(res.ok(), `${method} ${path} -> ${res.status()} ${await res.text()}`).toBe(true);
  return res;
}

type Campaign = {
  id: string;
  state: string;
  list_ids: string[];
  from_local_part: string | null;
  variants: { subject: string; template_id: string }[];
  schedule: { mode: string; zone: string; local: string | null; send_at: string | null } | null;
};
const apiCampaign = async (id = campaignId, ws = workspaceId): Promise<Campaign> =>
  (await (await admin.request.get(`${base(ws)}/campaigns/${id}`)).json()) as Campaign;

async function signIn(p: Page, email: string, password: string): Promise<void> {
  await p.goto("/login");
  await p.getByLabel("Email", { exact: true }).fill(email);
  await p.getByLabel("Password", { exact: true }).fill(password);
  await p.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(p.getByText(`Signed in as ${email}`)).toBeVisible();
}

const region = (p: Page, name: string) => p.getByRole("region", { name, exact: true });
const button = (p: Page | Locator, name: string) => p.getByRole("button", { name, exact: true });
/** The state pill: a rounded span, so "Scheduled" never matches the sentence beside it. */
const badge = (p: Page, name: string) => p.locator("span.rounded-full").filter({ hasText: name });

const openCampaign = async (p: Page = page): Promise<void> => {
  await p.goto(`/w/${SLUG}/campaigns/${campaignId}`);
  await expect(p.getByRole("heading", { name: "Autumn push", level: 2 })).toBeVisible();
};

/** Tomorrow's date in the workspace's zone, and the UTC instant 09:30 there is (a DST change is at night, so noon's offset is 09:30's). */
function tomorrowAt0930(): { date: string; local: string; utc: Date } {
  const zoned = (ms: number, extra: Intl.DateTimeFormatOptions) =>
    new Intl.DateTimeFormat("en-GB", { timeZone: ZONE, ...extra }).formatToParts(ms);
  const day = zoned(Date.now() + 86_400_000, { year: "numeric", month: "2-digit", day: "2-digit" });
  const part = (t: string) => day.find((p) => p.type === t)?.value ?? "";
  const date = `${part("year")}-${part("month")}-${part("day")}`;
  const noon = Date.parse(`${date}T12:00:00Z`);
  const offset = zoned(noon, { timeZoneName: "shortOffset" }).find((p) => p.type === "timeZoneName")?.value ?? "GMT";
  const hours = offset === "GMT" ? 0 : Number(offset.replace("GMT", ""));
  return { date, local: `${date}T09:30`, utc: new Date(Date.parse(`${date}T09:30:00Z`) - hours * 3_600_000) };
}

test.beforeAll(async ({ browser }) => {
  test.setTimeout(180_000);
  admin = await browser.newContext();
  viewer = await browser.newContext();
  page = await admin.newPage();
  viewerPage = await viewer.newPage();

  await signIn(page, ADMIN_EMAIL, ADMIN_PASSWORD);
  for (const [slug, name] of [
    [SLUG, "Campaign Co"],
    [OTHER_SLUG, "Other Campaign Co"],
  ] as const) {
    await page.goto("/workspaces/new");
    await page.getByLabel("Slug", { exact: true }).fill(slug);
    await page.getByLabel("Workspace name", { exact: true }).fill(name);
    await page.getByLabel("Time zone", { exact: true }).selectOption(ZONE);
    await page.getByRole("button", { name: "Create workspace", exact: true }).click();
    await expect(page).toHaveURL(new RegExp(`/w/${slug}/members$`));
  }
  const me = (await (await admin.request.get("/api/v1/me")).json()) as {
    csrf_token: string;
    workspaces: { id: string; slug: string }[];
  };
  csrf = me.csrf_token;
  workspaceId = me.workspaces.find((w) => w.slug === SLUG)?.id ?? "";
  otherWorkspaceId = me.workspaces.find((w) => w.slug === OTHER_SLUG)?.id ?? "";
  expect(workspaceId).not.toBe("");
  expect(otherWorkspaceId).not.toBe("");

  // The viewer joins by invitation.
  const invite = (await (await send("post", "/invites", { email: VIEWER, role: "viewer" })).json()) as {
    invite_url: string;
  };
  await viewerPage.goto(new URL(invite.invite_url).pathname);
  await viewerPage.getByLabel("Your name", { exact: true }).fill("viewer campaigns");
  await viewerPage.getByLabel("Password", { exact: true }).fill(VIEWER_PASSWORD);
  await viewerPage.getByRole("button", { name: "Accept", exact: true }).click();
  await expect(viewerPage.getByText(`Signed in as ${VIEWER}`)).toBeVisible();

  // What a campaign needs: a list, a template and a sending domain an admin has accepted. The
  // brand kit's postal address is added by the test that is about its absence.
  listId = ((await (await send("post", "/lists", { name: "Newsletter", double_optin: false })).json()) as { id: string }).id;
  templateId = (
    (await (
      await send("post", "/templates", {
        name: "Welcome",
        kind: "blocks",
        tree: { blocks: [{ type: "Text", runs: [{ type: "Text", text: "Hello there", marks: [] }] }] },
      })
    ).json()) as { id: string }
  ).id;
  domainId = ((await (await send("post", "/sending-domains", { domain: DOMAIN, signing: "relay" })).json()) as { id: string }).id;
  await send("patch", `/sending-domains/${domainId}`, { send_unverified_acknowledged: true });
});

test.afterAll(async () => {
  await admin.close();
  await viewer.close();
});

test("a_campaign_is_made_and_filled_in_through_the_screens_and_the_server_holds_what_was_saved", async () => {
  await page.goto(`/w/${SLUG}/campaigns`);
  await expect(page.getByText("No campaigns yet")).toBeVisible();
  await button(page, "New campaign").click();
  await page.getByRole("dialog").getByLabel("Name", { exact: true }).fill("Autumn push");
  await button(page, "Create campaign").click();
  await expect(page).toHaveURL(new RegExp(`/w/${SLUG}/campaigns/[0-9a-f-]{36}$`));
  campaignId = page.url().split("/").pop() ?? "";
  await expect(page.getByRole("heading", { name: "Autumn push", level: 2 })).toBeVisible();

  const audience = region(page, "Audience");
  await audience.getByLabel("Newsletter").check();
  await button(audience, "Save audience").click();
  await expect(audience.getByRole("status")).toHaveText("Saved.");

  const content = region(page, "Content");
  await content.getByLabel("Variant 1 subject").fill("Autumn is here");
  await content.getByLabel("Variant 1 template").selectOption({ label: "Welcome" });
  await button(content, "Save content").click();
  await expect(content.getByRole("status")).toHaveText("Saved.");

  const sender = region(page, "Sender");
  await sender.getByLabel("Domain").selectOption({ label: DOMAIN });
  await sender.getByLabel("Local part").fill("news");
  await sender.getByLabel("Sender name", { exact: true }).fill("Campaign Co");
  await button(sender, "Save sender").click();
  await expect(sender.getByRole("status")).toHaveText("Saved.");

  // The server's copy, not the screen's.
  const stored = await apiCampaign();
  expect(stored.state).toBe("draft");
  expect(stored.list_ids).toEqual([listId]);
  expect(stored.variants).toMatchObject([{ subject: "Autumn is here", template_id: templateId }]);
  expect(stored.from_local_part).toBe("news");

  // And it comes back into a fresh screen the same way round.
  await openCampaign();
  await expect(region(page, "Audience").getByLabel("Newsletter")).toBeChecked();
  await expect(region(page, "Content").getByLabel("Variant 1 subject")).toHaveValue("Autumn is here");
  await expect(region(page, "Sender").getByLabel("Local part")).toHaveValue("news");
});

test("scheduling_with_no_postal_address_shows_the_servers_refusal_and_the_campaign_stays_a_draft", async () => {
  // The refusal the server writes for this exact request, so the screen is held to its words.
  const refused = await admin.request.post(`${base()}/campaigns/${campaignId}/schedule`, {
    data: { mode: "instant", zone: ZONE, local: null },
    headers: { "X-CSRF-Token": csrf },
  });
  expect(refused.status()).toBe(409);
  const sentence = ((await refused.json()) as { detail: string }).detail;
  expect(sentence).not.toBe("");

  await openCampaign();
  await button(page, "Send").click();
  const dialog = page.getByRole("dialog");
  await dialog.getByRole("button", { name: "Send now", exact: true }).click();
  await expect(dialog.getByRole("alert")).toContainText(sentence);
  await expect(dialog.getByRole("link", { name: "Open the brand kit" })).toBeVisible();
  expect((await apiCampaign()).state).toBe("draft");
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(badge(page, "Draft")).toBeVisible();
});

test("a_test_send_is_accepted_and_says_how_many_addresses_it_was_queued_to", async () => {
  await send("put", "/brand-kit", { logo_url: null, palette: {}, font_stack: null, footer: { address: ADDRESS } });
  await openCampaign();
  const test_ = region(page, "Test send");
  await test_.getByLabel("Addresses").fill("qa@example.test");
  await button(test_, "Send test").click();
  await expect(test_.getByRole("status")).toHaveText("Test queued to 1 address.");

  // One to five: the server counts, and its sentence is shown.
  await test_.getByLabel("Addresses").fill("a@example.test b@example.test c@example.test d@example.test e@example.test f@example.test");
  await button(test_, "Send test").click();
  await expect(test_.getByRole("alert")).toBeVisible();
});

test("scheduling_for_tomorrow_shows_scheduled_with_the_time_the_server_resolved_and_unscheduling_returns_it_to_a_draft", async () => {
  const when = tomorrowAt0930();
  await openCampaign();
  const schedule = region(page, "Schedule");
  await schedule.getByLabel("Send at").check();
  // The default zone is the workspace's own, not UTC.
  await expect(schedule.getByLabel("Zone")).toHaveValue(ZONE);
  await schedule.getByLabel("Date and time").fill(when.local);

  await button(page, "Send").click();
  const dialog = page.getByRole("dialog");
  await expect(dialog).toContainText("recipients");
  await expect(dialog).toContainText("Campaign Co <news@" + DOMAIN + ">");
  await dialog.getByRole("button", { name: "Schedule send", exact: true }).click();

  await expect(badge(page, "Scheduled")).toBeVisible();
  const stored = await apiCampaign();
  expect(stored.state).toBe("scheduled");
  expect(stored.schedule).toMatchObject({ mode: "instant", zone: ZONE, local: when.local });
  // What the screen shows is what the server resolved, and the server resolved the right instant.
  expect(new Date(stored.schedule?.send_at ?? "").toISOString()).toBe(when.utc.toISOString());
  const shown = new Intl.DateTimeFormat("en-GB", { dateStyle: "medium", timeStyle: "short", timeZone: "UTC" }).format(when.utc);
  await expect(page.getByText(`Send at ${shown} UTC`)).toBeVisible();
  await expect(page.getByText(`Set for ${when.date} 09:30 ${ZONE}`)).toBeVisible();

  // The list says so too.
  await page.getByRole("link", { name: "Campaigns", exact: true }).click();
  await expect(page.getByRole("row", { name: /Autumn push/ })).toContainText("Scheduled");

  await page.getByRole("link", { name: "Autumn push", exact: true }).click();
  await button(page, "Unschedule").click();
  await expect(button(page, "Save audience")).toBeVisible();
  await expect(badge(page, "Draft")).toBeVisible();
  const back = await apiCampaign();
  expect(back.state).toBe("draft");
  expect(back.schedule).toBeNull();
});

test("a_viewer_can_read_a_campaign_in_every_state_and_is_offered_nothing_to_press", async () => {
  await viewerPage.goto(`/w/${SLUG}/campaigns`);
  await expect(viewerPage.getByRole("link", { name: "Autumn push", exact: true })).toBeVisible();
  await expect(button(viewerPage, "New campaign")).toHaveCount(0);

  const none = ["Send", "Save audience", "Save content", "Save sender", "Send test", "Unschedule", "Pause", "Resume", "Cancel campaign"];
  await openCampaign(viewerPage);
  for (const name of none) await expect(button(viewerPage, name)).toHaveCount(0);
  await expect(viewerPage.getByText("Preview", { exact: true })).toBeVisible();

  // Scheduled: still nothing to press, and still readable.
  await send("post", `/campaigns/${campaignId}/schedule`, { mode: "instant", zone: ZONE, local: tomorrowAt0930().local });
  await openCampaign(viewerPage);
  await expect(badge(viewerPage, "Scheduled")).toBeVisible();
  for (const name of none) await expect(button(viewerPage, name)).toHaveCount(0);
});

test("another_workspaces_campaign_is_not_found_here_and_never_arrives", async () => {
  const made = await send("post", "/campaigns", { name: "Secret of the other tenant" }, otherWorkspaceId);
  const other = (await made.json()) as { id: string };

  // The same id, asked for under THIS workspace: 404, never 403 and never the row.
  const refused = await admin.request.get(`${base()}/campaigns/${other.id}`);
  expect(refused.status()).toBe(404);
  expect(await refused.text()).not.toContain("Secret of the other tenant");

  await page.goto(`/w/${SLUG}/campaigns/${other.id}`);
  await expect(page.getByText("This campaign no longer exists.")).toBeVisible();
  await expect(page.getByText("Secret of the other tenant")).toHaveCount(0);

  // And it is not in this workspace's list.
  await page.goto(`/w/${SLUG}/campaigns`);
  await expect(page.getByRole("link", { name: "Autumn push", exact: true })).toBeVisible();
  await expect(page.getByText("Secret of the other tenant")).toHaveCount(0);
});
