// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, type Page, expect, test } from "@playwright/test";

import { expectNoViolations } from "./support/axe";
import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/**
 * The import wizard's first half (spec M1c section 13): the upload, then a mapping table with each
 * column's proposed target and a live sample from the file. The sample is what lets an operator
 * catch a wrong mapping before it runs, so these tests read it back from the screen.
 *
 * Its own file, with its own workspace, and not part of `audience-admin.spec.ts`: that file sits at
 * exactly the per-minute selection limit, and the next selection there answers 429.
 */
test.describe.configure({ mode: "serial" });

const SLUG = "importco";
const VIEWER = "viewer-import@example.com";
const VIEWER_PASSWORD = "viewer-import-password";

let admin: BrowserContext;
let viewer: BrowserContext;
let page: Page;
let viewerPage: Page;
let workspaceId = "";
let csrf = "";

async function signIn(p: Page, email: string, password: string): Promise<void> {
  await p.goto("/login");
  await p.getByLabel("Email", { exact: true }).fill(email);
  await p.getByLabel("Password", { exact: true }).fill(password);
  await p.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(p.getByText(`Signed in as ${email}`)).toBeVisible();
}

const csv = (text: string, name = "people.csv") => ({
  name,
  mimeType: "text/csv",
  buffer: Buffer.from(text),
});

