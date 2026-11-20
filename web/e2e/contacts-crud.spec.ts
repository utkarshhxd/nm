// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, type Locator, type Page, expect, test } from "@playwright/test";

import { expectNoViolations } from "./support/axe";
import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/**
 * The write paths: creating, editing, deleting and erasing a contact, creating a list, and
 * managing one contact's memberships and tags from the web app alone. Every assertion that
 * matters is checked twice — on the screen, and against the API, because a screen that says
 * "saved" over an unchanged row is the failure this suite exists to catch.
 *
 * One workspace with an admin, an editor and a viewer, seeded once and shared in order.
 */
test.describe.configure({ mode: "serial" });

const SLUG = "crudco";
const EDITOR = "editor-crud@example.com";
const EDITOR_PASSWORD = "editor-crud-password";
const VIEWER = "viewer-crud@example.com";
const VIEWER_PASSWORD = "viewer-crud-password";

/** The server's limit, and what Task 15 found breaking contact detail at 3,986 px wide. */
const LONG_EMAIL = `${"a".repeat(241)}@example.test`;

let admin: BrowserContext;
let editor: BrowserContext;
let viewer: BrowserContext;
let page: Page;
let editorPage: Page;
let viewerPage: Page;
let workspaceId = "";
let csrf = "";
const ids = { created: "", deleteMe: "", eraseMe: "", long: "", vip: "", list: "" };

const base = () => `/api/v1/workspaces/${workspaceId}`;

async function post(path: string, data?: unknown): Promise<{ id: string }> {
  const res = await admin.request.post(`${base()}${path}`, {
    data,
    headers: { "X-CSRF-Token": csrf },
  });
  expect(res.ok(), `post ${path} -> ${res.status()} ${await res.text()}`).toBe(true);
  return res.status() === 204 ? { id: "" } : ((await res.json()) as { id: string });
}

type ApiContact = { id: string; first_name: string; timezone: string | null; attributes: Record<string, unknown> };

const apiContact = async (id: string) =>
  (await (await admin.request.get(`${base()}/contacts/${id}`)).json()) as ApiContact;

async function findContact(q: string): Promise<ApiContact> {
  const body = (await (await admin.request.get(`${base()}/contacts?q=${q}`)).json()) as {
    data: ApiContact[];
  };
  expect(body.data.length, `no contact matching ${q}`).toBeGreaterThan(0);
  return body.data[0] as ApiContact;
}

const memberships = async (id: string) =>
  ((await (await admin.request.get(`${base()}/contacts/${id}/lists`)).json()) as {
    data: { list_id: string; name: string; status: string }[];
  }).data;

async function signIn(p: Page, email: string, password: string): Promise<void> {
  await p.goto("/login");
  await p.getByLabel("Email", { exact: true }).fill(email);
  await p.getByLabel("Password", { exact: true }).fill(password);
  await p.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(p.getByText(`Signed in as ${email}`)).toBeVisible();
}

async function join(p: Page, role: "editor" | "viewer", email: string, password: string): Promise<void> {
  const invite = (await post("/invites", { email, role })) as unknown as { invite_url: string };
  await p.goto(new URL(invite.invite_url).pathname);
  await p.getByLabel("Your name", { exact: true }).fill(`${role} crud`);
  await p.getByLabel("Password", { exact: true }).fill(password);
  await p.getByRole("button", { name: "Accept", exact: true }).click();
  await expect(p.getByText(`Signed in as ${email}`)).toBeVisible();
}

test.beforeAll(async ({ browser }) => {
  test.setTimeout(180_000);
  admin = await browser.newContext();
  editor = await browser.newContext();
  viewer = await browser.newContext();
  page = await admin.newPage();
  editorPage = await editor.newPage();
  viewerPage = await viewer.newPage();

  await signIn(page, ADMIN_EMAIL, ADMIN_PASSWORD);
  await page.goto("/workspaces/new");
  await page.getByLabel("Slug", { exact: true }).fill(SLUG);
  await page.getByLabel("Workspace name", { exact: true }).fill("Crud Co");
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

  await join(editorPage, "editor", EDITOR, EDITOR_PASSWORD);
  await join(viewerPage, "viewer", VIEWER, VIEWER_PASSWORD);

  await post("/custom-fields", { key: "company", label: "Company", kind: "text" });
  await post("/custom-fields", { key: "seats", label: "Seats", kind: "number" });
  await post("/custom-fields", { key: "plan", label: "Plan", kind: "select", options: ["Free", "Pro"] });
  await post("/custom-fields", {
    key: "interests",
    label: "Interests",
    kind: "multi_select",
    options: ["Email", "SMS"],
  });
  ids.vip = (await post("/tags", { name: "vip" })).id;

  ids.deleteMe = (await post("/contacts", { email: "delete-me@example.test" })).id;
  ids.eraseMe = (await post("/contacts", { email: "erase-me@example.test" })).id;
  ids.long = (await post("/contacts", { email: LONG_EMAIL })).id;
});

