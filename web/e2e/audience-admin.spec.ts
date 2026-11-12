// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, type Page, expect, test } from "@playwright/test";

import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/** One workspace with an admin and an editor, seeded once over the API and shared in order. */
test.describe.configure({ mode: "serial" });

const SLUG = "audienceco";
const EDITOR = "editor-aud@example.com";
const EDITOR_PASSWORD = "editor-aud-password";
/** A workspace admin who is NOT an instance admin: the two are deliberately different things. */
const WS_ADMIN = "wsadmin-aud@example.com";
const WS_ADMIN_PASSWORD = "wsadmin-aud-password";

let admin: BrowserContext;
let editor: BrowserContext;
let wsAdmin: BrowserContext;
let page: Page;
let editorPage: Page;
let wsAdminPage: Page;
let workspaceId = "";
let csrf = "";
const ids = {
  archiveMe: "",
  purgeMe: "",
  ada: "",
  list: "",
  crowd: "",
  bulkA: "",
  bulkB: "",
  herd: "",
  predC: "",
  raceA: "",
};

const base = () => `/api/v1/workspaces/${workspaceId}`;

async function post(path: string, data: unknown, scoped = true): Promise<{ id: string }> {
  const res = await admin.request.post(scoped ? `${base()}${path}` : `/api/v1${path}`, {
    data,
    headers: { "X-CSRF-Token": csrf },
  });
  expect(res.ok(), `post ${path} -> ${res.status()}`).toBe(true);
  return (await res.json()) as { id: string };
}

async function put(path: string): Promise<void> {
  const res = await admin.request.put(`${base()}${path}`, { headers: { "X-CSRF-Token": csrf } });
  expect(res.ok(), `put ${path} -> ${res.status()}`).toBe(true);
}

const get = async <T>(path: string): Promise<T> =>
  (await (await admin.request.get(`${base()}${path}`)).json()) as T;

const named = async (path: string) =>
  (await get<{ data: { name: string; status?: string }[] }>(path)).data;

const attributes = async (contactId: string) =>
  ((await (await admin.request.get(`${base()}/contacts/${contactId}`)).json()) as {
    attributes: Record<string, unknown>;
  }).attributes;

