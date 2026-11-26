// SPDX-License-Identifier: AGPL-3.0-only

import { type BrowserContext, type Page, expect, test } from "@playwright/test";

import type { components } from "../src/api/schema";
import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/**
 * The templates list and detail screens against the real API (task W6).
 *
 * What these pin is the part a mock could not: the tree the editor serialises is the tree the
 * server stores and hands back; a merge tag the server will not compile is refused with the
 * server's own sentence; another workspace's template is a 404 and never its data; and a preview
 * that could not be CHECKED is not an empty findings list.
 *
 * One workspace with an instance admin, seeded once and shared in order.
 */
test.describe.configure({ mode: "serial" });

const SLUG = "templatesco";
const SECOND_SLUG = "othertemplco";

/**
 * The preview route, stubbed away in every test that is not about it.
 *
 * A workspace gets 120 template previews a minute. The editor asks again 600 ms after the
 * document settles, so a test that types would otherwise spend the window on a panel it never
 * looks at and fail the tests that do look at it.
 */
const PREVIEW = "**/templates/*/preview";
const EMPTY_PREVIEW = {
  status: 200,
  contentType: "application/json",
  body: JSON.stringify({ html: "<!doctype html><html><body></body></html>", text: "", findings: [] }),
};

/** A `422 template-uncheckable` exactly as `src/error.rs` writes one. */
const UNCHECKABLE = {
  status: 422,
  contentType: "application/problem+json",
  body: JSON.stringify({
    type: "urn:nm:problem:template-uncheckable",
    title: "Unprocessable Entity",
    status: 422,
    detail: "this template is not valid MJML, so it could not be checked: unexpected token",
  }),
};

type TemplateView = components["schemas"]["TemplateView"];

let admin: BrowserContext;
let page: Page;
let workspaceId = "";
let otherWorkspaceId = "";
let csrf = "";
/** The one blocks template these tests edit, created by the first of them through the screen. */
let templateId = "";

const base = (ws = workspaceId) => `/api/v1/workspaces/${ws}`;

async function apiTemplate(id: string, ws = workspaceId): Promise<TemplateView> {
  const res = await admin.request.get(`${base(ws)}/templates/${id}`);
  expect(res.ok(), `get template -> ${res.status()}`).toBe(true);
  return (await res.json()) as TemplateView;
}

async function signIn(p: Page, email: string, password: string): Promise<void> {
  await p.goto("/login");
  await p.getByLabel("Email", { exact: true }).fill(email);
  await p.getByLabel("Password", { exact: true }).fill(password);
  await p.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(p.getByText(`Signed in as ${email}`)).toBeVisible();
}