test.afterAll(async () => {
  await admin.close();
  await editor.close();
  await viewer.close();
});

const contactsPath = `/w/${SLUG}/contacts`;
const contactPath = (id: string) => `${contactsPath}/${id}`;
const listsPath = `/w/${SLUG}/lists`;
// Takes a Locator as well as a Page, because the contact page has more than one table that
// names a list: the memberships and, since M1d, the consent history. A page-wide row filter
// there matches both and fails in strict mode.
const row = (p: Page | Locator, text: string) => p.getByRole("row").filter({ hasText: text });
const dialog = (p: Page, name: string) => p.getByRole("dialog", { name, exact: true });

test("create_a_contact_then_find_it_in_the_grid", async () => {
  await page.goto(contactsPath);
  await page.getByRole("button", { name: "New contact", exact: true }).click();

  const form = dialog(page, "New contact");
  await form.getByRole("textbox", { name: "Email", exact: true }).fill("Created@Example.test");
  await form.getByLabel("First name", { exact: true }).fill("Ada");
  await form.getByLabel("Last name", { exact: true }).fill("Lovelace");
  await form.getByLabel("Time zone", { exact: true }).selectOption("Europe/London");
  // The custom fields are part of the form: the server validates them, so a form that cannot
  // set them cannot create the contacts this workspace actually needs.
  await form.getByLabel("Company", { exact: true }).fill("Acme Ltd");
  await form.getByLabel("Seats", { exact: true }).fill("1250");
  await form.getByLabel("Plan", { exact: true }).selectOption("Pro");
  await form.getByRole("checkbox", { name: "SMS", exact: true }).check();
  await form.getByRole("button", { name: "Create contact", exact: true }).click();

  await expect(form).toBeHidden();
  await expect(row(page, "created@example.test")).toBeVisible();

  const contact = await findContact("created");
  ids.created = contact.id;
  expect(contact.first_name).toBe("Ada");
  expect(contact.timezone).toBe("Europe/London");
  expect(contact.attributes).toMatchObject({ company: "Acme Ltd", seats: 1250, plan: "Pro", interests: ["SMS"] });
});

test("a_rejected_create_says_which_field_was_wrong", async () => {
  await page.goto(contactsPath);
  await page.getByRole("button", { name: "New contact", exact: true }).click();
  const form = dialog(page, "New contact");
  await form.getByRole("textbox", { name: "Email", exact: true }).fill("created@example.test");
  await form.getByRole("button", { name: "Create contact", exact: true }).click();
  // Next to the field that caused it, not a toast and not a dead dialog.
  await expect(form.getByRole("alert")).toContainText(/already/i);
  await expect(form.getByRole("textbox", { name: "Email", exact: true })).toHaveAttribute("aria-invalid", "true");
  await page.keyboard.press("Escape");
  await expect(form).toBeHidden();
});

test("edit_a_contact_and_see_the_change", async () => {
  await page.goto(contactPath(ids.created));
  await page.getByRole("button", { name: "Edit contact", exact: true }).click();
  const form = dialog(page, "Edit contact");

  // The address is not editable: `PATCH` has no `email`, so a box that looked editable would lie.
  await expect(form.getByRole("textbox", { name: "Email", exact: true })).toHaveCount(0);
  await expect(form).toContainText("cannot be changed");

  await form.getByLabel("First name", { exact: true }).fill("Grace");
  // Emptying an optional field must clear it, which takes an explicit `null`, not `""`.
  await form.getByLabel("Time zone", { exact: true }).selectOption("");
  await form.getByLabel("Company", { exact: true }).fill("");
  await form.getByRole("button", { name: "Save contact", exact: true }).click();
  await expect(form).toBeHidden();

  await expect(page.getByText("Grace Lovelace")).toBeVisible();
  const contact = await apiContact(ids.created);
  expect(contact.first_name).toBe("Grace");
  expect(contact.timezone).toBeNull();
  // Removed, not stored as an empty string: `""` is a value, and a value is not "cleared".
  expect(contact.attributes["company"]).toBeUndefined();
  expect(contact.attributes["seats"]).toBe(1250);
});

