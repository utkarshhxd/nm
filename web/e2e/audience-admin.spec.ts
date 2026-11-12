// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, type Page, expect, test } from "@playwright/test";

import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/** One workspace with an admin and an editor, seeded once over the API and shared in order. */
test.describe.configure({ mode: "serial" });

const SLUG = "audienceco";
const EDITOR = "editor-aud@example.com";
const EDITOR_PASSWORD = "editor-aud-password";

let admin: BrowserContext;
let editor: BrowserContext;
let page: Page;
let editorPage: Page;
let workspaceId = "";
let csrf = "";
const ids = { archiveMe: "", purgeMe: "", ada: "" };

const base = () => `/api/v1/workspaces/${workspaceId}`;

async function post(path: string, data: unknown, scoped = true): Promise<{ id: string }> {
  const res = await admin.request.post(scoped ? `${base()}${path}` : `/api/v1${path}`, {
    data,
    headers: { "X-CSRF-Token": csrf },
  });
  expect(res.ok(), `post ${path} -> ${res.status()}`).toBe(true);
  return (await res.json()) as { id: string };
}

const attributes = async (contactId: string) =>
  ((await (await admin.request.get(`${base()}/contacts/${contactId}`)).json()) as {
    attributes: Record<string, unknown>;
  }).attributes;

