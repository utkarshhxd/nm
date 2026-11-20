// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, type Page, expect, test } from "@playwright/test";

import { expectNoViolations } from "./support/axe";
import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/**
 * The forms admin screens: the list, the editor, the embed snippet, and deleting a form.
 *
 * **Two workspaces, both with a form slugged `newsletter`.** A form slug is unique per workspace
 * and not globally, so the embed snippet has to name the workspace as well — and a snippet
 * carrying the wrong one posts a visitor's address into another tenant's list, with nothing on
 * the operator's own page to show it. With one workspace a snippet built from the workspace on
 * screen and a snippet built from "the first workspace this account has" are the same string, so
 * there are two.
 */
test.describe.configure({ mode: "serial" });

/**
 * Its own per-IP rate-limit budget: this file posts the snippet to `POST /f/…`, which is one of
 * the five public routes limited at `PUBLIC_PER_MINUTE` per address. See the
 * `NM_TRUST_PROXY_HEADERS` comment in `support/server.ts`. **One** request of the 60 is spent
 * against this address; everything else here is the authenticated admin UI, which is limited per
 * workspace and does not touch this budget.
 */
test.use({ extraHTTPHeaders: { "x-forwarded-for": "198.51.100.10" } });

const SLUG = "formsadmin";
const RIVAL = "formsrival";

let context: BrowserContext;
let page: Page;
let csrf = "";
const ids = { ws: "", rival: "", rivalForm: "", list: "" };

async function post(workspaceId: string, path: string, data: unknown): Promise<{ id: string }> {
  const res = await context.request.post(`/api/v1/workspaces/${workspaceId}${path}`, {
    data,
    headers: { "X-CSRF-Token": csrf },
  });
  expect(res.ok(), `post ${path} -> ${res.status()} ${await res.text()}`).toBe(true);
  return (await res.json()) as { id: string };
}