async function createWorkspace(slug: string, name: string): Promise<void> {
  await page.goto("/workspaces/new");
  await page.getByLabel("Slug", { exact: true }).fill(slug);
  await page.getByLabel("Workspace name", { exact: true }).fill(name);
  await page.getByLabel("Time zone", { exact: true }).selectOption("Europe/London");
  await page.getByRole("button", { name: "Create workspace", exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/w/${slug}/members$`));
}

const openTemplates = async (): Promise<void> => {
  await page.goto(`/w/${SLUG}/templates`);
  await expect(page.getByRole("heading", { name: "Templates", exact: true })).toBeVisible();
};

const openTemplate = async (query = ""): Promise<void> => {
  await page.goto(`/w/${SLUG}/templates/${templateId}${query}`);
  await expect(page.getByRole("button", { name: "Save template", exact: true })).toBeVisible();
};

const textBox = (name: string) => page.getByRole("textbox", { name, exact: true });

test.beforeAll(async ({ browser }) => {
  test.setTimeout(180_000);
  admin = await browser.newContext();
  page = await admin.newPage();

  await signIn(page, ADMIN_EMAIL, ADMIN_PASSWORD);
  await createWorkspace(SLUG, "Templates Co");
  await createWorkspace(SECOND_SLUG, "Other Templates Co");

  const me = (await (await admin.request.get("/api/v1/me")).json()) as {
    csrf_token: string;
    workspaces: { id: string; slug: string }[];
  };
  csrf = me.csrf_token;
  workspaceId = me.workspaces.find((w) => w.slug === SLUG)?.id ?? "";
  otherWorkspaceId = me.workspaces.find((w) => w.slug === SECOND_SLUG)?.id ?? "";
  expect(workspaceId).not.toBe("");
  expect(otherWorkspaceId).not.toBe("");
});

test.afterAll(async () => {
  await admin.close();
});

test.beforeEach(async () => {
  await page.unroute(PREVIEW);
  await page.route(PREVIEW, (route) => route.fulfill(EMPTY_PREVIEW));
});

test("a_template_created_here_round_trips_its_tree_through_the_server", async () => {
  await openTemplates();
  await expect(page.getByText("No templates yet")).toBeVisible();

  await page.getByRole("button", { name: "New template", exact: true }).click();
  await page.getByRole("dialog").getByLabel("Name", { exact: true }).fill("Welcome");
  await page.getByLabel("Kind", { exact: true }).selectOption("blocks");
  await page.getByRole("button", { name: "Create template", exact: true }).click();

  // The create lands on the detail screen for the template the server made.
  await expect(page).toHaveURL(/\/templates\/[0-9a-f-]{36}$/);
  templateId = (page.url().split("/").pop() ?? "").trim();
  expect(templateId).not.toBe("");

  await page.getByRole("button", { name: "Add text", exact: true }).click();
  await textBox("Text block 1").click();
  await page.keyboard.type("Hello there");
  await page.getByRole("button", { name: "Save template", exact: true }).click();
  await expect(page.getByText("Template saved.")).toBeVisible();

  // The server's copy, not the screen's: this is the assertion a mock could not make.
  const stored = await apiTemplate(templateId);
  expect(stored.kind).toBe("blocks");
  expect(stored.tree).toEqual({
    blocks: [{ type: "Text", runs: [{ type: "Text", text: "Hello there", marks: [] }] }],
  });

  // And it comes back into a fresh editor the same way round.
  await openTemplate();
  await expect(textBox("Text block 1")).toHaveText("Hello there");
  await openTemplates();
  await expect(page.getByRole("link", { name: "Welcome", exact: true })).toBeVisible();
});

test("a_merge_tag_naming_an_unknown_field_is_refused_with_the_field_it_names", async () => {
  await openTemplate();
  const before = await apiTemplate(templateId);

  await textBox("Text block 1").click();
  await page.keyboard.press("End");
  await page.getByLabel("Merge tag", { exact: true }).fill("not_a_field");
  await page.getByRole("button", { name: "Insert merge tag", exact: true }).click();
  await page.getByRole("button", { name: "Save template", exact: true }).click();

  // The server's own sentence, which names the field the operator wrote. A generic "that did not
  // work" would leave them hunting through the document for which tag is wrong.
  const alert = page.getByRole("alert");
  await expect(alert).toContainText("not_a_field");
  await expect(alert).toContainText("does not exist");
  await expect(page.getByText("Template saved.")).toBeHidden();

  // Refused whole: the stored document is the one from before the edit.
  expect((await apiTemplate(templateId)).tree).toEqual(before.tree);
});

test("the_document_comes_from_the_server_and_never_from_the_address_bar", async () => {
  // `?doc=` is the e2e editor harness's fixture affordance. It is confined to that build, and a
  // route that honoured it would let a link decide what an operator is editing.
  const planted = JSON.stringify({
    blocks: [{ type: "Text", runs: [{ type: "Text", text: "Planted from the URL", marks: [] }] }],
  });
  await openTemplate(`?doc=${encodeURIComponent(planted)}`);
  await expect(textBox("Text block 1")).toHaveText("Hello there");
  await expect(page.getByText("Planted from the URL")).toBeHidden();
});

test("another_workspaces_template_is_not_found_here_and_its_name_never_arrives", async () => {
  const made = await admin.request.post(`${base(otherWorkspaceId)}/templates`, {
    data: { name: "Secret of the other tenant", kind: "blocks", tree: { blocks: [] } },
    headers: { "X-CSRF-Token": csrf },
  });
  expect(made.ok(), `create -> ${made.status()}`).toBe(true);
  const other = (await made.json()) as TemplateView;

  // The same id, asked for under THIS workspace: 404, never 403 and never the row.
  const refused = await admin.request.get(`${base()}/templates/${other.id}`);
  expect(refused.status()).toBe(404);

  await page.goto(`/w/${SLUG}/templates/${other.id}`);
  await expect(page.getByRole("heading", { name: "Template not found" })).toBeVisible();
  await expect(page.getByText("Secret of the other tenant")).toBeHidden();
});

test("the_preview_shows_the_servers_findings_and_a_check_that_could_not_run_is_not_a_pass", async () => {
  await openTemplate();
  await page.unroute(PREVIEW);

  // A button with no address: the server's link checker has a rule for it, and this is the real
  // one answering, not a stand-in with the same shape.
  await page.getByRole("button", { name: "Add button", exact: true }).click();
  await expect(page.getByRole("list", { name: "Findings" })).toContainText("has no address", {
    timeout: 15_000,
  });
  await expect(page.getByTestId("findings-count")).not.toHaveText("No findings.");
  // `at` is a PATH into the block tree (contract section 12), and this is what it is for: the
  // panel can take the operator to the block the finding is about. A finding that arrived with
  // no path offers no such control, so its presence is the path's.
  await expect(page.getByRole("button", { name: "Show block 2", exact: true })).toBeVisible();

  // Now the checks cannot RUN. An empty findings list would be read as a pass, so this must not
  // be one: the panel says nothing has been checked.
  await page.route(PREVIEW, (route) => route.fulfill(UNCHECKABLE));
  await textBox("Text block 1").click();
  await page.keyboard.type("!");
  await expect(
    page.getByText("Preview and checks unavailable.", { exact: false }),
  ).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId("findings-count")).toBeHidden();
  await expect(page.getByRole("list", { name: "Findings" })).toBeHidden();
});

test("a_hand_written_template_edits_its_source_and_the_kind_cannot_change", async () => {
  await openTemplates();
  await page.getByRole("button", { name: "New template", exact: true }).click();
  await page.getByRole("dialog").getByLabel("Name", { exact: true }).fill("Raw");
  await page.getByLabel("Kind", { exact: true }).selectOption("html");
  await page.getByRole("button", { name: "Create template", exact: true }).click();
  await expect(page).toHaveURL(/\/templates\/[0-9a-f-]{36}$/);
  const id = (page.url().split("/").pop() ?? "").trim();

  await expect(page.getByRole("textbox", { name: "HTML", exact: true })).toBeVisible();
  await page.getByRole("textbox", { name: "HTML", exact: true }).fill("<p>Edited by hand</p>");
  await page.getByRole("button", { name: "Save template", exact: true }).click();
  await expect(page.getByText("Template saved.")).toBeVisible();

  const stored = await apiTemplate(id);
  expect(stored.kind).toBe("html");
  expect(stored.source).toBe("<p>Edited by hand</p>");
  expect(stored.tree).toBeNull();

  // `kind` is not patchable: the server rejects it rather than quietly dropping it.
  const patched = await admin.request.patch(`${base()}/templates/${id}`, {
    data: { kind: "blocks" },
    headers: { "X-CSRF-Token": csrf },
  });
  expect(patched.status()).toBe(422);
});

test("a_hand_written_template_says_its_links_were_not_checked_in_the_real_servers_own_words", async () => {
  // The preview is NOT stubbed here: the sentence on screen is what `preview_template` answers.
  await page.unroute(PREVIEW);
  // A quoted fallback inside an attribute: valid, and refused at preview before the lint fix.
  const MJML = `<mjml><mj-body><mj-section><mj-column><mj-button href="https://x.example/?n={{ first_name | "friend" }}">Go</mj-button></mj-column></mj-section></mj-body></mjml>`;
  for (const [kind, source, said] of [
    ["html", "<p>Hi</p>", "a raw HTML template is not link-checked or style-checked"],
    ["mjml", MJML, "a raw MJML template's links are not checked"],
  ] as const) {
    const created = await admin.request.post(`${base()}/templates`, {
      data: { name: `Real ${kind}`, kind, source },
      headers: { "X-CSRF-Token": csrf },
    });
    expect(created.status(), `create ${kind}`).toBe(201);
    const { id } = (await created.json()) as { id: string };
    await page.goto(`/w/${SLUG}/templates/${id}`);
    const findings = page.getByRole("list", { name: "Findings" });
    await expect(findings).toContainText(said, { timeout: 15_000 });
    await expect(findings).toContainText("Warning");
    await expect(page.getByText("No findings.")).toHaveCount(0);
    await expect(page.getByTestId("preview-links")).toHaveText(
      "Links are checked for form, not for whether they resolve.",
    );
  }
});

test("a_deleted_template_leaves_the_list_and_the_screen_behind_it", async () => {
  await openTemplates();
  await page.getByRole("button", { name: "Delete Raw", exact: true }).click();
  await page.getByRole("button", { name: "Delete template", exact: true }).click();
  await expect(page.getByRole("link", { name: "Raw", exact: true })).toBeHidden();
  await expect(page.getByRole("link", { name: "Welcome", exact: true })).toBeVisible();
});