test.beforeAll(async ({ browser }) => {
  test.setTimeout(120_000);
  admin = await browser.newContext();
  editor = await browser.newContext();
  page = await admin.newPage();
  editorPage = await editor.newPage();

  await page.goto("/login");
  await page.getByLabel("Email", { exact: true }).fill(ADMIN_EMAIL);
  await page.getByLabel("Password", { exact: true }).fill(ADMIN_PASSWORD);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(page.getByText(`Signed in as ${ADMIN_EMAIL}`)).toBeVisible();

  await page.goto("/workspaces/new");
  await page.getByLabel("Slug", { exact: true }).fill(SLUG);
  await page.getByLabel("Workspace name", { exact: true }).fill("Audience Co");
  await page.getByLabel("Time zone", { exact: true }).selectOption("Europe/London");
  await page.getByRole("button", { name: "Create workspace", exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/w/${SLUG}/members$`));

  const me = (await (await admin.request.get("/api/v1/me")).json()) as {
    csrf_token: string;
    workspaces: { id: string; slug: string }[];
  };
  csrf = me.csrf_token;
  workspaceId = me.workspaces.find((w) => w.slug === SLUG)?.id ?? "";
  expect(workspaceId).not.toBe("");

  // The editor joins by invitation, as a real one would.
  const invite = (await post("/invites", { email: EDITOR, role: "editor" })) as unknown as {
    invite_url: string;
  };
  await editorPage.goto(new URL(invite.invite_url).pathname);
  await editorPage.getByLabel("Your name", { exact: true }).fill("Ed Itor");
  await editorPage.getByLabel("Password", { exact: true }).fill(EDITOR_PASSWORD);
  await editorPage.getByRole("button", { name: "Accept", exact: true }).click();
  await expect(editorPage.getByText(`Signed in as ${EDITOR}`)).toBeVisible();

  ids.archiveMe = (await post("/custom-fields", { key: "region", label: "Region", kind: "text" })).id;
  ids.purgeMe = (await post("/custom-fields", { key: "tier", label: "Tier", kind: "text" })).id;
  ids.ada = (
    await post("/contacts", {
      email: "ada@example.test",
      attributes: { region: "EMEA", tier: "gold" },
    })
  ).id;
  await post("/tags", { name: "vip" });
  await post("/suppressions", { scope: "address", email: "seeded@example.test", reason: "complained" });

  // An erasure leaves a tombstone: the one suppression nobody may lift.
  const erased = await post("/contacts", { email: "erased@example.test" });
  const res = await admin.request.post(`${base()}/contacts/${erased.id}/erase`, {
    headers: { "X-CSRF-Token": csrf },
  });
  expect(res.ok(), `erase -> ${res.status()}`).toBe(true);
});

test.afterAll(async () => {
  await admin.close();
  await editor.close();
});

const fieldsPath = `/w/${SLUG}/custom-fields`;
const suppressionsPath = `/w/${SLUG}/suppressions`;
const row = (p: Page, text: string) => p.getByRole("row").filter({ hasText: text });

test("creating_a_field_offers_every_kind", async () => {
  await page.goto(fieldsPath);
  await page.getByRole("button", { name: "New field", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "New custom field", exact: true });
  const kinds = await dialog.getByLabel("Type", { exact: true }).locator("option").evaluateAll(
    (os) => os.map((o) => (o as HTMLOptionElement).value),
  );
  expect(kinds).toEqual([
    "text",
    "number",
    "date",
    "datetime",
    "boolean",
    "select",
    "multi_select",
  ]);

  await dialog.getByLabel("Label", { exact: true }).fill("Plan");
  await dialog.getByLabel("Key", { exact: true }).fill("plan");
  await dialog.getByLabel("Type", { exact: true }).selectOption("select");
  await dialog.getByLabel("Options", { exact: true }).fill("Free\nPro");
  await dialog.getByRole("button", { name: "Create field", exact: true }).click();
  await expect(row(page, "Plan")).toContainText("Single choice");
  await expect(row(page, "Plan")).toContainText("Free, Pro");
});

test("deleting_a_field_warns_that_values_are_kept", async () => {
  await page.goto(fieldsPath);
  // There is no "Delete": the product archives, and the control says so.
  await expect(page.getByRole("button", { name: /delete/i })).toHaveCount(0);
  await row(page, "Region").getByRole("button", { name: "Archive Region", exact: true }).click();

  const dialog = page.getByRole("dialog", { name: "Archive the field Region", exact: true });
  await expect(dialog).toContainText("archived");
  await expect(dialog).toContainText("Every contact keeps the value it already has");
  await expect(dialog).toContainText("still readable");
  await expect(dialog).toContainText("New values are refused");
  await expect(dialog).not.toContainText(/delet/i);
  await dialog.getByRole("button", { name: "Archive field", exact: true }).click();

  await expect(row(page, "Region")).toContainText("Archived");
  // The promise the copy made: nothing was removed.
  expect((await attributes(ids.ada))["region"]).toBe("EMEA");
});

test("purging_a_field_requires_it_to_be_archived_first", async () => {
  await page.goto(fieldsPath);
  // An active field offers no purge, and the server refuses it too.
  await expect(row(page, "Tier").getByRole("button", { name: /purge/i })).toHaveCount(0);
  const refused = await admin.request.post(`${base()}/custom-fields/${ids.purgeMe}/purge`, {
    headers: { "X-CSRF-Token": csrf },
  });
  expect(refused.status()).toBe(409);
  expect((await attributes(ids.ada))["tier"]).toBe("gold");

  await row(page, "Tier").getByRole("button", { name: "Archive Tier", exact: true }).click();
  await page
    .getByRole("dialog", { name: "Archive the field Tier", exact: true })
    .getByRole("button", { name: "Archive field", exact: true })
    .click();
  await expect(row(page, "Tier")).toContainText("Archived");

  await row(page, "Tier").getByRole("button", { name: "Purge values of Tier", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Purge the values of Tier", exact: true });
  await expect(dialog).toContainText("removes the Tier value from every contact");
  await expect(dialog).toContainText("cannot be undone");
  await dialog.getByRole("button", { name: "Purge values permanently", exact: true }).click();

  await expect(page.getByRole("status").filter({ hasText: "Removed the Tier value from 1 contact" })).toBeVisible();
  expect((await attributes(ids.ada))["tier"]).toBeUndefined();
  // The other field's value was not touched.
  expect((await attributes(ids.ada))["region"]).toBe("EMEA");
});

test("tags_can_be_created_renamed_and_deleted_with_a_warning", async () => {
  await page.goto(`/w/${SLUG}/tags`);
  await page.getByLabel("New tag name", { exact: true }).fill("prospect");
  await page.getByRole("button", { name: "Add tag", exact: true }).click();
  await expect(row(page, "prospect")).toBeVisible();

  await row(page, "prospect").getByRole("button", { name: "Rename prospect", exact: true }).click();
  const rename = page.getByRole("dialog", { name: "Rename the tag prospect", exact: true });
  await rename.getByLabel("Name", { exact: true }).fill("lead");
  await rename.getByRole("button", { name: "Save", exact: true }).click();
  await expect(row(page, "lead")).toBeVisible();

  await row(page, "lead").getByRole("button", { name: "Delete lead", exact: true }).click();
  const del = page.getByRole("dialog", { name: "Delete the tag lead", exact: true });
  // Unlike a field, a tag has no value worth keeping: the copy says it comes off every contact.
  await expect(del).toContainText("removes it from every contact");
  await expect(del).toContainText("The contacts themselves are not deleted");
  await del.getByRole("button", { name: "Delete tag", exact: true }).click();
  await expect(row(page, "lead")).toHaveCount(0);
});

test("adding_a_suppression_then_lifting_it_as_admin", async () => {
  await page.goto(suppressionsPath);
  // A list that looks empty is not proof of anything: the page says so.
  await expect(page.getByText("Instance-wide blocks are not shown here")).toBeVisible();

  await page.getByLabel("Email address", { exact: true }).fill("Opted.Out@Example.test");
  await page.getByLabel("Reason", { exact: true }).selectOption("unsubscribed_all");
  await page.getByRole("button", { name: "Add suppression", exact: true }).click();
  await expect(row(page, "opted.out@example.test")).toContainText("Unsubscribed from everything");

  await row(page, "opted.out@example.test")
    .getByRole("button", { name: "Lift suppression for opted.out@example.test", exact: true })
    .click();
  const dialog = page.getByRole("dialog", {
    name: "Lift the suppression on opted.out@example.test",
    exact: true,
  });
  // It names who, why they were blocked, and what lifting does.
  await expect(dialog).toContainText("opted.out@example.test");
  await expect(dialog).toContainText("Unsubscribed from everything");
  await expect(dialog).toContainText("mailable again");
  await dialog.getByRole("button", { name: "Lift suppression", exact: true }).click();
  await expect(row(page, "opted.out@example.test")).toHaveCount(0);

  await page.getByLabel("Show lifted", { exact: true }).check();
  await expect(row(page, "opted.out@example.test")).toContainText("Lifted");
});

test("an_erasure_tombstone_offers_no_lift_even_to_an_admin", async () => {
  await page.goto(suppressionsPath);
  const tomb = row(page, "Erased on request (GDPR)");
  await expect(tomb).toContainText("cannot be lifted");
  await expect(tomb.getByRole("button")).toHaveCount(0);
});

test("an_editor_sees_no_lift_control", async () => {
  await editorPage.goto(suppressionsPath);
  await expect(row(editorPage, "seeded@example.test")).toContainText("Complained");
  await expect(editorPage.getByRole("button", { name: /lift/i })).toHaveCount(0);
  // An editor may still add one: that is editor-and-above.
  await expect(editorPage.getByRole("button", { name: "Add suppression", exact: true })).toBeVisible();

  // Field administration is admin-only, so an editor gets a read-only list.
  await editorPage.goto(fieldsPath);
  await expect(row(editorPage, "Region")).toBeVisible();
  await expect(editorPage.getByRole("button", { name: "New field", exact: true })).toHaveCount(0);
  await expect(editorPage.getByRole("button", { name: /archive|purge/i })).toHaveCount(0);
});

test("axe_reports_no_violations_on_all_three_pages", async () => {
  for (const [path, ready] of [
    [`/w/${SLUG}/tags`, "vip"],
    [fieldsPath, "Region"],
    [suppressionsPath, "seeded@example.test"],
  ] as const) {
    await page.goto(path);
    await expect(page.getByText(ready, { exact: true }).first()).toBeVisible();
    await expect(page.getByRole("status").filter({ hasText: /^Loading/ })).toHaveCount(0);
    const { violations } = await new AxeBuilder({ page }).analyze();
    expect(violations, path).toEqual([]);
  }
});

test("axe_reports_no_violations_with_each_destructive_dialog_open", async () => {
  const scan = async (name: string) => {
    const dialog = page.getByRole("dialog", { name, exact: true });
    await expect(dialog).toBeVisible();
    const { violations } = await new AxeBuilder({ page }).analyze();
    expect(violations, name).toEqual([]);
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
  };

  await page.goto(fieldsPath);
  await row(page, "Plan").getByRole("button", { name: "Archive Plan", exact: true }).click();
  await scan("Archive the field Plan");
  await row(page, "Region").getByRole("button", { name: "Purge values of Region", exact: true }).click();
  await scan("Purge the values of Region");

  await page.goto(suppressionsPath);
  await row(page, "seeded@example.test")
    .getByRole("button", { name: "Lift suppression for seeded@example.test", exact: true })
    .click();
  await scan("Lift the suppression on seeded@example.test");

  await page.goto(`/w/${SLUG}/tags`);
  await row(page, "vip").getByRole("button", { name: "Delete vip", exact: true }).click();
  await scan("Delete the tag vip");
});
