// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, type Page, expect, test } from "@playwright/test";

import { expectNoViolations } from "./support/axe";
import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/**
 * One small workspace, seeded once through the API and shared by the tests. Every test but the
 * last only reads it; `the_list_editor_sets_a_lists_visibility` creates a list of its own rather
 * than editing one the earlier tests count.
 */
test.describe.configure({ mode: "serial" });

const SLUG = "detailco";

let context: BrowserContext;
let page: Page;
let workspaceId = "";
let csrf = "";

const ids = { ada: "", bounced: "", blocked: "", single: "", double: "" };

async function post(path: string, data: unknown, scoped = true): Promise<{ id: string }> {
  const url = scoped ? `/api/v1/workspaces/${workspaceId}${path}` : `/api/v1${path}`;
  const res = await context.request.post(url, { data, headers: { "X-CSRF-Token": csrf } });
  expect(res.ok(), `post ${path} -> ${res.status()}`).toBe(true);
  return (await res.json()) as { id: string };
}

async function put(path: string): Promise<void> {
  const res = await context.request.put(`/api/v1/workspaces/${workspaceId}${path}`, {
    headers: { "X-CSRF-Token": csrf },
  });
  expect(res.ok(), `put ${path} -> ${res.status()}`).toBe(true);
}

test.beforeAll(async ({ browser }) => {
  test.setTimeout(120_000);
  context = await browser.newContext();
  page = await context.newPage();
  await page.goto("/login");
  await page.getByLabel("Email", { exact: true }).fill(ADMIN_EMAIL);
  await page.getByLabel("Password", { exact: true }).fill(ADMIN_PASSWORD);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(page.getByText(`Signed in as ${ADMIN_EMAIL}`)).toBeVisible();

  await page.goto("/workspaces/new");
  await page.getByLabel("Slug", { exact: true }).fill(SLUG);
  await page.getByLabel("Workspace name", { exact: true }).fill("Detail Co");
  await page.getByLabel("Time zone", { exact: true }).selectOption("Europe/London");
  await page.getByRole("button", { name: "Create workspace", exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/w/${SLUG}/members$`));

  const me = (await (await context.request.get("/api/v1/me")).json()) as {
    csrf_token: string;
    workspaces: { id: string; slug: string }[];
  };
  csrf = me.csrf_token;
  workspaceId = me.workspaces.find((w) => w.slug === SLUG)?.id ?? "";
  expect(workspaceId).not.toBe("");

  const field = (key: string, label: string, kind: string, options?: string[]) =>
    post("/custom-fields", { key, label, kind, options });
  await field("company", "Company", "text");
  await field("seats", "Seats", "number");
  await field("renewal", "Renewal date", "date");
  await field("signed_at", "Signed at", "datetime");
  await field("champion", "Champion", "boolean");
  await field("plan", "Plan", "select", ["Free", "Pro"]);
  await field("interests", "Interests", "multi_select", ["Email", "SMS", "Push"]);

  const vip = (await post("/tags", { name: "vip" })).id;
  ids.single = (await post("/lists", { name: "Announcements", double_optin: false })).id;
  ids.double = (await post("/lists", { name: "Newsletter", double_optin: true })).id;

  ids.ada = (
    await post("/contacts", {
      email: "ada@example.test",
      first_name: "Ada",
      last_name: "Lovelace",
      timezone: "Europe/London",
      attributes: {
        company: "Acme Ltd",
        seats: 1250,
        renewal: "2026-03-09",
        signed_at: "2026-03-09T14:30:00Z",
        champion: true,
        plan: "Pro",
        interests: ["Email", "SMS"],
      },
    })
  ).id;
  ids.bounced = (await post("/contacts", { email: "bounced@example.test" })).id;
  ids.blocked = (await post("/contacts", { email: "blocked@example.test" })).id;
  const clean = [
    (await post("/contacts", { email: "clean1@example.test" })).id,
    (await post("/contacts", { email: "clean2@example.test" })).id,
  ];

  for (const c of [ids.ada, ids.bounced, ids.blocked, ...clean]) {
    await post(`/lists/${ids.single}/members`, { contact_id: c });
  }
  await post(`/lists/${ids.double}/members`, { contact_id: ids.ada });
  await put(`/contacts/${ids.ada}/tags/${vip}`);

  await post("/suppressions", { scope: "address", email: "bounced@example.test", reason: "bounced" });
  // The block a workspace admin cannot see in the workspace's own suppression list.
  await post(
    "/admin/suppressions",
    { scope: "address", email: "blocked@example.test", reason: "complained" },
    false,
  );
});

test.afterAll(async () => {
  await context.close();
});

const contact = (id: string) => `/w/${SLUG}/contacts/${id}`;
const status = () => page.getByRole("region", { name: "Mailing status", exact: true });
const counting = () => page.getByRole("status").filter({ hasText: "Counting mailable members" });

test("shows_typed_custom_field_values", async () => {
  await page.goto(contact(ids.ada));
  await expect(page.getByRole("heading", { name: "ada@example.test", exact: true })).toBeVisible();
  const fields = page.getByRole("region", { name: "Custom fields", exact: true });
  const value = (label: string) => fields.locator(`dt:text-is("${label}") + dd`);
  await expect(value("Company")).toHaveText("Acme Ltd");
  // A number is formatted as a number, a date as a date, a boolean in words: not raw JSON.
  await expect(value("Seats")).toHaveText("1,250");
  await expect(value("Renewal date")).toHaveText("Mar 9, 2026");
  await expect(value("Signed at")).toHaveText(/Mar 9, 2026.*2:30\s?PM.*GMT/);
  await expect(value("Champion")).toHaveText("Yes");
  await expect(value("Plan")).toHaveText("Pro");
  await expect(value("Interests")).toContainText("Email");
  await expect(value("Interests")).toContainText("SMS");
  await expect(page.getByRole("region", { name: "Tags", exact: true })).toContainText("vip");
});

test("shows_list_memberships_with_status", async () => {
  await page.goto(contact(ids.ada));
  const lists = page.getByRole("region", { name: "Lists", exact: true });
  const row = (name: string) => lists.getByRole("row").filter({ hasText: name });
  await expect(row("Announcements")).toContainText("Subscribed");
  await expect(row("Newsletter")).toContainText("Pending");
  // Pending is not a softer "subscribed": it is not mailable, and nothing confirms it yet.
  await expect(lists).toContainText("not confirmed");
  await expect(lists).toContainText("cannot be mailed");
  await expect(lists).toContainText("nothing sends the confirmation yet");
});

test("explains_why_a_suppressed_contact_is_not_mailable", async () => {
  await page.goto(contact(ids.ada));
  await expect(status()).toContainText("Mailable");
  await expect(status()).not.toContainText("Not mailable");

  await page.goto(contact(ids.bounced));
  await expect(status()).toContainText("Not mailable");
  await expect(status().getByText("Workspace suppression", { exact: true })).toBeVisible();
  await expect(status().getByText("Bounced", { exact: true })).toBeVisible();
});

test("explains_an_instance_wide_block_the_workspace_list_does_not_contain", async () => {
  // The workspace's own suppression list has no row for this address, so a page built on that
  // list would say nothing while the mail silently never went.
  const own = (await (
    await context.request.get(`/api/v1/workspaces/${workspaceId}/suppressions`)
  ).json()) as { data: { email: string }[] };
  expect(own.data.map((s) => s.email)).not.toContain("blocked@example.test");

  await page.goto(contact(ids.blocked));
  await expect(status()).toContainText("Not mailable");
  await expect(status().getByText("Instance-wide suppression", { exact: true })).toBeVisible();
  await expect(status().getByText("Complained", { exact: true })).toBeVisible();
});

test("list_detail_shows_a_mailable_count_after_a_skeleton", async () => {
  // Hold the count back, so the skeleton is observable rather than a race with a fast server.
  let release = () => {};
  const held = new Promise<void>((done) => (release = done));
  await page.route("**/mailable-count", async (route) => {
    await held;
    await route.continue();
  });
  await page.goto(`/w/${SLUG}/lists/${ids.single}`);

  // The page is not blocked on the count: settings and members are already there.
  await expect(page.getByRole("heading", { name: "Announcements", exact: true })).toBeVisible();
  await expect(page.getByText("Single opt-in")).toBeVisible();
  await expect(page.getByRole("row").filter({ hasText: "ada@example.test" })).toBeVisible();
  await expect(counting()).toBeVisible();

  release();
  // 5 subscribed, minus one blocked in the workspace and one blocked instance-wide.
  await expect(page.getByTestId("mailable-count")).toHaveText("3");
  await expect(counting()).toHaveCount(0);
  await page.unroute("**/mailable-count");
});

test("axe_reports_no_violations_on_both_pages", async () => {
  for (const [path, ready] of [
    [contact(ids.blocked), "Instance-wide suppression"],
    [`/w/${SLUG}/lists/${ids.double}`, "Newsletter"],
    [`/w/${SLUG}/lists`, "Announcements"],
  ] as const) {
    await page.goto(path);
    await expect(page.getByText(ready, { exact: true }).first()).toBeVisible();
    // Every skeleton has resolved, so axe sees the page and not its placeholder.
    await expect(page.getByRole("status").filter({ hasText: /^(Loading|Counting)/ })).toHaveCount(0);
    await expectNoViolations(new AxeBuilder({ page }), path);
  }
});

test("enter_on_a_grid_row_opens_the_contact", async () => {
  await page.goto(`/w/${SLUG}/contacts?q=ada@example`);
  const row = page.locator("tbody tr[aria-rowindex]").first();
  await expect(row).toContainText("ada@example.test");
  // The row is the grid's one tab stop; its link is out of the tab order, so Enter must follow it.
  await row.focus();
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(new RegExp(`/contacts/${ids.ada}$`));
  await expect(page.getByRole("heading", { name: "ada@example.test", exact: true })).toBeVisible();
});

/**
 * The consent panel answers "did this person agree, and how?".
 *
 * Ada's two memberships were written by an operator through the API in `beforeAll`: one to a
 * single-opt-in list, which is agreement, and one to a double-opt-in list, which is only a
 * request. Both must be legible as what they are, with the path they arrived by beside them.
 *
 * It does not assert a `confirmed` row, and cannot: the only writer of one is `GET /c/{token}`,
 * the token exists only inside an enqueued `confirmation_email` job, and this harness has no way
 * to read that queue — no database client on PATH and no API or CLI over `jobs`. M1d Task 12's
 * preference-centre spec is where that gap has to be closed.
 */
test("consent_history_says_how_each_membership_was_made", async () => {
  await page.goto(contact(ids.ada));
  const consent = page.getByRole("region", { name: "Consent history", exact: true });
  const log = consent.getByRole("table");
  const row = (list: string) => log.getByRole("row").filter({ hasText: list });

  // The single-opt-in list: subscribed outright, by an operator.
  await expect(row("Announcements")).toContainText("Subscribed");
  await expect(row("Announcements")).toContainText("Operator");
  // The double-opt-in list: ASKED, not agreed. A panel that showed this as "Subscribed" would be
  // asserting consent nobody gave.
  await expect(row("Newsletter")).toContainText("Requested");
  await expect(row("Newsletter")).not.toContainText("Subscribed");
  await expect(consent).toContainText("asked and has not agreed yet");

  // An operator's own request is not the subscriber's evidence, so neither row carries one.
  await expect(log).not.toContainText("IP address");
  await expect(log).not.toContainText("Browser");
});

test("the_list_editor_sets_a_lists_visibility", async () => {
  await page.goto(`/w/${SLUG}/lists`);
  await page.getByRole("button", { name: "New list", exact: true }).click();
  const creating = page.getByRole("dialog", { name: "New list", exact: true });
  await creating.getByLabel("Name", { exact: true }).fill("Topics");
  await creating.getByLabel("Visibility", { exact: true }).selectOption("public");
  await creating.getByRole("button", { name: "Create list", exact: true }).click();

  // The visibility cell itself, not the row: the list's own name would satisfy a row-wide match.
  const row = page.getByRole("row").filter({ hasText: "Topics" });
  await expect(row.getByRole("cell").nth(2)).toHaveText("public");
  await row.getByRole("link", { name: "Topics", exact: true }).click();
  const settings = page.getByRole("region", { name: "Settings", exact: true });
  await expect(settings.locator('dt:text-is("Visibility") + dd')).toHaveText("public");

  // And back again: the control reads the list it is editing, rather than defaulting every time.
  await page.getByRole("button", { name: "Edit list", exact: true }).click();
  const editing = page.getByRole("dialog", { name: "Edit list", exact: true });
  await expect(editing.getByLabel("Visibility", { exact: true })).toHaveValue("public");
  await editing.getByLabel("Visibility", { exact: true }).selectOption("private");
  await editing.getByRole("button", { name: "Save list", exact: true }).click();
  await expect(settings.locator('dt:text-is("Visibility") + dd')).toHaveText("private");
});