/** Uploads `file` from the import screen and waits for the mapping screen. */
async function upload(file: ReturnType<typeof csv>): Promise<void> {
  await page.goto(`/w/${SLUG}/imports`);
  await expect(page.getByRole("heading", { name: "Import contacts", exact: true })).toBeVisible();
  await page.getByLabel("CSV file", { exact: true }).setInputFiles(file);
  await page.getByRole("button", { name: "Upload", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Map the columns", exact: true })).toBeVisible();
}

const PEOPLE = [
  "Email Address,First Name,Company,Notes",
  "ada@example.com,Ada,Analytical Engines,",
  "grace@example.com,Grace,Navy,first compiler",
  "",
].join("\n");

/** The row of the mapping table for one CSV column. */
const row = (column: string) =>
  page.getByRole("row").filter({ has: page.getByRole("rowheader", { name: column, exact: true }) });
const target = (column: string) =>
  page.getByLabel(`Import column ${column} as`, { exact: true });

test.beforeAll(async ({ browser }) => {
  test.setTimeout(180_000);
  admin = await browser.newContext();
  viewer = await browser.newContext();
  page = await admin.newPage();
  viewerPage = await viewer.newPage();

  await signIn(page, ADMIN_EMAIL, ADMIN_PASSWORD);
  await page.goto("/workspaces/new");
  await page.getByLabel("Slug", { exact: true }).fill(SLUG);
  await page.getByLabel("Workspace name", { exact: true }).fill("Import Co");
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

  const base = `/api/v1/workspaces/${workspaceId}`;
  const send = async (path: string, data: unknown) => {
    const res = await admin.request.post(`${base}${path}`, {
      data,
      headers: { "X-CSRF-Token": csrf },
    });
    expect(res.ok(), `post ${path} -> ${res.status()} ${await res.text()}`).toBe(true);
    return (await res.json()) as { invite_url: string };
  };
  await send("/custom-fields", { key: "company", label: "Company", kind: "text" });
  const invite = await send("/invites", { email: VIEWER, role: "viewer" });
  await viewerPage.goto(new URL(invite.invite_url).pathname);
  await viewerPage.getByLabel("Your name", { exact: true }).fill("viewer import");
  await viewerPage.getByLabel("Password", { exact: true }).fill(VIEWER_PASSWORD);
  await viewerPage.getByRole("button", { name: "Accept", exact: true }).click();
  await expect(viewerPage.getByText(`Signed in as ${VIEWER}`)).toBeVisible();
});

test.afterAll(async () => {
  await admin.close();
  await viewer.close();
});

test("the mapping table has one row per column, each with a proposal and a sample", async () => {
  await upload(csv(PEOPLE));

  await expect(page.getByRole("row")).toHaveCount(5); // the header and four columns
  await expect(row("Email Address")).toContainText("ada@example.com");
  await expect(row("Email Address")).toContainText("grace@example.com");
  await expect(row("Notes")).toContainText("first compiler");
  // A blank is not a sample: the first Notes cell is empty and only the second one shows.
  await expect(row("Notes").getByRole("listitem")).toHaveCount(1);

  // The server proposes only what it is sure of: `First Name` is `first_name`, `Company` is the
  // custom field, and `Email Address` is nothing it will claim.
  await expect(target("First Name")).toHaveValue("field:first_name");
  await expect(target("Company")).toHaveValue("custom:company");
  await expect(target("Email Address")).toHaveValue("ignore");
  await expect(target("Notes")).toHaveValue("ignore");

  await expect(page.getByText("A blank cell leaves the existing value alone.")).toBeVisible();
});

test("a mapping with no email column cannot be reviewed, and says why", async () => {
  await upload(csv(PEOPLE));
  await expect(page.getByRole("button", { name: "Review mapping", exact: true })).toBeDisabled();
  await expect(page.getByText("Map one column to Email.")).toBeVisible();

  await target("Email Address").selectOption({ label: "Email" });
  await target("First Name").selectOption({ label: "Email" });
  await expect(page.getByText("Email is chosen for 2 columns.")).toBeVisible();
  await expect(page.getByRole("button", { name: "Review mapping", exact: true })).toBeDisabled();
});

test("a changed mapping is what the confirmation step shows, and survives going back", async () => {
  await upload(csv(PEOPLE));
  await target("Email Address").selectOption({ label: "Email" });
  await target("Notes").selectOption({ label: "Tags" });
  await page.getByRole("button", { name: "Review mapping", exact: true }).click();

  await expect(page.getByRole("heading", { name: "Review the mapping", exact: true })).toBeVisible();
  const review = page.getByRole("table", { name: "Mapping to review" });
  await expect(review.getByRole("row", { name: /Email Address.*Email/ })).toBeVisible();
  await expect(review.getByRole("row", { name: /First Name.*First name/ })).toBeVisible();
  await expect(review.getByRole("row", { name: /Company.*Company/ })).toBeVisible();
  await expect(review.getByRole("row", { name: /Notes.*Tags/ })).toBeVisible();
  await expect(page.getByText("A blank cell leaves the existing value alone.")).toBeVisible();

  await page.getByRole("button", { name: "Back to mapping", exact: true }).click();
  await expect(target("Email Address")).toHaveValue("field:email");
  await expect(target("Notes")).toHaveValue("field:tags");
});

test("a refusal from the server is shown as its own sentence", async () => {
  await page.goto(`/w/${SLUG}/imports`);
  await page.getByLabel("CSV file", { exact: true }).setInputFiles(csv("", "empty.csv"));
  await page.getByRole("button", { name: "Upload", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("no rows");
  await expect(page.getByRole("heading", { name: "Map the columns", exact: true })).toHaveCount(0);
});

test("a reload of the mapping screen says the upload is gone rather than showing nothing", async () => {
  await upload(csv(PEOPLE));
  await page.reload();
  await expect(page.getByRole("heading", { name: "This upload is not open here", exact: true })).toBeVisible();
  await page.getByRole("link", { name: "Upload a file", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Import contacts", exact: true })).toBeVisible();
});

test("a viewer is told importing takes editor, and is not offered the form", async () => {
  await viewerPage.goto(`/w/${SLUG}/imports`);
  await expect(viewerPage.getByRole("heading", { name: "Importing takes editor", exact: true })).toBeVisible();
  await expect(viewerPage.getByLabel("CSV file", { exact: true })).toHaveCount(0);
});

test("worst-case names and samples keep the screens inside 320 px and axe-clean in both themes", async () => {
  const long = "x".repeat(200);
  await page.setViewportSize({ width: 320, height: 640 });
  try {
    for (const theme of ["light", "dark"] as const) {
      await page.goto("/login");
      await page.evaluate((t) => localStorage.setItem("theme", t), theme);
      await upload(csv(`${long},Email\n${long},ada@example.com\n`, `${long}.csv`));
      await expect(page.locator("html")).toHaveAttribute("data-theme", theme);

      for (const where of ["mapping", "review"]) {
        if (where === "review") {
          await target("Email").selectOption({ label: "Email" });
          await page.getByRole("button", { name: "Review mapping", exact: true }).click();
          await expect(page.getByRole("heading", { name: "Review the mapping", exact: true })).toBeVisible();
        }
        const widths = await page.evaluate(() => ({
          page: document.documentElement.scrollWidth,
          width: document.documentElement.clientWidth,
        }));
        expect(widths.page, `${where} (${theme}) scrolls sideways`).toBeLessThanOrEqual(widths.width);
        await expectNoViolations(new AxeBuilder({ page }), `${where} (${theme})`);
      }
    }
  } finally {
    await page.setViewportSize({ width: 1280, height: 720 });
  }
});