/** Creates a workspace through the UI, the way the other admin specs do, and returns its id. */
async function workspace(slug: string, name: string): Promise<string> {
  await page.goto("/workspaces/new");
  await page.getByLabel("Slug", { exact: true }).fill(slug);
  await page.getByLabel("Workspace name", { exact: true }).fill(name);
  await page.getByLabel("Time zone", { exact: true }).selectOption("Europe/London");
  await page.getByRole("button", { name: "Create workspace", exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/w/${slug}/members$`));
  const me = (await (await context.request.get("/api/v1/me")).json()) as {
    csrf_token: string;
    workspaces: { id: string; slug: string }[];
  };
  csrf = me.csrf_token;
  const id = me.workspaces.find((w) => w.slug === slug)?.id ?? "";
  expect(id, `the signed-in account is a member of ${slug}`).not.toBe("");
  return id;
}

const snippet = () => page.getByLabel("Embed snippet", { exact: true });

/** Opens the one form this file creates through the UI, from the list screen. */
async function openTheForm(): Promise<void> {
  await page.goto(`/w/${SLUG}/forms`);
  await page.getByRole("link", { name: "Weekly newsletter", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Weekly newsletter", exact: true })).toBeVisible();
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

  ids.ws = await workspace(SLUG, "Forms Admin Co");
  ids.list = (await post(ids.ws, "/lists", { name: "News", double_optin: false })).id;

  // The other tenant, seeded over the API: what matters about it is that its form carries the
  // same slug as the one created through the UI below.
  ids.rival = await workspace(RIVAL, "Rival Co");
  const rivalList = (await post(ids.rival, "/lists", { name: "Rival news" })).id;
  ids.rivalForm = (
    await post(ids.rival, "/forms", {
      slug: "newsletter",
      name: "Rival newsletter",
      list_ids: [rivalList],
      fields: [{ target: { kind: "email" }, required: true, label: "Email address" }],
      headline: "Join Rival Co",
      button_label: "Join",
    })
  ).id;
});

test.afterAll(async () => {
  await context.close();
});

test("a_form_is_created_from_the_screen_and_appears_in_the_list", async () => {
  await page.goto(`/w/${SLUG}/forms`);
  await expect(page.getByText("No forms yet")).toBeVisible();

  await page.getByRole("button", { name: "New form", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "New form", exact: true });
  await dialog.getByLabel("Name", { exact: true }).fill("Weekly newsletter");
  await dialog.getByLabel("URL slug", { exact: true }).fill("newsletter");
  await dialog.getByLabel("Headline", { exact: true }).fill("Join the list");
  await dialog.getByLabel("Button label", { exact: true }).fill("Subscribe");
  await dialog.getByRole("checkbox", { name: "News", exact: true }).check();
  await dialog.getByRole("checkbox", { name: "Collect First name", exact: true }).check();
  await dialog.getByRole("button", { name: "Create form", exact: true }).click();

  await expect(page.getByRole("row").filter({ hasText: "Weekly newsletter" })).toBeVisible();
  await expect(page.getByText("No forms yet")).toHaveCount(0);
});

test("the_snippet_posts_to_this_workspace_and_the_server_accepts_what_it_sends", async () => {
  await openTheForm();

  const html = await snippet().inputValue();
  expect(html).toContain(`action="http://localhost:8090/f/${SLUG}/newsletter"`);

  // The snippet is pasted onto the operator's own site, so nothing of ours is there to help it:
  // its `action` and its input names are the whole contract with `POST /f/{workspace}/{slug}`.
  // Submitting exactly what the snippet declares is the only check that the two still agree — a
  // renamed input on either side leaves a form that looks right and collects nothing.
  const action = /action="([^"]+)"/.exec(html)?.[1] ?? "";
  const names = [...html.matchAll(/name="([^"]+)"/g)].map((m) => m[1] ?? "");
  expect(names).toContain("email");
  expect(names).toContain("first_name");
  // The decoy a script fills in and a person never does; an embedder gets it too.
  expect(names).toContain("website");

  const body: Record<string, string> = {};
  for (const name of names) body[name] = "";
  body["email"] = "embedded@example.test";
  body["first_name"] = "Emmy";
  const submitted = await context.request.post(action, { form: body });
  expect(submitted.status(), await submitted.text()).toBe(200);

  const found = (await (
    await context.request.get(
      `/api/v1/workspaces/${ids.ws}/contacts?q=${encodeURIComponent("embedded@example.test")}`,
    )
  ).json()) as { data: { id: string; first_name: string }[] };
  expect(found.data.length, "the submission reached this workspace").toBe(1);
  expect(found.data[0]?.first_name).toBe("Emmy");
  const memberships = (await (
    await context.request.get(`/api/v1/workspaces/${ids.ws}/contacts/${found.data[0]?.id}/lists`)
  ).json()) as { data: { list_id: string; status: string }[] };
  expect(memberships.data.map((m) => [m.list_id, m.status])).toEqual([[ids.list, "subscribed"]]);
});

test("each_workspaces_snippet_names_its_own_workspace", async () => {
  await openTheForm();
  const mine = await snippet().inputValue();

  await page.goto(`/w/${RIVAL}/forms/${ids.rivalForm}`);
  await expect(page.getByRole("heading", { name: "Rival newsletter", exact: true })).toBeVisible();
  const theirs = await snippet().inputValue();

  // Both forms are slugged `newsletter`, so only the workspace segment tells the two apart.
  expect(mine).toContain(`/f/${SLUG}/newsletter"`);
  expect(theirs).toContain(`/f/${RIVAL}/newsletter"`);
  expect(mine).not.toContain(`/f/${RIVAL}/`);
  expect(theirs).not.toContain(`/f/${SLUG}/`);
});

test("editing_a_form_changes_what_the_snippet_posts_to", async () => {
  await openTheForm();
  await page.getByRole("button", { name: "Edit form", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Edit form", exact: true });
  await dialog.getByLabel("URL slug", { exact: true }).fill("signup");
  await dialog.getByLabel("Headline", { exact: true }).fill("Sign up for the weekly");
  await dialog.getByRole("button", { name: "Save form", exact: true }).click();

  await expect(snippet()).toHaveValue(
    new RegExp(`action="http://localhost:8090/f/${SLUG}/signup"`),
  );
  await expect(page.getByText("Sign up for the weekly")).toBeVisible();
});

test("axe_reports_no_violations_on_the_forms_screens", async () => {
  for (const [path, ready] of [
    [`/w/${SLUG}/forms`, "Weekly newsletter"],
    [`/w/${RIVAL}/forms/${ids.rivalForm}`, "Rival newsletter"],
  ] as const) {
    await page.goto(path);
    await expect(page.getByText(ready).first()).toBeVisible();
    await expect(page.getByRole("status").filter({ hasText: /^Loading/ })).toHaveCount(0);
    await expectNoViolations(new AxeBuilder({ page }), path);
  }
});

test("deleting_a_form_takes_it_off_the_list", async () => {
  await openTheForm();
  await page.getByRole("button", { name: "Delete form", exact: true }).click();
  await page
    .getByRole("dialog", { name: "Delete this form", exact: true })
    .getByRole("button", { name: "Delete form", exact: true })
    .click();

  await expect(page).toHaveURL(new RegExp(`/w/${SLUG}/forms$`));
  await expect(page.getByText("No forms yet")).toBeVisible();
  // The lists a form named are its links, not its property: deleting the form keeps them.
  const lists = (await (
    await context.request.get(`/api/v1/workspaces/${ids.ws}/lists`)
  ).json()) as { data: { id: string }[] };
  expect(lists.data.map((l) => l.id)).toContain(ids.list);
});