test("an_edit_sends_only_the_fields_that_changed", async () => {
  // The other half of RFC 7386. `update_contact` writes the list of field names a PATCH sent
  // into the audit log, deliberately without their values, and that log outlives an erasure: a
  // form that resends everything makes every edit look like it touched every field, and the
  // record of what a person actually did is then wrong.
  await page.goto(contactPath(ids.created));

  async function save(change: (form: ReturnType<typeof dialog>) => Promise<void>) {
    await page.getByRole("button", { name: "Edit contact", exact: true }).click();
    const form = dialog(page, "Edit contact");
    await change(form);
    const sent = page.waitForRequest(
      (r) => r.method() === "PATCH" && r.url().includes(`/contacts/${ids.created}`),
    );
    await form.getByRole("button", { name: "Save contact", exact: true }).click();
    const body = JSON.parse((await sent).postData() ?? "{}") as Record<string, unknown>;
    await expect(form).toBeHidden();
    return body;
  }

  const name = await save((form) => form.getByLabel("First name", { exact: true }).fill("Ada"));
  expect(Object.keys(name)).toEqual(["first_name"]);
  expect(name["first_name"]).toBe("Ada");

  // One custom field, and only that key inside `attributes`: the merge patch is per key.
  const seats = await save((form) => form.getByLabel("Seats", { exact: true }).fill("7"));
  expect(Object.keys(seats)).toEqual(["attributes"]);
  expect(seats["attributes"]).toEqual({ seats: 7 });

  expect((await apiContact(ids.created)).attributes["seats"]).toBe(7);
});

test("delete_a_contact_and_it_leaves_the_grid", async () => {
  await page.goto(contactPath(ids.deleteMe));
  await page.getByRole("button", { name: "Delete contact", exact: true }).click();
  const confirm = dialog(page, "Delete this contact");
  await expect(confirm).toContainText("delete-me@example.test");
  // Delete is not erasure: it blocks nothing, and the copy must not imply that it does.
  await expect(confirm).toContainText("not suppressed");
  await expect(confirm).toContainText("can be added again");
  await confirm.getByRole("button", { name: "Delete contact", exact: true }).click();

  await expect(page).toHaveURL(new RegExp(`${contactsPath}$`));
  await expect(row(page, "delete-me@example.test")).toHaveCount(0);

  const suppressions = (await (await admin.request.get(`${base()}/suppressions`)).json()) as {
    data: { email: string | null }[];
  };
  expect(suppressions.data.map((s) => s.email)).not.toContain("delete-me@example.test");
  // The promise the dialog made: the address is usable again.
  const again = await admin.request.post(`${base()}/contacts`, {
    data: { email: "delete-me@example.test" },
    headers: { "X-CSRF-Token": csrf },
  });
  expect(again.status()).toBe(201);
});

test("erase_a_contact_is_admin_only_and_names_what_it_destroys", async () => {
  await page.goto(contactPath(ids.eraseMe));
  await page.getByRole("button", { name: "Erase contact", exact: true }).click();
  const confirm = dialog(page, "Erase this contact");
  await expect(confirm).toContainText("erase-me@example.test");
  await expect(confirm).toContainText("list memberships");
  await expect(confirm).toContainText("tags");
  await expect(confirm).toContainText("permanently blocked");
  await expect(confirm).toContainText("cannot be undone");
  await expect(confirm).toContainText(/no one can unblock|cannot be unblocked|cannot be lifted/i);
  await confirm.getByRole("button", { name: "Erase contact permanently", exact: true }).click();

  await expect(page).toHaveURL(new RegExp(`${contactsPath}$`));
  await expect(row(page, "erase-me@example.test")).toHaveCount(0);

  // The address is blocked for good: adding it again is refused, and the block cannot be lifted.
  const again = await admin.request.post(`${base()}/contacts`, {
    data: { email: "erase-me@example.test" },
    headers: { "X-CSRF-Token": csrf },
  });
  expect(again.status()).toBe(409);
  await page.goto(`/w/${SLUG}/suppressions`);
  const tomb = row(page, "Erased on request (GDPR)");
  await expect(tomb).toContainText("cannot be lifted");
  await expect(tomb.getByRole("button")).toHaveCount(0);
});

test("an_editor_sees_no_erase_control", async () => {
  await editorPage.goto(contactPath(ids.created));
  await expect(editorPage.getByRole("heading", { name: "created@example.test", exact: true })).toBeVisible();
  await expect(editorPage.getByRole("button", { name: /erase/i })).toHaveCount(0);
  // Everything an editor may do is still there.
  await expect(editorPage.getByRole("button", { name: "Edit contact", exact: true })).toBeVisible();
  await expect(editorPage.getByRole("button", { name: "Delete contact", exact: true })).toBeVisible();
  await editorPage.goto(contactsPath);
  await expect(editorPage.getByRole("button", { name: "New contact", exact: true })).toBeVisible();
});

