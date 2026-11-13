// SPDX-License-Identifier: AGPL-3.0-only

import { readFile } from "node:fs/promises";

import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, type Page, expect, test } from "@playwright/test";

import { expectNoViolations } from "./support/axe";
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
  churn: "",
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

  // Four for the widening case, two of them tagged: the filter can then be widened from outside
  // the browser, over the API, exactly as a second editor would.
  ids.churn = (await post("/tags", { name: "churn" })).id;
  for (const letter of ["a", "b", "c", "d"]) {
    const c = await post("/contacts", { email: `wide-${letter}@example.test` });
    if (letter === "a" || letter === "b") await put(`/contacts/${c.id}/tags/${ids.churn}`);
  }

  // Two that no other test deletes: a frozen selection the search can then show less of.
  await post("/contacts", { email: "view-a@example.test" });
  await post("/contacts", { email: "view-b@example.test" });

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

test("the_bulk_tag_confirmation_says_what_a_tag_will_mean", async () => {
  // Spec §9.1: snapshots answer *which* contacts an action reaches, structurally. What they
  // cannot answer is what the operator understood they were agreeing to — and tagging is about to
  // become the thing a campaign is aimed at.
  await selectMatching("bulk-", 3);
  await page.getByLabel("Tag to add or remove", { exact: true }).selectOption({ label: "crowd" });
  await page.getByRole("button", { name: "Add tag", exact: true }).click();

  const confirm = page.getByRole("dialog", { name: "Tag these contacts", exact: true });
  // The sentence, not the button: "Tag 3 contacts" on the button satisfies a whole-dialog
  // substring check on its own, so that check passes with the count in the copy made up.
  await expect(confirm.getByRole("paragraph").first()).toContainText("3 contacts");
  await expect(confirm).toContainText("crowd");
  await expect(confirm).toContainText("campaign");
  await expect(confirm).toContainText("mailable");
  // The honest tense: nothing can send to a tag in this milestone, and copy that said otherwise
  // would be describing a feature that does not exist.
  await expect(confirm).toContainText("Nothing sends to a tag yet");
  // §9.1's paragraph names the count too, and it is a *different* paragraph from the one above:
  // a whole-dialog check is satisfied by the other one and by the button, so this names `.nth(1)`.
  await expect(confirm.getByRole("paragraph").nth(1)).toContainText("3 contacts");
  // A tag is not a list membership. Saying "subscribed" here would describe the one action that
  // really does put people on a mailing list, which this is not.
  await expect(confirm).not.toContainText(/subscrib/i);
  // Not red. §9.1 puts `bulk/tag` on the reporting side, "Remove tag" undoes it completely, and
  // the bulk delete two controls along is the one thing on this bar that cannot be undone.
  const tag = confirm.getByRole("button", { name: "Tag 3 contacts", exact: true });
  await expect(tag).toHaveClass(/bg-accent/);
  await expect(tag).not.toHaveClass(/bg-danger/);
  await tag.click();

  await expect(result()).toContainText("Tagged 3 contacts with “crowd”");
  expect(await named(`/contacts/${ids.bulkA}/tags`)).toMatchObject([{ name: "crowd" }]);
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
  // The bulk bar and its button are gone with the selection, so focus has nowhere to return to
  // and is put on the outcome instead of falling to <body>.
  await expect(page.getByRole("region", { name: "Bulk actions" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Delete contacts", exact: true })).toHaveCount(0);
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
  await expect(page.getByText("Here is what you selected: all 3 contacts.")).toBeVisible();
  for (const letter of ["a", "b", "c"]) {
    await page
      .getByRole("checkbox", { name: `Select bulk-${letter}@example.test`, exact: true })
      .uncheck();
  }
  await expect(
    page.getByText("Here is what you selected: 0 of 3 contacts (3 left out)."),
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
  await expect(page.getByText("Here is what you selected: all 3 contacts.")).toBeVisible();
  await page.getByRole("checkbox", { name: "Select pred-c@example.test", exact: true }).uncheck();
  await expect(
    page.getByText("Here is what you selected: 2 of 3 contacts (1 left out)."),
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

/** Every `snapshot_id` the grid has asked `GET /contacts` for, newest last; `null` for none. */
function watchGrid(): (string | null)[] {
  const seen: (string | null)[] = [];
  page.on("request", (r) => {
    const url = new URL(r.url());
    if (r.method() === "GET" && url.pathname.endsWith("/contacts")) {
      seen.push(url.searchParams.get("snapshot_id"));
    }
  });
  return seen;
}

test("the_grid_shows_the_frozen_set_and_a_newcomer_never_joins_it", async () => {
  // Spec §14's "here is what you selected": once a selection exists the grid is the snapshot,
  // not the live filter. That removes at source the divergence §8.9 had to fix arithmetically —
  // a contact tagged into the filter after selection used to appear as a ticked row the
  // selection never held, and unticking it refused the action.
  const asked = watchGrid();
  await page.goto(contactsPath);
  await page.getByLabel("Filter by tag", { exact: true }).selectOption({ label: "churn" });
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(2);
  await page.getByRole("checkbox", { name: "Select all loaded contacts", exact: true }).check();
  await page
    .getByRole("button", { name: "Select all contacts matching this view", exact: true })
    .click();
  await expect(page.getByText("Here is what you selected: all 2 contacts.")).toBeVisible();
  // The parameter itself, on the wire: without it every assertion below would be satisfied by a
  // grid that simply had not refetched yet.
  await expect
    .poll(() => asked.at(-1), { message: "the grid never asked for the snapshot" })
    .toMatch(/^[0-9a-f-]{36}$/);

  // A second editor tags one more into the filter. The live filter now matches three; the frozen
  // set is still the two that were written down.
  const found = await get<{ data: { id: string }[] }>("/contacts?q=wide-c@");
  const wideC = found.data[0]?.id ?? "";
  await put(`/contacts/${wideC}/tags/${ids.churn}`);

  // An action over the token, which refetches the grid afterwards because the view is filtered
  // by a tag the action changed.
  await page.getByLabel("Tag to add or remove", { exact: true }).selectOption({ label: "crowd" });
  await page.getByRole("button", { name: "Add tag", exact: true }).click();
  await page
    .getByRole("dialog", { name: "Tag these contacts", exact: true })
    .getByRole("button", { name: "Tag 2 contacts", exact: true })
    .click();
  await expect(result()).toContainText("Tagged 2 contacts with “crowd”");
  expect(await named(`/contacts/${wideC}/tags`)).toMatchObject([{ name: "churn" }]);

  // Two rows, not three: the newcomer matches the filter and is not in the snapshot, so the grid
  // does not show it and there is no row to untick that the action would not reach.
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(2);
  await expect(row(page, "wide-c@example.test")).toHaveCount(0);
  await expect(page.getByText("Here is what you selected: all 2 contacts.")).toBeVisible();

  // Untick one that *is* in the frozen set: the ordinary case proceeds and spares exactly it.
  await page.getByRole("checkbox", { name: "Select wide-a@example.test", exact: true }).uncheck();
  await expect(page.getByText("Here is what you selected: 1 of 2 contacts (1 left out).")).toBeVisible();
  await page.getByRole("button", { name: "Delete contacts", exact: true }).click();
  await page
    .getByRole("dialog", { name: "Delete these contacts", exact: true })
    .getByRole("button", { name: "Delete 1 contact", exact: true })
    .click();
  await expect(result()).toContainText("Deleted 1 contact");
  const after = await get<{ data: { email: string }[] }>("/contacts?q=wide-");
  expect(after.data.map((c) => c.email).sort()).toEqual([
    "wide-a@example.test",
    "wide-c@example.test",
    "wide-d@example.test",
  ]);
  page.removeAllListeners("request");
});

test("filtering_a_frozen_selection_narrows_the_view_and_not_the_selection", async () => {
  // The filters stay usable over a frozen set, and they narrow what is *shown*. The screen has to
  // say so in both numbers, or someone unticks what they can see and misjudges the rest.
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("view-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(2);
  await page.getByRole("checkbox", { name: "Select all loaded contacts", exact: true }).check();
  await page
    .getByRole("button", { name: "Select all contacts matching this view", exact: true })
    .click();
  await expect(page.getByText("Here is what you selected: all 2 contacts.")).toBeVisible();

  // Reachable at all: the toolbar used to be hidden the moment anything was selected.
  const search = page.getByLabel("Search contacts", { exact: true });
  await expect(search).toBeVisible();
  await search.fill("view-b");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(1);

  // Both numbers, and which of the two the filter moved.
  await expect(
    page.getByText(
      "Showing 1 of the 2 contacts you selected. Filtering changes what is shown here, not what is selected.",
    ),
  ).toBeVisible();
  // The selection itself did not move: not the bar, not the button, not the announcement that a
  // ticked set gets when the filters change.
  await expect(page.getByText("Here is what you selected: all 2 contacts.")).toBeVisible();
  // Past the live region's 500 ms debounce, so "nothing was announced" is a fact and not a race:
  // a selection that had been thrown away would have said so by now.
  await page.waitForTimeout(900);
  await expect(page.getByRole("status").filter({ hasText: /Selection cleared/ })).toHaveCount(0);
  // What it *does* say. R3's other false sentence lived here — "All contacts matching this view
  // selected" — and it is the one surface in this screen a screen-reader user hears.
  await expect(
    page.getByRole("status").filter({ hasText: /^2 contacts selected$/ }),
  ).toHaveCount(1);
  await page.getByRole("button", { name: "Delete contacts", exact: true }).click();
  const confirm = page.getByRole("dialog", { name: "Delete these contacts", exact: true });
  await expect(confirm.getByRole("paragraph").first()).toContainText("2 contacts");
  await page.keyboard.press("Escape");
  await expect(confirm).toBeHidden();

  // And back: widening the view again puts the second row back and retires the sentence.
  await search.fill("view-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(2);
  await expect(page.getByText("Filtering changes what is shown here")).toHaveCount(0);
});

/** `GET /contacts`, whatever its query string. A glob would have to guess at `?`. */
const gridRequests = (url: URL) => url.pathname.endsWith("/contacts");

test("the_rows_cannot_be_ticked_while_the_frozen_page_is_in_flight", async () => {
  // The window a real `409 selection-count-mismatch` came through. `placeholderData` keeps the
  // previous, live-filter rows on screen while the snapshot page loads, and `shownKey` does not
  // remount the grid — so a row that had left the filter before the snapshot was taken was still
  // tickable, and unticking it excluded an id the snapshot never held.
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("view-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(2);
  await page.getByRole("checkbox", { name: "Select all loaded contacts", exact: true }).check();

  let release = () => {};
  const held = new Promise<void>((done) => {
    release = done;
  });
  let holding = true;
  await page.route(gridRequests, async (route) => {
    if (holding && route.request().url().includes("snapshot_id=")) {
      holding = false;
      await held;
    }
    await route.continue().catch(() => undefined);
  });

  await page
    .getByRole("button", { name: "Select all contacts matching this view", exact: true })
    .click();
  await expect(page.getByText("Here is what you selected: all 2 contacts.")).toBeVisible();

  // Off, and it says why: a disabled control that does not explain itself is one nobody can act on.
  await expect(page.getByText("Loading the contacts you selected")).toBeVisible();
  const row = page.getByRole("checkbox", { name: "Select view-a@example.test", exact: true });
  const header = page.getByRole("checkbox", { name: "Select all loaded contacts", exact: true });
  await expect(row).toBeDisabled();
  await expect(header).toBeDisabled();
  // And says it to a screen reader, not only to a screen. This state lasts one round trip, so a
  // reader who lands on a dimmed box after the sentence has been read has no second chance at it:
  // the box itself has to carry it.
  await expect(row).toHaveAccessibleDescription(/cannot be ticked until it arrives/);
  await expect(header).toHaveAccessibleDescription(/cannot be ticked until it arrives/);

  release();
  await page.unroute(gridRequests);
  // And back on once the rows on screen really are the frozen set. The description is not
  // asserted here: the paragraph it points at is unmounted with the lock, so the computed
  // description is empty whatever the attribute says — a check that could not fail.
  await expect(row).toBeEnabled();
  await expect(page.getByText("Loading the contacts you selected")).toHaveCount(0);
});

test("a_real_server_409_count_mismatch_still_reaches_the_screen_in_its_own_words", async () => {
  // The screen can no longer build a selection whose confirmed count is under what the action
  // reaches — the test above closed the one window that allowed it — so the one field is forged
  // here. Everything else is real: the server computes the mismatch, writes the sentence, and the
  // client renders what it is given. A stub would be asserting the client against itself.
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("view-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(2);
  await page.getByRole("checkbox", { name: "Select all loaded contacts", exact: true }).check();
  await page
    .getByRole("button", { name: "Select all contacts matching this view", exact: true })
    .click();
  await expect(page.getByText("Here is what you selected: all 2 contacts.")).toBeVisible();

  await page.route("**/contacts/bulk/delete", async (route) => {
    const body = route.request().postDataJSON() as Record<string, unknown>;
    await route.continue({ postData: JSON.stringify({ ...body, confirmed_count: 1 }) });
  });
  await page.getByRole("button", { name: "Delete contacts", exact: true }).click();
  await page
    .getByRole("dialog", { name: "Delete these contacts", exact: true })
    .getByRole("button", { name: "Delete 2 contacts", exact: true })
    .click();

  // `selection.rs` wrote this, not the test.
  await expect(page.getByRole("alert")).toHaveText(
    "This would have acted on 2 contacts and the confirmation said 1, so nothing was done. " +
      "Select again to see what you would be acting on.",
  );
  await page.unroute("**/contacts/bulk/delete");
  // Refused before it deleted anything.
  const left = await get<{ data: { email: string }[] }>("/contacts?q=view-");
  expect(left.data).toHaveLength(2);
});

test("a_refused_bulk_action_shows_the_servers_sentence_and_drops_the_selection", async () => {
  // The rendering, with the refusal stubbed. The test above it produces a real one from the real
  // server; this one pins what the screen does with it — the server's own words, and the dead
  // selection dropped — without depending on how the server was persuaded to send it.
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("bulk-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(3);
  await page.getByRole("checkbox", { name: "Select all loaded contacts", exact: true }).check();
  await page
    .getByRole("button", { name: "Select all contacts matching this view", exact: true })
    .click();
  await expect(page.getByText("Here is what you selected: all 3 contacts.")).toBeVisible();

  await page.route("**/contacts/bulk/delete", (route) =>
    route.fulfill({
      status: 409,
      contentType: "application/problem+json",
      body: JSON.stringify({
        type: "urn:nm:problem:selection-count-mismatch",
        title: "Conflict",
        status: 409,
        detail:
          "This would have acted on 4 contacts and the confirmation said 3, so nothing was done. Select again to see what you would be acting on.",
      }),
    }),
  );
  await page.getByRole("button", { name: "Delete contacts", exact: true }).click();
  await page
    .getByRole("dialog", { name: "Delete these contacts", exact: true })
    .getByRole("button", { name: "Delete 3 contacts", exact: true })
    .click();

  // The server's sentence, whole and on its own: the client's fallback says nothing a person
  // could act on, and an exact match catches it where a substring check would not. It waits for
  // the alert rather than racing it, which a bare "the fallback is absent" could never do.
  await expect(page.getByRole("alert")).toHaveText(
    "This would have acted on 4 contacts and the confirmation said 3, so nothing was done. " +
      "Select again to see what you would be acting on.",
  );
  // The selection it refused is dropped, so the same button cannot repeat the same refusal. The
  // tick box is what makes this wait: it stays checked until the selection goes.
  await expect(
    page.getByRole("checkbox", { name: "Select all loaded contacts", exact: true }),
  ).not.toBeChecked();
  await expect(page.getByText(/^Here is what you selected/)).toHaveCount(0);
  await page.unroute("**/contacts/bulk/delete");
  expect((await get<{ data: unknown[] }>("/contacts?q=bulk-")).data).toHaveLength(3);
});

test("a_view_that_shrank_for_another_reason_states_the_fact_and_not_a_cause", async () => {
  // Fewer of the frozen set on screen than were selected is true whenever it is true — a second
  // editor deleting one does it with no filter typed. The *explanation* is about filtering, so it
  // is said only when filtering is why; a sentence that always blames the filter is a sentence
  // asserting something that did not happen.
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("view-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(2);
  await page.getByRole("checkbox", { name: "Select all loaded contacts", exact: true }).check();
  await page
    .getByRole("button", { name: "Select all contacts matching this view", exact: true })
    .click();
  await expect(page.getByText("Here is what you selected: all 2 contacts.")).toBeVisible();

  // The search goes. The grid is the snapshot, so it still shows exactly those two and there is
  // nothing to say: the view and the selection are the same set.
  await page.getByLabel("Search contacts", { exact: true }).fill("");
  await expect(page).not.toHaveURL(/q=view/);
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(2);
  await expect(page.getByText(/^Showing /)).toHaveCount(0);
  await expect(page.getByText("Filtering changes what is shown here")).toHaveCount(0);

  // A second editor removes one of them, and the grid is made to ask again by sorting — which is
  // not a filter.
  const found = await get<{ data: { id: string }[] }>("/contacts?q=view-b@");
  const gone = await admin.request.delete(`${base()}/contacts/${found.data[0]?.id ?? ""}`, {
    headers: { "X-CSRF-Token": csrf },
  });
  expect(gone.ok(), `delete -> ${gone.status()}`).toBe(true);
  await page.getByRole("button", { name: "Email", exact: true }).click();
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(1);

  // The fact, with both numbers. Not the reason, because filtering is not the reason.
  await expect(page.getByText("Showing 1 of the 2 contacts you selected.")).toBeVisible();
  await expect(page.getByText("Filtering changes what is shown here")).toHaveCount(0);
});

/**
 * The columns `src/contacts/export.rs` writes: the contact's own fields, every **live** custom
 * field — `region` and `tier` are archived by the tests above, `plan` is not — then the three
 * spec §10 adds.
 */
const CSV_HEADER =
  "id,email,first_name,last_name,timezone,created_at,updated_at,attr:plan,lists,tags,suppressed";

const exportDialog = (p: Page) =>
  p.getByRole("dialog", { name: "Export these contacts", exact: true });

test("an_editor_never_sees_the_export_control", async () => {
  await editorPage.goto(contactsPath);
  // A waiting neighbour for the absence below: the grid is drawn and the editor's own controls
  // are on it, so "no Export" is an absence and not a page that has not drawn its buttons yet.
  await expect(editorPage.getByRole("grid", { name: "Contacts" })).toBeVisible();
  // The assertion that both waits and discriminates: the two header buttons as a list, so an
  // Export that appeared would make the list two long. Checked the other way round — "no Export
  // button" on its own passes against a page that has not drawn its buttons yet, which is why it
  // is not left to carry this alone.
  await expect(editorPage.getByRole("button", { name: /^(Export CSV|New contact)$/ })).toHaveText([
    "New contact",
  ]);
  // Absent, not disabled (spec §14): a greyed-out Export names the door for every editor to knock
  // on, and §10 is explicit that admin-only is about raising the cost of exfiltration. Kept for
  // its breadth — it catches a control under any other name — but it carries nothing on its own:
  // a page-wide absence has nothing to wait on. Do not copy it into a test without the line above.
  await expect(editorPage.getByRole("button", { name: /export/i })).toHaveCount(0);

  // The screen is only a courtesy; the route refuses them too. The problem code is asserted
  // because a missing CSRF header is also a 403, and that would pass while proving nothing.
  const me = (await (await editor.request.get("/api/v1/me")).json()) as { csrf_token: string };
  const refused = await editor.request.post(`${base()}/contacts/export`, {
    data: {},
    headers: { "X-CSRF-Token": me.csrf_token },
  });
  expect(refused.status()).toBe(403);
  expect(await refused.json()).toMatchObject({ type: "urn:nm:problem:insufficient-role" });
});

test("exporting_says_what_leaves_and_the_download_starts", async () => {
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("bulk-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(3);
  await page.getByRole("button", { name: "Export CSV", exact: true }).click();

  const dialog = exportDialog(page);
  await expect(dialog).toContainText("personal data");
  await expect(dialog).toContainText("audit log");
  // Of the filter, never of a selection: `POST /contacts/export` takes a `NewSelection` and has
  // no `snapshot_id`, so it re-runs the filter and cannot be aimed at a frozen set.
  await expect(dialog).toContainText("everything matching the filters on this screen");
  // Nobody has counted this filter, so no number is named. `Page` carries no total and the one
  // endpoint that counts a filter exactly writes a snapshot to do it; a number here is invented.
  await expect(dialog).toContainText("The number of rows is not known here");
  expect(await dialog.innerText()).not.toMatch(/\d/);

  // Red, and not by the bulk-tag dialog's reasoning: a downloaded file cannot be un-downloaded.
  const go = dialog.getByRole("button", { name: "Export CSV", exact: true });
  await expect(go).toHaveClass(/bg-danger/);

  const [file, res] = await Promise.all([
    page.waitForEvent("download"),
    page.waitForResponse("**/contacts/export"),
    go.click(),
  ]);
  expect(file.suggestedFilename()).toBe("contacts.csv");
  // The client names the file itself — a body it reads into a blob never reaches the browser's
  // download machinery, so `content-disposition` is not applied. Asserting the client's own name
  // alone would let the two drift in silence; this ties it to what the server said.
  expect(res.headers()["content-disposition"]).toContain(`filename="${file.suggestedFilename()}"`);
  const csv = await readFile(await file.path(), "utf8");

  const lines = csv.trim().split("\r\n");
  expect(lines[0]).toBe(CSV_HEADER);
  // The filter reached the server: the three it matched, and nothing else in the workspace.
  expect(lines).toHaveLength(4);
  expect(csv).toContain("bulk-a@example.test");
  expect(csv).not.toContain("ada@example.test");
  // §10: every membership carries its status, or M1c's importer reads this file back as a
  // re-subscribe for someone who unsubscribed.
  expect(lines.find((l) => l.includes("bulk-a@"))).toContain("Weekly digest:unsubscribed");

  // Closed when the file arrived, and nothing was refused.
  await expect(dialog).toBeHidden();
  await expect(page.getByRole("alert")).toHaveCount(0);
});

test("a_slow_export_says_it_is_not_stuck", async () => {
  // The whole CSV is read into the page before the download can start, so a large export looks
  // hung for as long as it takes. The dialog has to say so while it waits, and say it to a screen
  // reader and not only to a screen.
  let release = () => {};
  const held = new Promise<void>((done) => {
    release = done;
  });
  let holding = true;
  await page.route("**/contacts/export", async (route) => {
    if (holding) {
      holding = false;
      await held;
    }
    await route.continue().catch(() => undefined);
  });

  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("bulk-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(3);
  await page.getByRole("button", { name: "Export CSV", exact: true }).click();
  const dialog = exportDialog(page);
  await dialog.getByRole("button", { name: "Export CSV", exact: true }).click();

  await expect(dialog.getByRole("status")).toHaveText(
    "Preparing the file. The whole export has to arrive before the download starts, so a large " +
      "one takes a while — nothing is stuck.",
  );
  await expect(dialog.getByRole("button", { name: "Export CSV", exact: true })).toHaveAttribute(
    "aria-busy",
    "true",
  );

  const arriving = page.waitForEvent("download");
  release();
  expect((await arriving).suggestedFilename()).toBe("contacts.csv");
  await page.unroute("**/contacts/export");
  await expect(dialog).toBeHidden();
});

test("an_export_that_never_arrives_says_so_instead_of_closing_quietly", async () => {
  // A request that produces no response document to render. There is no refusal to show, and the
  // dialog is closing either way, so without a sentence of its own the screen would say nothing.
  //
  // This aborts **before** the response, which is one of the two ways in. The other is the abort
  // `export.rs` sends when the query fails mid-book rather than passing a short file off as a
  // whole one: `fetch` resolves and the body rejects instead. Both leave `api.POST` the same way
  // — `openapi-fetch` reads an ok response as a bare `return await response[parseAs]()` — so the
  // `catch` under test is the same one. Four attempts to synthesise the mid-body half from the
  // browser side are written up in the fix report; Playwright and CDP both re-frame an
  // intercepted body, so neither can deliver a response that starts and then fails.
  await page.route("**/contacts/export", (route) => route.abort("failed"));

  let started = 0;
  const count = () => {
    started += 1;
  };
  page.on("download", count);
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("bulk-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(3);

  // A bulk action first, so its success paragraph is on screen when the export fails. Removing a
  // tag none of the three carries changes nothing and still reports. Two reports of "the last
  // thing that happened", one green and one red, is the state this must not leave behind.
  await page.getByRole("checkbox", { name: "Select all loaded contacts", exact: true }).check();
  await page.getByLabel("Tag to add or remove", { exact: true }).selectOption({ label: "herd" });
  await page.getByRole("button", { name: "Remove tag", exact: true }).click();
  await expect(result()).toContainText("The other 3 did not have it");

  await page.getByRole("button", { name: "Export CSV", exact: true }).click();
  const dialog = exportDialog(page);
  await dialog.getByRole("button", { name: "Export CSV", exact: true }).click();

  await expect(page.getByRole("alert")).toHaveText(
    "The export did not finish, so no file was saved. Try again.",
  );
  await expect(dialog).toBeHidden();
  // The earlier success is gone with it, rather than sitting above the refusal.
  await expect(result()).toHaveCount(0);
  expect(started).toBe(0);
  page.off("download", count);
  await page.unroute("**/contacts/export");
});

test("the_export_dialog_names_a_count_only_when_one_was_taken_for_this_filter", async () => {
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("bulk-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(3);
  await page.getByRole("checkbox", { name: "Select all loaded contacts", exact: true }).check();
  await page
    .getByRole("button", { name: "Select all contacts matching this view", exact: true })
    .click();
  await expect(page.getByText("Here is what you selected: all 3 contacts.")).toBeVisible();

  await page.getByRole("button", { name: "Export CSV", exact: true }).click();
  const dialog = exportDialog(page);
  // The one exact count of this filter anybody paid for, named as what it is: taken then, not now.
  await expect(dialog).toContainText(
    "3 contacts matched when you selected them, and the export runs the filters again, so the " +
      "file holds whatever matches when it runs.",
  );
  // Nothing was left out and the filter has not moved, so the selection *is* this filter and
  // there is nothing to disown. The paragraph count is the assertion that waits — "a sentence is
  // absent" has nothing to wait on and passes against a dialog that has not drawn yet.
  await expect(dialog.getByRole("paragraph")).toHaveCount(2);
  await expect(dialog).not.toContainText("An export cannot be pointed at a selection");
  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();

  // Leave one out and the two part company: the export would still take all three.
  await page.getByRole("checkbox", { name: "Select bulk-c@example.test", exact: true }).uncheck();
  await expect(
    page.getByText("Here is what you selected: 2 of 3 contacts (1 left out)."),
  ).toBeVisible();
  await page.getByRole("button", { name: "Export CSV", exact: true }).click();
  await expect(dialog).toContainText("This is not the 2 contacts you selected");
  await expect(dialog).toContainText("3 contacts matched when you selected them");
  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
});

test("re_ordering_the_grid_changes_nothing_the_export_dialog_says", async () => {
  // Sorting moves no contact in or out, so the count is still this filter's count and the
  // selection is still the whole of it. Keying either on the grid's own cache identity — which
  // has to carry `sort` and `dir` — made a column header throw the number away and, worse, add
  // "This is not the 3 contacts you selected" about the very three it would export.
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("bulk-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(3);
  await page.getByRole("checkbox", { name: "Select all loaded contacts", exact: true }).check();
  await page
    .getByRole("button", { name: "Select all contacts matching this view", exact: true })
    .click();
  await expect(page.getByText("Here is what you selected: all 3 contacts.")).toBeVisible();

  // Default order is newest first. Sorting by email re-orders the same three rows.
  await page.getByRole("button", { name: "Email", exact: true }).click();
  await expect(page).toHaveURL(/sort=email/);
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(3);
  await expect(page.locator("tbody tr[aria-rowindex]").first()).toContainText("bulk-a@example.test");
  await expect(page.getByText("Here is what you selected: all 3 contacts.")).toBeVisible();

  await page.getByRole("button", { name: "Export CSV", exact: true }).click();
  const dialog = exportDialog(page);
  await expect(dialog).toContainText("3 contacts matched when you selected them");
  // Two paragraphs, so there is no third disowning a selection that has not changed. The count is
  // the assertion that waits; "a sentence is absent" alone would pass on an undrawn dialog.
  await expect(dialog.getByRole("paragraph")).toHaveCount(2);
  await expect(dialog).not.toContainText("An export cannot be pointed at a selection");
  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
});

test("the_export_dialog_disowns_ticked_rows_too", async () => {
  // The other way a selection exists: rows ticked by hand, with nothing written down on the
  // server. The export still takes the filter, so the dialog still has to say it is not these.
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("bulk-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(3);
  await page.getByRole("checkbox", { name: "Select bulk-a@example.test", exact: true }).check();
  await expect(page.getByText("1 contact selected", { exact: true })).toBeVisible();

  await page.getByRole("button", { name: "Export CSV", exact: true }).click();
  const dialog = exportDialog(page);
  await expect(dialog).toContainText("This is not the 1 contact you selected");
  // Ticking rows materialises nothing, so there is still no count of this filter to name.
  await expect(dialog).toContainText("The number of rows is not known here");
  await page.keyboard.press("Escape");
  await expect(dialog).toBeHidden();
});

test("a_rate_limited_export_shows_the_servers_sentence_and_downloads_nothing", async () => {
  // Real, not stubbed. `EXPORT_PER_MINUTE` is 5 per workspace, so five over the API spend the
  // window and the one through the UI is refused by the server that wrote the rule. This is the
  // last export in this file: it leaves the window spent for a minute.
  for (let n = 0; n < 5; n++) {
    await admin.request.post(`${base()}/contacts/export`, {
      data: { q: "bulk-" },
      headers: { "X-CSRF-Token": csrf },
    });
  }

  let started = 0;
  const count = () => {
    started += 1;
  };
  page.on("download", count);
  await page.goto(contactsPath);
  await page.getByLabel("Search contacts", { exact: true }).fill("bulk-");
  await expect(page.locator("tbody tr[aria-rowindex]")).toHaveCount(3);
  await page.getByRole("button", { name: "Export CSV", exact: true }).click();
  const dialog = exportDialog(page);
  await dialog.getByRole("button", { name: "Export CSV", exact: true }).click();

  // `src/error.rs` wrote this sentence, not the client. A nicer one of the client's own would be
  // the client guessing at a rule only the server knows.
  await expect(page.getByRole("alert")).toHaveText("too many requests");
  // The refusal arrives after the dialog was agreed to, so it lands where the person is looking:
  // the dialog is gone and the page's own alert carries it.
  await expect(dialog).toBeHidden();
  expect(started).toBe(0);
  page.off("download", count);
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
    await expectNoViolations(new AxeBuilder({ page }), path);
  }
});

test("axe_reports_no_violations_with_each_destructive_dialog_open", async () => {
  const scan = async (name: string) => {
    const dialog = page.getByRole("dialog", { name, exact: true });
    await expect(dialog).toBeVisible();
    await expectNoViolations(new AxeBuilder({ page }), name);
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
  await page.getByLabel("Tag to add or remove", { exact: true }).selectOption({ label: "crowd" });
  await page.getByRole("button", { name: "Add tag", exact: true }).click();
  await scan("Tag these contacts");
  await page.getByRole("button", { name: "Delete contacts", exact: true }).click();
  await scan("Delete these contacts");
  // With the selection still in hand, so the paragraph that disowns it is scanned too.
  await page.getByRole("button", { name: "Export CSV", exact: true }).click();
  await scan("Export these contacts");
});
