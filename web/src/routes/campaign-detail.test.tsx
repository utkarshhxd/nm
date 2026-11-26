// SPDX-License-Identifier: AGPL-3.0-only

// The campaign editor in a real browser, on the in-memory source (see campaigns.test.tsx for why
// the seed travels in `window.__nmCampaignSeed`). W3 moves these to stubbing the real API.

import { type Page, expect } from "@playwright/test";

import { test } from "../../e2e/support/app";
import type { components } from "../api/schema";
import { blankCampaign, type MockState } from "./campaign-source";
import type { Campaign } from "./campaign-model";

type Me = components["schemas"]["MeResponse"];
type Role = Me["workspaces"][number]["role"];

const ACME = "11111111-1111-4111-8111-111111111111";
const BETA = "33333333-3333-4333-8333-333333333333";
const T1 = "aaaaaaaa-0000-4000-8000-000000000001";
const T2 = "aaaaaaaa-0000-4000-8000-000000000002";

const template = (id: string, name: string) => ({
  id,
  name,
  kind: "blocks" as const,
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
});
const OPTIONS: MockState["options"] = {
  lists: [
    { id: "bbbbbbbb-0000-4000-8000-000000000001", name: "Customers" },
    { id: "bbbbbbbb-0000-4000-8000-000000000002", name: "Newsletter" },
  ],
  segments: [{ id: "cccccccc-0000-4000-8000-000000000001", name: "Active in 30 days" }],
  sendingDomains: [{ id: "dddddddd-0000-4000-8000-000000000001", name: "mail.acme.test" }],
  templates: [template(T1, "Welcome"), template(T2, "Promo")],
};
const DOMAIN = OPTIONS.sendingDomains[0]!.id;
const VARIANT = { subject: "Hello", template_id: T1, from_name: null };

const draft = (over: Partial<Campaign> = {}) => blankCampaign(ACME, { name: "Spring sale", ...over });

async function open(page: Page, app: string, c: Campaign, role: Role = "editor", more: Partial<MockState> = {}) {
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
  await page.addInitScript((s) => {
    (window as unknown as { __nmCampaignSeed: unknown }).__nmCampaignSeed = s;
  }, { campaigns: [c], options: OPTIONS, ...more });
  await page.goto(`${app}/w/acme/campaigns/${c.id}`);
  await expect(page.getByRole("heading", { name: c.name, level: 2 })).toBeVisible();
}

const region = (page: Page, name: string) => page.getByRole("region", { name, exact: true });
const problem = (status: number, code: string, detail: string, errors?: unknown[]) => ({
  type: `urn:nm:problem:${code}`,
  title: "Error",
  status,
  detail,
  ...(errors ? { errors } : {}),
});