test("a_viewer_sees_no_write_control_at_all", async () => {
  await viewerPage.goto(contactsPath);
  await expect(viewerPage.getByRole("button", { name: "New contact", exact: true })).toHaveCount(0);
  await viewerPage.goto(contactPath(ids.created));
  await expect(viewerPage.getByRole("heading", { name: "created@example.test", exact: true })).toBeVisible();
  for (const name of [/edit/i, /delete/i, /erase/i, /add to list/i, /unsubscribe/i, /apply tag/i]) {
    await expect(viewerPage.getByRole("button", { name })).toHaveCount(0);
  }
  await viewerPage.goto(listsPath);
  await expect(viewerPage.getByRole("button", { name: "New list", exact: true })).toHaveCount(0);
});

test("create_a_list_then_add_a_contact_to_it", async () => {
  await page.goto(listsPath);
  await page.getByRole("button", { name: "New list", exact: true }).click();
  const form = dialog(page, "New list");
  await form.getByLabel("Name", { exact: true }).fill("Product news");
  await form.getByLabel("Description", { exact: true }).fill("Releases and changes");
  // Single opt-in, so the member is subscribed at once rather than pending for ever.
  await form.getByLabel("Double opt-in", { exact: true }).uncheck();
  await form.getByRole("button", { name: "Create list", exact: true }).click();
  await expect(form).toBeHidden();
  await expect(row(page, "Product news")).toContainText("Single opt-in");

  await page.goto(contactPath(ids.created));
  const lists = page.getByRole("region", { name: "Lists", exact: true });
  await lists.getByLabel("List to add", { exact: true }).selectOption({ label: "Product news" });
  await lists.getByRole("button", { name: "Add to list", exact: true }).click();
  await expect(row(lists, "Product news")).toContainText("Subscribed");
  const joined = await memberships(ids.created);
  expect(joined).toMatchObject([{ name: "Product news", status: "subscribed" }]);
  ids.list = joined[0]?.list_id ?? "";
  expect(ids.list).not.toBe("");
});

test("remove_a_membership_leaves_it_unsubscribed_not_deleted", async () => {
  await page.goto(contactPath(ids.created));
  const lists = page.getByRole("region", { name: "Lists", exact: true });
  // The control says what it does. "Remove" would be a lie: the row stays.
  await expect(lists.getByRole("button", { name: /^remove/i })).toHaveCount(0);
  await expect(lists).toContainText("not deleted");

  await lists.getByRole("button", { name: "Unsubscribe Product news", exact: true }).click();
  await lists.getByRole("button", { name: "Confirm unsubscribe Product news", exact: true }).click();

  // Still there, as unsubscribed — on the screen and in the database.
  await expect(row(lists, "Product news")).toContainText("Unsubscribed");
  expect(await memberships(ids.created)).toMatchObject([
    { name: "Product news", status: "unsubscribed" },
  ]);
});