test.beforeAll(async ({ browser }) => {
  test.setTimeout(120_000);
  admin = await browser.newContext();
  editor = await browser.newContext();
  wsAdmin = await browser.newContext();
  page = await admin.newPage();
  editorPage = await editor.newPage();
  wsAdminPage = await wsAdmin.newPage();

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

  // The editor and the workspace admin join by invitation, as real ones would.
  const join = async (p: Page, email: string, role: string, name: string, password: string) => {
    const invite = (await post("/invites", { email, role })) as unknown as { invite_url: string };
    await p.goto(new URL(invite.invite_url).pathname);
    await p.getByLabel("Your name", { exact: true }).fill(name);
    await p.getByLabel("Password", { exact: true }).fill(password);
    await p.getByRole("button", { name: "Accept", exact: true }).click();
    await expect(p.getByText(`Signed in as ${email}`)).toBeVisible();
  };
  await join(editorPage, EDITOR, "editor", "Ed Itor", EDITOR_PASSWORD);
  await join(wsAdminPage, WS_ADMIN, "admin", "Wendy Sadmin", WS_ADMIN_PASSWORD);

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

  // Three contacts for the bulk actions, two of whom carry the tag and are on the list: the gap
  // between "affected" and "selected" is then a real one the screen has to explain.
  ids.list = (await post("/lists", { name: "Weekly digest", double_optin: false })).id;
  ids.crowd = (await post("/tags", { name: "crowd" })).id;
  ids.bulkA = (await post("/contacts", { email: "bulk-a@example.test" })).id;
  ids.bulkB = (await post("/contacts", { email: "bulk-b@example.test" })).id;
  await post("/contacts", { email: "bulk-c@example.test" });
  for (const id of [ids.bulkA, ids.bulkB]) {
    await put(`/contacts/${id}/tags/${ids.crowd}`);
    await post(`/contacts/${id}/lists`, { list_id: ids.list });
  }
  await post("/contacts", { email: "del-a@example.test" });
  await post("/contacts", { email: "del-b@example.test" });

  // Three more, all tagged and all on the list, for the snapshotted-predicate path.
  ids.herd = (await post("/tags", { name: "herd" })).id;
  for (const letter of ["a", "b", "c"]) {
    const c = await post("/contacts", { email: `pred-${letter}@example.test` });
    if (letter === "c") ids.predC = c.id;
    await put(`/contacts/${c.id}/tags/${ids.herd}`);
    await post(`/contacts/${c.id}/lists`, { list_id: ids.list });
  }

  ids.raceA = (await post("/contacts", { email: "race-a@example.test" })).id;
  await post("/contacts", { email: "race-b@example.test" });
  await post("/contacts", { email: "race-c@example.test" });

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
  await wsAdmin.close();
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

const contactsPath = `/w/${SLUG}/contacts`;
const instancePath = "/admin/suppressions";
const result = () => page.getByTestId("bulk-result");

/** Narrows the grid to one prefix, then ticks every row it loaded. */
async function selectMatching(q: string, n: number): Promise<void> {
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill(q);
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(n);
  await page.getByRole("checkbox", { name: "Select all loaded contacts", exact: true }).check();
  await expect(page.getByText(`${n} contacts selected`, { exact: true })).toBeVisible();
}

test("bulk_untag_over_a_selection", async () => {
  await selectMatching("bulk-", 3);
  await page.getByLabel("Tag to add or remove", { exact: true }).selectOption({ label: "crowd" });
  await page.getByRole("button", { name: "Remove tag", exact: true }).click();

  // Two of the three had it. A bare "2" would read as success over the whole selection.
  await expect(result()).toContainText("2 of 3 selected contacts");
  await expect(result()).toContainText("crowd");
  await expect(result()).toContainText("The other 1 did not have it");
  expect(await named(`/contacts/${ids.bulkA}/tags`)).toEqual([]);
  expect(await named(`/contacts/${ids.bulkB}/tags`)).toEqual([]);
});

test("bulk_unsubscribe_names_the_list_and_reports_affected_of_selected", async () => {
  await selectMatching("bulk-", 3);
  await page
    .getByLabel("List to subscribe to or unsubscribe from", { exact: true })
    .selectOption({ label: "Weekly digest" });
  await page.getByRole("button", { name: "Unsubscribe from list", exact: true }).click();

  const confirm = page.getByRole("dialog", { name: "Unsubscribe from this list", exact: true });
  // Which list, and how many: an unsubscribe that did not name the list is unreviewable.
  await expect(confirm).toContainText("Weekly digest");
  await expect(confirm).toContainText("3 selected contacts");
  // Per list, never the workspace-wide opt-out, which is a suppression and not a membership.
  await expect(confirm).toContainText("only this list");
  await expect(confirm).not.toContainText(/every list|all mail/i);
  await confirm.getByRole("button", { name: "Unsubscribe these contacts", exact: true }).click();

  await expect(result()).toContainText("2 of 3 selected contacts");
  await expect(result()).toContainText("Weekly digest");
  await expect(result()).toContainText("The other 1 was not subscribed to it");
  expect(await named(`/contacts/${ids.bulkA}/lists`)).toMatchObject([
    { name: "Weekly digest", status: "unsubscribed" },
  ]);
});

test("bulk_delete_requires_confirmation_naming_the_count", async () => {
  await selectMatching("del-", 2);
  await page.getByRole("button", { name: "Delete contacts", exact: true }).click();

  const confirm = page.getByRole("dialog", { name: "Delete these contacts", exact: true });
  // The sentence, not the button: "Delete 2 contacts" on the button satisfies a whole-dialog
  // substring check on its own, so that check passes with the count in the copy made up.
  await expect(confirm.getByRole("paragraph").first()).toContainText("2 contacts");
  await expect(confirm).toContainText("cannot be undone");
  // Deleting is not erasing: nothing is suppressed, so every address stays usable tomorrow.
  await expect(confirm).toContainText("not suppressed");
  await expect(confirm).toContainText("can be added again");
  await confirm.getByRole("button", { name: "Delete 2 contacts", exact: true }).click();

  await expect(result()).toContainText("Deleted 2 contacts");
  // The bulk bar and its button are gone with the selection; focus is on the outcome, not <body>.
  await expect(result()).toBeFocused();
  await expect(row(page, "del-a@example.test")).toHaveCount(0);
  // The promise the dialog made, checked rather than assumed.
  const suppressions = await get<{ data: { email: string | null }[] }>("/suppressions");
  expect(suppressions.data.map((s) => s.email)).not.toContain("del-a@example.test");
  const again = await admin.request.post(`${base()}/contacts`, {
    data: { email: "del-a@example.test" },
    headers: { "X-CSRF-Token": csrf },
  });
  expect(again.status()).toBe(201);
});

test("an_empty_selection_offers_no_action_to_run_on_it", async () => {
  // "All matching", less every row: the bar still shows, and nothing may be run from it.
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("bulk-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(3);
  await page.getByRole("checkbox", { name: "Select bulk-a@example.test", exact: true }).check();
  await page
    .getByRole("button", { name: "Select all contacts matching this view", exact: true })
    .click();
  await expect(page.getByText("All 3 contacts matching this view are selected.")).toBeVisible();
  for (const letter of ["a", "b", "c"]) {
    await page
      .getByRole("checkbox", { name: `Select bulk-${letter}@example.test`, exact: true })
      .uncheck();
  }
  await expect(
    page.getByText("0 of 3 contacts matching this view are selected (3 left out)."),
  ).toBeVisible();

  await expect(page.getByRole("button", { name: "Delete contacts", exact: true })).toBeDisabled();
  await page.getByLabel("Tag to add or remove", { exact: true }).selectOption({ label: "crowd" });
  await expect(page.getByRole("button", { name: "Remove tag", exact: true })).toBeDisabled();
});

test("a_predicate_selection_spares_the_excluded_contact_in_all_three_actions", async () => {
  // The path that matters: a signed token and the rows ticked back off, never a list of ids.
  // When this went wrong earlier in this milestone it would have swept the whole workspace.
  type Body = { selection_token?: string; excluded_ids?: string[]; ids?: string[] };
  const bodies: Record<string, Body> = {};
  for (const kind of ["untag", "unsubscribe", "delete"]) {
    await page.route(`**/contacts/bulk/${kind}`, async (route) => {
      bodies[kind] = route.request().postDataJSON();
      await route.continue();
    });
  }

  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("pred-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(3);
  await page.getByRole("checkbox", { name: "Select pred-a@example.test", exact: true }).check();
  await page
    .getByRole("button", { name: "Select all contacts matching this view", exact: true })
    .click();
  await expect(page.getByText("All 3 contacts matching this view are selected.")).toBeVisible();
  await page.getByRole("checkbox", { name: "Select pred-c@example.test", exact: true }).uncheck();
  await expect(
    page.getByText("2 of 3 contacts matching this view are selected (1 left out)."),
  ).toBeVisible();

  await page.getByLabel("Tag to add or remove", { exact: true }).selectOption({ label: "herd" });
  await page.getByRole("button", { name: "Remove tag", exact: true }).click();
  await expect(result()).toContainText("Removed “herd” from 2 contacts");
  expect(await named(`/contacts/${ids.predC}/tags`)).toMatchObject([{ name: "herd" }]);

  await page
    .getByLabel("List to subscribe to or unsubscribe from", { exact: true })
    .selectOption({ label: "Weekly digest" });
  await page.getByRole("button", { name: "Unsubscribe from list", exact: true }).click();
  await page
    .getByRole("dialog", { name: "Unsubscribe from this list", exact: true })
    .getByRole("button", { name: "Unsubscribe these contacts", exact: true })
    .click();
  await expect(result()).toContainText("Unsubscribed 2 contacts from “Weekly digest”");
  expect(await named(`/contacts/${ids.predC}/lists`)).toMatchObject([{ status: "subscribed" }]);

  await page.getByRole("button", { name: "Delete contacts", exact: true }).click();
  await page
    .getByRole("dialog", { name: "Delete these contacts", exact: true })
    .getByRole("button", { name: "Delete 2 contacts", exact: true })
    .click();
  await expect(result()).toContainText("Deleted 2 contacts");
  // The one left out is still here, with everything it had; the other two are gone.
  const left = await get<{ data: { email: string }[] }>("/contacts?q=pred-");
  expect(left.data.map((c) => c.email)).toEqual(["pred-c@example.test"]);

  for (const kind of ["untag", "unsubscribe", "delete"]) {
    expect(typeof bodies[kind]?.selection_token, kind).toBe("string");
    expect(bodies[kind]?.excluded_ids, kind).toHaveLength(1);
    expect(bodies[kind]?.ids, kind).toBeUndefined();
    await page.unroute(`**/contacts/bulk/${kind}`);
  }
});

test("a_delete_that_lost_a_race_is_measured_against_what_it_promised", async () => {
  await selectMatching("race-", 3);
  await page.getByRole("button", { name: "Delete contacts", exact: true }).click();
  const confirm = page.getByRole("dialog", { name: "Delete these contacts", exact: true });
  await expect(confirm.getByRole("paragraph").first()).toContainText("3 contacts");

  // Someone else removes one of the three while the dialog is open. The server's own two
  // numbers agree whatever happens, so only the promise can show the gap.
  const gone = await admin.request.delete(`${base()}/contacts/${ids.raceA}`, {
    headers: { "X-CSRF-Token": csrf },
  });
  expect(gone.ok(), `delete -> ${gone.status()}`).toBe(true);

  await confirm.getByRole("button", { name: "Delete 3 contacts", exact: true }).click();
  await expect(result()).toContainText("2 of the 3 contacts you confirmed");
  await expect(result()).toContainText("someone else");
});

test("the_instance_suppression_screen_is_instance_admin_only", async () => {
  await page.goto(contactsPath);
  await page.getByRole("link", { name: "Instance suppressions", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Instance suppressions", exact: true })).toBeVisible();
  await expect(page.getByText("every workspace").first()).toBeVisible();

  await page.getByLabel("Email address", { exact: true }).fill("Everywhere@Example.test");
  await page.getByLabel("Reason", { exact: true }).selectOption("complained");
  await page.getByRole("button", { name: "Add suppression", exact: true }).click();
  await expect(row(page, "everywhere@example.test")).toContainText("Complained");

  // An instance-wide block is not a workspace one, and the workspace screen must not show it.
  await page.goto(suppressionsPath);
  await expect(row(page, "everywhere@example.test")).toHaveCount(0);

  await page.goto(instancePath);
  await row(page, "everywhere@example.test")
    .getByRole("button", { name: "Lift suppression for everywhere@example.test", exact: true })
    .click();
  const dialog = page.getByRole("dialog", {
    name: "Lift the suppression on everywhere@example.test",
    exact: true,
  });
  await expect(dialog).toContainText("every workspace");
  await dialog.getByRole("button", { name: "Lift suppression", exact: true }).click();
  await expect(row(page, "everywhere@example.test")).toHaveCount(0);
});

test("a_workspace_admin_cannot_reach_it", async () => {
  // An admin of this workspace, with every workspace-level control: the audit log is admin-only.
  await wsAdminPage.goto(`/w/${SLUG}/audit-log`);
  await expect(wsAdminPage.getByRole("heading", { name: "Audit log", exact: true })).toBeVisible();
  await expect(wsAdminPage.getByRole("link", { name: "Instance suppressions" })).toHaveCount(0);

  await wsAdminPage.goto(instancePath);
  // The gate's own sentence. "Only an instance admin" alone would also match the 403 the list
  // query would answer with if the gate were gone, so it would not tell the two apart.
  await expect(
    wsAdminPage.getByText("Being an admin of a workspace is not the same thing"),
  ).toBeVisible();
  await expect(wsAdminPage.getByRole("table")).toHaveCount(0);
  await expect(wsAdminPage.getByRole("button", { name: "Add suppression" })).toHaveCount(0);
  // The screen is only a courtesy; the route itself refuses them, and with 403 rather than 404.
  const refused = await wsAdmin.request.get("/api/v1/admin/suppressions");
  expect(refused.status()).toBe(403);
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

  // The bulk confirmations, which need a selection before they exist at all.
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("bulk-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(3);
  await page.getByRole("checkbox", { name: "Select all loaded contacts", exact: true }).check();
  await page
    .getByLabel("List to subscribe to or unsubscribe from", { exact: true })
    .selectOption({ label: "Weekly digest" });
  await page.getByRole("button", { name: "Unsubscribe from list", exact: true }).click();
  await scan("Unsubscribe from this list");
  await page.getByRole("button", { name: "Delete contacts", exact: true }).click();
  await scan("Delete these contacts");
});