test("each_section_saves_through_update_and_the_values_survive_leaving_the_screen", async ({ page, app }) => {
  const c = draft();
  await open(page, app, c);

  const audience = region(page, "Audience");
  await audience.getByLabel("Customers").check();
  await audience.getByLabel("Segment").selectOption({ label: "Active in 30 days" });
  await audience.getByRole("button", { name: "Save audience" }).click();
  await expect(audience.getByRole("status")).toHaveText("Saved.");

  const content = region(page, "Content");
  await content.getByLabel("Variant 1 subject").fill("Spring is here");
  await content.getByLabel("Variant 1 template").selectOption({ label: "Promo" });
  await content.getByLabel("Variant 1 from name").fill("The Acme team");
  await content.getByRole("button", { name: "Save content" }).click();
  await expect(content.getByRole("status")).toHaveText("Saved.");

  const sender = region(page, "Sender");
  await sender.getByLabel("Domain").selectOption({ label: "mail.acme.test" });
  await sender.getByLabel("Local part").fill("hello");
  await sender.getByLabel("Sender name", { exact: true }).fill("Acme News");
  await sender.getByLabel("Reply-to").fill("support@acme.test");
  await sender.getByRole("button", { name: "Save sender" }).click();
  await expect(sender.getByRole("status")).toHaveText("Saved.");

  // Away to the list and back: the screen reads what the source now holds, not what was typed.
  await page.getByRole("link", { name: "Campaigns", exact: true }).click();
  await page.getByRole("link", { name: "Spring sale", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Spring sale", level: 2 })).toBeVisible();
  await expect(audience.getByLabel("Customers")).toBeChecked();
  await expect(audience.getByLabel("Newsletter")).not.toBeChecked();
  await expect(audience.getByLabel("Segment")).toHaveValue("cccccccc-0000-4000-8000-000000000001");
  await expect(content.getByLabel("Variant 1 subject")).toHaveValue("Spring is here");
  await expect(content.getByLabel("Variant 1 template")).toHaveValue(T2);
  await expect(content.getByLabel("Variant 1 from name")).toHaveValue("The Acme team");
  await expect(sender.getByLabel("Domain")).toHaveValue(DOMAIN);
  await expect(sender.getByLabel("Local part")).toHaveValue("hello");
  await expect(sender.getByLabel("Sender name", { exact: true })).toHaveValue("Acme News");
  await expect(sender.getByLabel("Reply-to")).toHaveValue("support@acme.test");
});

test("a_second_variant_reveals_the_percentage_and_disables_recipient_local_with_its_reason", async ({ page, app }) => {
  await open(page, app, draft({ variants: [VARIANT] }), "admin");
  const content = region(page, "Content");
  const schedule = region(page, "Schedule");
  const local = schedule.getByLabel("Each recipient's local time");
  await expect(local).toBeEnabled();
  expect(await content.getByLabel("A/B test percentage").count()).toBe(0);

  await content.getByRole("button", { name: "Add variant" }).click();
  await expect(content.getByLabel("A/B test percentage")).toBeVisible();
  await expect(local).toBeDisabled();
  await expect(schedule).toContainText("A split test sends at one moment for everyone");

  await content.getByRole("button", { name: "Remove variant 2" }).click();
  await expect(local).toBeEnabled();
  expect(await content.getByLabel("A/B test percentage").count()).toBe(0);
});

test("the_send_dialog_shows_the_estimate_the_resolved_time_and_the_from", async ({ page, app }) => {
  const c = draft({
    variants: [VARIANT],
    sending_domain_id: DOMAIN,
    from_local_part: "hello",
    from_name: "Acme News",
  });
  await open(page, app, c, "admin");
  const schedule = region(page, "Schedule");
  await schedule.getByLabel("Send at", { exact: true }).check();
  await schedule.getByLabel("Date and time").fill("2027-07-01T09:30");
  await schedule.getByLabel("Zone").fill("Europe/London");

  await page.getByRole("button", { name: "Send", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Send campaign" });
  await expect(dialog).toContainText("1,234 recipients");
  await expect(dialog).toContainText("2027-07-01 09:30 Europe/London");
  await expect(dialog).toContainText("1 Jul 2027, 08:30 UTC"); // BST is an hour ahead of UTC
  await expect(dialog).toContainText("Acme News <hello@mail.acme.test>");

  await dialog.getByRole("button", { name: "Schedule send" }).click();
  await expect(page.getByText("Scheduled")).toBeVisible();
  // Scheduled: the editor is gone, there is nothing left to save.
  await expect(page.getByRole("button", { name: "Save audience" })).toHaveCount(0);
});

test("an_estimate_that_cannot_be_counted_quickly_says_so_in_the_editor_and_the_dialog", async ({ page, app }) => {
  const c = draft({ variants: [VARIANT], sending_domain_id: DOMAIN, from_local_part: "hello" });
  await open(page, app, c, "admin", { estimate: { count: null, exact: false } });
  await expect(region(page, "Audience")).toContainText("Too many to count quickly");
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect(page.getByRole("dialog", { name: "Send campaign" })).toContainText("Too many to count quickly");
});

for (const [code, link, href] of [
  ["postal-address-required", "Open the brand kit", "/w/acme/brand-kit"],
  ["sending-domain-unverified", "Open sending domains", "/w/acme/sending-domains"],
  ["template-stale", "Open template Welcome", `/w/acme/templates/${T1}`],
] as const) {
  test(`a_409_${code.replaceAll("-", "_")}_shows_its_detail_and_a_link_to_fix_it`, async ({ page, app }) => {
    const sentence = `The server's own sentence for ${code}.`;
    const c = draft({ variants: [VARIANT], sending_domain_id: DOMAIN, from_local_part: "hello" });
    await open(page, app, c, "admin", { scheduleRefusal: problem(409, code, sentence) });
    await page.getByRole("button", { name: "Send", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "Send campaign" });
    await dialog.getByRole("button", { name: "Send now" }).click();
    await expect(dialog.getByRole("alert")).toHaveText(sentence);
    await expect(dialog.getByRole("link", { name: link })).toHaveAttribute("href", href);
    await expect(dialog).toBeVisible(); // still a draft, still open
  });
}

test("a_422_on_a_variants_template_lands_on_that_variants_picker", async ({ page, app }) => {
  await open(page, app, draft({ variants: [VARIANT, { subject: "B", template_id: "", from_name: null }] }));
  const content = region(page, "Content");
  await content.getByRole("button", { name: "Save content" }).click();
  const second = content.getByRole("group", { name: "Variant 2" });
  await expect(second.getByRole("alert")).toHaveText("That template does not exist.");
  await expect(second.getByLabel("Variant 2 template")).toHaveAttribute("aria-invalid", "true");
  // One look at the first variant, now that the error is on screen: it has none.
  expect(await content.getByRole("group", { name: "Variant 1" }).getByRole("alert").count()).toBe(0);
});

test("a_422_on_the_schedule_time_lands_on_the_time_field_and_closes_the_dialog", async ({ page, app }) => {
  const sentence = "That time has already passed.";
  const c = draft({ variants: [VARIANT], sending_domain_id: DOMAIN, from_local_part: "hello" });
  await open(page, app, c, "admin", {
    scheduleRefusal: problem(422, "validation", "request validation failed", [
      { field: "schedule.local", message: sentence },
    ]),
  });
  const schedule = region(page, "Schedule");
  await schedule.getByLabel("Send at", { exact: true }).check();
  await schedule.getByLabel("Date and time").fill("2020-01-01T09:30");
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await page.getByRole("dialog", { name: "Send campaign" }).getByRole("button", { name: "Schedule send" }).click();
  await expect(schedule.getByRole("alert")).toHaveText(sentence);
  await expect(schedule.getByLabel("Date and time")).toHaveAttribute("aria-invalid", "true");
  await expect(page.getByRole("dialog")).toHaveCount(0);
});

test("an_editor_sees_send_test_but_no_send_and_a_viewer_neither", async ({ page, app }) => {
  const c = draft({ variants: [VARIANT] });
  await open(page, app, c, "editor");
  await expect(page.getByRole("button", { name: "Send test" })).toBeVisible();
  expect(await page.getByRole("button", { name: "Send", exact: true }).count()).toBe(0);

  await open(page, app, c, "admin");
  await expect(page.getByRole("button", { name: "Send", exact: true })).toBeVisible();

  await open(page, app, c, "viewer");
  await expect(region(page, "Preview")).toBeVisible();
  expect(await page.getByRole("button", { name: /^Send/ }).count()).toBe(0);
  expect(await page.getByRole("button", { name: /^Save/ }).count()).toBe(0);
  await expect(region(page, "Audience").getByLabel("Customers")).toBeDisabled();
});

test("send_waits_for_unsaved_edits_to_be_saved", async ({ page, app }) => {
  await open(page, app, draft({ variants: [VARIANT], sending_domain_id: DOMAIN, from_local_part: "hello" }), "admin");
  const send = page.getByRole("button", { name: "Send", exact: true });
  await expect(send).toBeEnabled();
  const content = region(page, "Content");
  await content.getByLabel("Variant 1 subject").fill("Changed");
  await expect(send).toBeDisabled();
  await expect(page.getByText("Save your changes before sending.")).toBeVisible();
  await content.getByRole("button", { name: "Save content" }).click();
  await expect(send).toBeEnabled();
});

test("a_test_send_takes_one_to_five_addresses_and_shows_the_servers_sentence_otherwise", async ({ page, app }) => {
  await open(page, app, draft({ variants: [VARIANT] }));
  const test_ = region(page, "Test send");
  await test_.getByLabel("Addresses").fill("a@example.com,b@example.com\nc@example.com;d@example.com");
  await test_.getByRole("button", { name: "Send test" }).click();
  await expect(test_.getByRole("status")).toHaveText("Test queued to 4 addresses.");

  await test_.getByLabel("Addresses").fill("1@x.test 2@x.test 3@x.test 4@x.test 5@x.test 6@x.test");
  await test_.getByRole("button", { name: "Send test" }).click();
  await expect(test_.getByRole("alert")).toHaveText("Send a test to between 1 and 5 addresses.");
  await expect(test_.getByLabel("Addresses")).toHaveAttribute("aria-invalid", "true");
});

test("the_preview_is_a_sandboxed_frame_with_a_plain_text_tab", async ({ page, app }) => {
  await open(page, app, draft({ variants: [{ ...VARIANT, subject: "Hello there" }] }));
  const preview = region(page, "Preview");
  const frame = preview.locator("iframe");
  await expect(frame).toHaveAttribute("sandbox", ""); // nothing in an email runs here
  await expect(frame).toHaveAttribute("srcdoc", /Hello there/);
  await preview.getByRole("button", { name: "Plain text" }).click();
  await expect(preview.getByText("Hello there", { exact: true })).toBeVisible();
  await expect(frame).toHaveCount(0);
});

test("a_campaign_that_is_not_a_draft_shows_its_state_and_no_editor", async ({ page, app }) => {
  await open(page, app, draft({ variants: [VARIANT], state: "scheduled" }), "admin");
  await expect(page.getByText("Scheduled")).toBeVisible();
  expect(await page.getByRole("region", { name: "Audience" }).count()).toBe(0);
  expect(await page.getByRole("button", { name: "Send", exact: true }).count()).toBe(0);
});

for (const [name, refusal, sentence] of [
  ["a_409_campaign_state", problem(409, "campaign-state", "campaign is scheduled; it must be draft"), "campaign is scheduled; it must be draft"],
  ["a_429", problem(429, "rate-limited", "Too many requests. Try again in a minute."), "Too many requests. Try again in a minute."],
  // A 422 that names a field the editor does not have must still be seen.
  ["a_422_on_a_field_the_editor_does_not_show", problem(422, "validation", "request validation failed", [{ field: "name", message: "A campaign needs a name." }]), "A campaign needs a name."],
] as const) {
  test(`a_save_refused_with_${name}_shows_the_servers_sentence_by_the_save_button`, async ({ page, app }) => {
    await open(page, app, draft({ variants: [VARIANT] }), "editor", { updateRefusal: refusal });
    const content = region(page, "Content");
    await content.getByRole("button", { name: "Save content" }).click();
    await expect(content.getByRole("alert")).toHaveText(sentence);
    await expect(content.getByRole("status")).toHaveText(""); // and no "Saved."
  });
}

test("a_422_on_a_non_schedule_field_from_send_closes_the_dialog_onto_that_field", async ({ page, app }) => {
  const c = draft({ variants: [VARIANT] });
  await open(page, app, c, "admin", {
    scheduleRefusal: problem(422, "validation", "request validation failed", [
      { field: "sending_domain_id", message: "Choose a sending domain." },
    ]),
  });
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await page.getByRole("dialog", { name: "Send campaign" }).getByRole("button", { name: "Send now" }).click();
  await expect(region(page, "Sender").getByRole("alert")).toHaveText("Choose a sending domain.");
  await expect(page.getByRole("dialog")).toHaveCount(0);
});

test("a_409_on_test_send_carries_the_same_fix_link_as_the_editors_409s", async ({ page, app }) => {
  await open(page, app, draft({ variants: [VARIANT] }), "editor", {
    testRefusal: problem(409, "postal-address-required", "Set a postal address first."),
  });
  const test_ = region(page, "Test send");
  await test_.getByLabel("Addresses").fill("a@example.com");
  await test_.getByRole("button", { name: "Send test" }).click();
  await expect(test_.getByRole("alert")).toHaveText("Set a postal address first.");
  await expect(test_.getByRole("link", { name: "Open the brand kit" })).toHaveAttribute("href", "/w/acme/brand-kit");
});