test("apply_and_remove_a_tag_on_one_contact", async () => {
  // By way of the tags screen, whose query holds the same cache key as this picker's: a paged
  // query's data under that key would reach the picker as its rows and break it on arrival.
  await page.goto(`/w/${SLUG}/tags`);
  await expect(row(page, "vip")).toBeVisible();
  // In the browser, so the cache the tags screen filled is the cache this page reads.
  await page.getByRole("link", { name: "Contacts", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Contacts", exact: true })).toBeVisible();
  await page.getByRole("link", { name: "created@example.test", exact: true }).click();

  const tags = page.getByRole("region", { name: "Tags", exact: true });
  await tags.getByLabel("Tag to apply", { exact: true }).selectOption({ label: "vip" });
  await tags.getByRole("button", { name: "Apply tag", exact: true }).click();
  // The remove control, which exists only once the contact has the tag: the word "vip" alone is
  // also in the picker, so waiting for that would not wait for the write.
  await expect(tags.getByRole("button", { name: "Remove vip", exact: true })).toBeVisible();

  const applied = (await (await admin.request.get(`${base()}/contacts/${ids.created}/tags`)).json()) as {
    data: { name: string }[];
  };
  expect(applied.data.map((t) => t.name)).toEqual(["vip"]);

  await tags.getByRole("button", { name: "Remove vip", exact: true }).click();
  await expect(tags).toContainText("No tags");
  const after = (await (await admin.request.get(`${base()}/contacts/${ids.created}/tags`)).json()) as {
    data: { name: string }[];
  };
  expect(after.data).toEqual([]);
});

test("axe_reports_no_violations_with_each_dialog_open", async () => {
  const scan = async (name: string) => {
    await expect(dialog(page, name)).toBeVisible();
    await expect(page.getByRole("status").filter({ hasText: /^Loading/ })).toHaveCount(0);
    await expectNoViolations(new AxeBuilder({ page }), name);
    await page.keyboard.press("Escape");
    await expect(dialog(page, name)).toBeHidden();
  };

  await page.goto(contactsPath);
  await page.getByRole("button", { name: "New contact", exact: true }).click();
  await scan("New contact");

  await page.goto(contactPath(ids.created));
  await page.getByRole("button", { name: "Edit contact", exact: true }).click();
  await scan("Edit contact");
  await page.getByRole("button", { name: "Delete contact", exact: true }).click();
  await scan("Delete this contact");
  await page.getByRole("button", { name: "Erase contact", exact: true }).click();
  await scan("Erase this contact");

  await page.goto(listsPath);
  await page.getByRole("button", { name: "New list", exact: true }).click();
  await scan("New list");

  await page.goto(`${listsPath}/${ids.list}`);
  await page.getByRole("button", { name: "Edit list", exact: true }).click();
  await scan("Edit list");
  await page.getByRole("button", { name: "Delete list", exact: true }).click();
  await scan("Delete this list");
});

test("the_dialogs_hold_up_at_320px_with_a_254_character_address", async () => {
  await page.setViewportSize({ width: 320, height: 640 });
  try {
    await page.goto(contactPath(ids.long));
    for (const [open, name] of [
      ["Edit contact", "Edit contact"],
      ["Delete contact", "Delete this contact"],
      ["Erase contact", "Erase this contact"],
    ] as const) {
      await page.getByRole("button", { name: open, exact: true }).click();
      const box = dialog(page, name);
      await expect(box).toBeVisible();
      // Nothing runs off the side, and the dialog itself fits the viewport.
      const problems = await page.evaluate(() => {
        const root = document.documentElement;
        const d = document.querySelector("dialog[open]") as HTMLElement | null;
        const out: string[] = [];
        if (root.scrollWidth > root.clientWidth) out.push(`page scrolls sideways: ${root.scrollWidth}`);
        if (d && d.getBoundingClientRect().right > root.clientWidth + 1) out.push("dialog is wider than the viewport");
        if (d && d.scrollWidth > d.clientWidth + 1) out.push(`dialog content is ${d.scrollWidth} in ${d.clientWidth}`);
        return out;
      });
      expect(problems, name).toEqual([]);
      await page.keyboard.press("Escape");
      await expect(box).toBeHidden();
    }
  } finally {
    await page.setViewportSize({ width: 1280, height: 720 });
  }
});

// Last, because it destroys the list the tests above built on.
test("rename_a_list_then_delete_it_and_say_what_goes_with_it", async () => {
  await page.goto(`${listsPath}/${ids.list}`);
  await page.getByRole("button", { name: "Edit list", exact: true }).click();
  const form = dialog(page, "Edit list");
  await form.getByLabel("Name", { exact: true }).fill("Product updates");
  await form.getByRole("button", { name: "Save list", exact: true }).click();
  await expect(form).toBeHidden();
  await expect(page.getByRole("heading", { name: "Product updates", exact: true })).toBeVisible();

  // The unsubscribed membership from the earlier test is still here, before the delete.
  expect(await memberships(ids.created)).toMatchObject([{ status: "unsubscribed" }]);

  // Nothing asks the server about the list after it is gone. Refreshing this page's own queries
  // before leaving it refetches the list, its count and its members from a URL that now 404s.
  const missing: string[] = [];
  page.on("response", (r) => {
    if (r.status() === 404 && r.url().includes(`/lists/${ids.list}`)) missing.push(r.url());
  });

  await page.getByRole("button", { name: "Delete list", exact: true }).click();
  const confirm = dialog(page, "Delete this list");
  await expect(confirm).toContainText("Product updates");
  await expect(confirm).toContainText("every membership");
  // What it really costs: the opt-out recorded against this list goes with the list.
  await expect(confirm).toContainText("unsubscribed");
  await expect(confirm).toContainText("No contact is deleted");
  await confirm.getByRole("button", { name: "Delete list", exact: true }).click();

  await expect(page).toHaveURL(new RegExp(`${listsPath}$`));
  await expect(row(page, "Product updates")).toHaveCount(0);
  expect(missing).toEqual([]);
  expect(await memberships(ids.created)).toEqual([]);
  // The contact itself survived.
  expect((await apiContact(ids.created)).id).toBe(ids.created);
});
