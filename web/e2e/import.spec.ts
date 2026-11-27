// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, type Page, type Route, expect, test } from "@playwright/test";
import { readFile } from "node:fs/promises";

import type { components } from "../src/api/schema";
import { expectNoViolations } from "./support/axe";
import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/**
 * The import wizard (spec M1c section 13). First half: the upload, then a mapping table with each
 * column's proposed target and a live sample from the file; the sample is what lets an operator
 * catch a wrong mapping before it runs, so these tests read it back from the screen. Second half:
 * a confirmation that names what will happen, the run, and a report that breaks every count out.
 *
 * Its own file, with its own workspace, and not part of `audience-admin.spec.ts`: that file sits at
 * exactly the per-minute selection limit, and the next selection there answers 429.
 */
test.describe.configure({ mode: "serial" });

const SLUG = "importco";
const VIEWER = "viewer-import@example.com";
const VIEWER_PASSWORD = "viewer-import-password";
const LONG_LIST = "W".repeat(100);
/** At the server's 254-character limit: the one place an address reaches a failed import's reason. */
const LONG_EMAIL = `${"a".repeat(241)}@example.test`;

let admin: BrowserContext;
let viewer: BrowserContext;
let page: Page;
let viewerPage: Page;
let workspaceId = "";
let csrf = "";
const ids = { newsletter: "", decl1: "" };

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
  "Mail,First Name,Company,Notes",
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
    return res.status() === 204 ? { invite_url: "", id: "" } : ((await res.json()) as { invite_url: string; id: string });
  };
  await send("/custom-fields", { key: "company", label: "Company", kind: "text" });
  ids.newsletter = (await send("/lists", { name: "Newsletter", double_optin: false })).id;
  await send("/lists", { name: LONG_LIST, double_optin: true });
  // Contacts that already exist, so a run can update them, skip them or refuse them; one has left
  // the Newsletter, so a run that asks for a subscription is told no.
  for (const email of ["old1@example.com", "old2@example.com", LONG_EMAIL]) {
    await send("/contacts", { email });
  }
  ids.decl1 = (await send("/contacts", { email: "decl1@example.com" })).id;
  await send(`/contacts/${ids.decl1}/lists`, { list_id: ids.newsletter });
  await send(`/contacts/${ids.decl1}/lists/${ids.newsletter}/unsubscribe`, undefined);
  for (const email of ["blocked1@example.com", "blocked2@example.com"]) {
    await send("/suppressions", { scope: "address", email, reason: "unsubscribed_all" });
  }
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
  await expect(row("Mail")).toContainText("ada@example.com");
  await expect(row("Mail")).toContainText("grace@example.com");
  await expect(row("Notes")).toContainText("first compiler");
  // A blank is not a sample: the first Notes cell is empty and only the second one shows.
  await expect(row("Notes").getByRole("listitem")).toHaveCount(1);

  // The server proposes only what it is sure of: `First Name` is `first_name`, `Company` is the
  // custom field, and `Mail` is nothing it will claim.
  await expect(target("First Name")).toHaveValue("field:first_name");
  await expect(target("Company")).toHaveValue("custom:company");
  await expect(target("Mail")).toHaveValue("ignore");
  await expect(target("Notes")).toHaveValue("ignore");

  await expect(page.getByText("A blank cell leaves the existing value alone.")).toBeVisible();
});

test("a mapping with no email column cannot be reviewed, and says why", async () => {
  await upload(csv(PEOPLE));
  await expect(page.getByRole("button", { name: "Review mapping", exact: true })).toBeDisabled();
  await expect(page.getByText("Map one column to Email.")).toBeVisible();
  // Said in a live region, so a screen reader hears the list change when a select does.
  await expect(page.getByRole("status").filter({ hasText: "Map one column to Email." })).toBeVisible();

  await target("Mail").selectOption({ label: "Email" });
  await target("First Name").selectOption({ label: "Email" });
  await expect(page.getByText("Email is chosen for 2 columns.")).toBeVisible();
  await expect(page.getByRole("button", { name: "Review mapping", exact: true })).toBeDisabled();
});

test("a changed mapping is what the confirmation step shows, and survives going back", async () => {
  await upload(csv(PEOPLE));
  await target("Mail").selectOption({ label: "Email" });
  await target("Notes").selectOption({ label: "Tags" });
  await page.getByRole("button", { name: "Review mapping", exact: true }).click();

  await expect(page.getByRole("heading", { name: "Review the mapping", exact: true })).toBeVisible();
  const review = page.getByRole("table", { name: "Mapping to review" });
  await expect(review.getByRole("row", { name: /Mail.*Email/ })).toBeVisible();
  await expect(review.getByRole("row", { name: /First Name.*First name/ })).toBeVisible();
  await expect(review.getByRole("row", { name: /Company.*Company/ })).toBeVisible();
  await expect(review.getByRole("row", { name: /Notes.*Tags/ })).toBeVisible();
  await expect(page.getByText("A blank cell leaves the existing value alone.")).toBeVisible();
  await expect(page.getByText("Every column is imported.")).toBeVisible();

  // A keyboard user's focus must not fall to <body> when the button that was pressed unmounts.
  await expect(page.getByRole("heading", { name: "Review the mapping", exact: true })).toBeFocused();

  await page.getByRole("button", { name: "Back to mapping", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Map the columns", exact: true })).toBeFocused();
  await expect(target("Mail")).toHaveValue("field:email");
  await expect(target("Notes")).toHaveValue("field:tags");
});

test("a refusal from the server is shown as its own sentence", async () => {
  await page.goto(`/w/${SLUG}/imports`);
  await page.getByLabel("CSV file", { exact: true }).setInputFiles(csv("", "empty.csv"));
  await page.getByRole("button", { name: "Upload", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("no rows");
  await expect(page.getByRole("heading", { name: "Map the columns", exact: true })).toHaveCount(0);
});

/** Answers a POST on `glob` with a problem document: a refusal the real server only gives under load. */
async function refuse(glob: string, status: number, code: string): Promise<void> {
  await page.route(glob, (route: Route) =>
    route.request().method() === "POST"
      ? route.fulfill({
          status,
          contentType: "application/problem+json",
          body: JSON.stringify({ type: `urn:nm:problem:${code}`, title: code, status, detail: code.replaceAll("-", " ") }),
        })
      : route.fallback(),
  );
}

test("an upload refused for size or rate says what to do, not just that it failed", async () => {
  const glob = "**/api/v1/workspaces/*/imports";
  for (const [status, code, text] of [
    [422, "too-many-rows", "more than 1,000,000 rows"],
    [429, "rate-limited", "wait a minute"],
  ] as const) {
    await refuse(glob, status, code);
    try {
      await page.goto(`/w/${SLUG}/imports`);
      await page.getByLabel("CSV file", { exact: true }).setInputFiles(csv(PEOPLE));
      await page.getByRole("button", { name: "Upload", exact: true }).click();
      await expect(page.getByRole("alert")).toContainText(text);
    } finally {
      await page.unroute(glob);
    }
  }
});

test("starting an import while one runs says it may be this one, and says the file is fine", async () => {
  await upload(csv(PEOPLE));
  await review("Mail");
  const glob = "**/api/v1/workspaces/*/imports/*/start";
  await refuse(glob, 409, "import-in-progress");
  try {
    await runButton().click();
    // NOT "another import". The run that holds the workspace is commonly the operator's OWN,
    // because the lock is held by a live transaction that has committed nothing — so a 409 after
    // a cut connection is their own run, and a run in progress is in no list until it ends. The
    // old copy sent them looking for somebody else's import.
    await expect(page.getByRole("alert")).toContainText("An import is already running");
    await expect(page.getByRole("alert")).toContainText("It may be this one");
    await expect(page.getByRole("alert")).toContainText("appears in no list until it finishes");
    await expect(page.getByRole("alert")).toContainText("Nothing is wrong with this file");
  } finally {
    await page.unroute(glob);
  }
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
          // Not left as `ignore`: the review table lists only mapped columns, so the long name and
          // sample must be mapped to be rendered there at all.
          await target(long).selectOption({ label: "Tags" });
          await page.getByRole("button", { name: "Review mapping", exact: true }).click();
          await expect(page.getByRole("heading", { name: "Review the mapping", exact: true })).toBeVisible();
          const review = page.getByRole("table", { name: "Mapping to review" });
          await expect(review.getByRole("rowheader", { name: long, exact: true })).toBeVisible();
          // The server cuts a sample to 100 characters (`SAMPLE_CHARS`).
          await expect(review.getByRole("cell", { name: "x".repeat(100), exact: true })).toBeVisible();
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

// ---- The second half: confirmation, run, report. ----

/** Maps `column` to Email on the mapping screen and opens the confirmation. */
async function review(column = "Mail"): Promise<void> {
  await target(column).selectOption({ label: "Email" });
  await page.getByRole("button", { name: "Review mapping", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Review the mapping", exact: true })).toBeVisible();
}

const runButton = () => page.getByRole("button", { name: "Run the import", exact: true });
const listPicker = () => page.getByLabel("Add every contact to a list", { exact: true });
const will = () => page.getByRole("region", { name: "What will happen", exact: true });

/** Runs the import from the confirmation step and waits for the report. */
async function run(): Promise<void> {
  await runButton().click();
  await expect(page.getByRole("heading", { name: "Import report", exact: true })).toBeVisible();
}

/** The value cell of one count in the report. */
const count = (name: string) =>
  page
    .getByRole("row")
    .filter({ has: page.getByRole("rowheader", { name: new RegExp(`^${name}\\b`) }) })
    .getByRole("cell");

const dedup = (name: RegExp) => page.getByRole("radio", { name });
const rejected = () => page.getByRole("table", { name: "Rejected rows", exact: true });

/** RFC 4180 as far as the server writes it: quoted cells, doubled quotes, CRLF or LF. */
function parseCsv(text: string): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  let cell = "";
  let quoted = false;
  for (let i = 0; i < text.length; i++) {
    const c = text.charAt(i);
    if (quoted) {
      if (c === '"' && text.charAt(i + 1) === '"') {
        cell += '"';
        i++;
      } else if (c === '"') quoted = false;
      else cell += c;
    } else if (c === '"') quoted = true;
    else if (c === ",") {
      row.push(cell);
      cell = "";
    } else if (c === "\n" || c === "\r") {
      if (c === "\r" && text.charAt(i + 1) === "\n") i++;
      row.push(cell);
      rows.push(row);
      row = [];
      cell = "";
    } else cell += c;
  }
  if (cell !== "" || row.length > 0) rows.push([...row, cell]);
  return rows;
}

async function downloadedCsv(): Promise<{ name: string; rows: string[][] }> {
  const [download] = await Promise.all([
    page.waitForEvent("download"),
    page.getByRole("button", { name: /^Download the \d[\d,]* listed rows as CSV$/ }).click(),
  ]);
  return {
    name: download.suggestedFilename(),
    rows: parseCsv(await readFile(await download.path(), "utf8")),
  };
}

test("the confirmation names the de-duplication mode and the target list, and follows both choices", async () => {
  await upload(csv(PEOPLE));
  await review();
  await expect(page.getByText("1 column is not imported.")).toBeVisible();

  // The default is named, not implied: an operator about to change a million contacts is told
  // which of the three modes will run.
  await expect(dedup(/^Update it/)).toBeChecked();
  await expect(will()).toContainText("already a contact will be updated");
  await expect(will()).toContainText("blank cell in your file leaves the stored value as it is");
  await expect(will()).toContainText("not added to a list");

  await dedup(/^Skip it/).check();
  await expect(will()).toContainText("already a contact will be skipped");
  await expect(will()).toContainText("left exactly as it is");
  await dedup(/^Stop/).check();
  await expect(will()).toContainText("nothing is imported");

  await listPicker().selectOption({ label: "Newsletter" });
  await expect(will()).toContainText("subscribed to Newsletter");
  await expect(will()).not.toContainText("not added to a list");
  // A double opt-in list subscribes no one on import: it makes them pending.
  await listPicker().selectOption({ label: LONG_LIST });
  await expect(will()).toContainText("pending until they confirm");

  // The choices survive going back to the mapping and forward again.
  await page.getByRole("button", { name: "Back to mapping", exact: true }).click();
  await page.getByRole("button", { name: "Review mapping", exact: true }).click();
  await expect(dedup(/^Stop/)).toBeChecked();
  await expect(listPicker()).toHaveValue(/.+/);
});

test("the report shows every count on its own, and says which two are not rows", async () => {
  const rows = [
    "Mail,Name",
    ...Array.from({ length: 6 }, () => "n1@example.com,N"), // one import and five collapsed
    ...["n2", "n3", "n4", "n5", "blocked1", "blocked2"].map((n) => `${n}@example.com,N`),
    ...["old1", "old2", "decl1"].map((n) => `${n}@example.com,Existing`),
    ...["bad one", "bad two", "bad three", "bad four"].map((n) => `${n},Bad`),
    "",
  ];
  await upload(csv(rows.join("\n")));
  await review();
  await listPicker().selectOption({ label: "Newsletter" });
  await run();

  // Seven numbers, six of them different from each other, so a label on the wrong one is seen.
  await expect(count("Imported")).toHaveText("7");
  await expect(count("Updated")).toHaveText("3");
  await expect(count("Skipped")).toHaveText("0");
  await expect(count("Collapsed")).toHaveText("5");
  await expect(count("Failed")).toHaveText("4");
  await expect(count("Suppressed")).toHaveText("2");
  await expect(count("Memberships declined")).toHaveText("1");
  await expect(page.getByText("19 rows in the file")).toBeVisible();

  // The last two describe rows the first five already hold: said on the screen and held in a
  // table of their own, so nobody adds seven numbers and gets 22 rows out of 19.
  const inside = page.getByRole("table", { name: "Counted inside those rows, not extra rows", exact: true });
  await expect(inside.getByRole("rowheader")).toHaveCount(2);
  await expect(inside.getByRole("rowheader", { name: /^Suppressed/ })).toBeVisible();
  await expect(inside.getByRole("rowheader", { name: /^Memberships declined/ })).toBeVisible();
  await expect(page.getByText("already counted above", { exact: false })).toBeVisible();
  const outside = page.getByRole("table", { name: "Rows in the file, by outcome", exact: true });
  await expect(outside.getByRole("rowheader")).toHaveCount(5);

  // Every rejected row fit in the list: no truncation notice.
  await expect(page.getByRole("note", { name: "The rejected rows are not all listed", exact: true })).toHaveCount(0);

  // The report is the server's, not this tab's: a reload of a finished import shows it again.
  await page.reload();
  await expect(page.getByRole("heading", { name: "Import report", exact: true })).toBeVisible();
  await expect(count("Imported")).toHaveText("7");
  await expect(count("Memberships declined")).toHaveText("1");
});

test("the rejected rows page, and the CSV starts with the row the table starts with", async () => {
  await upload(csv(["Email,Name", "fresh-a@example.com,A", "not-an-address,B", "=1+1,C", "also bad,D", ""].join("\n")));
  await review("Email");
  await run();
  await expect(count("Failed")).toHaveText("3");

  const body = rejected().getByRole("row");
  await expect(body).toHaveCount(4); // the header and three rejected rows, in file order
  const first = await body.nth(1).locator("th, td").allTextContents();
  // The line the row is on in the file, the header line counted: what a spreadsheet shows.
  expect(first[0]).toBe("3");
  expect(first).toContain("not-an-address");

  const { name, rows } = await downloadedCsv();
  expect(name).toBe("import-errors.csv");
  expect(rows[0]).toEqual(["row_number", "column_name", "reason", "value"]);
  expect(rows[1]).toEqual(first);
  expect(rows).toHaveLength(4);
  // A cell from someone else's file that opens with = is a formula to a spreadsheet.
  expect(rows[2]?.[3]).toBe("'=1+1");
});

test("a list of rejected rows cut at the server's cap says so, shows the first of them, and pages", async () => {
  test.setTimeout(180_000);
  // 10,003 rows that are not addresses, then one that is. The server keeps 10,000 error rows
  // (MAX_IMPORT_ERRORS) and counts all 10,003 as failed.
  const lines = ["Email", ...Array.from({ length: 10_003 }, (_, n) => `bad-${n + 1}`), "ok@example.com", ""];
  await upload(csv(lines.join("\n")));
  await review("Email");
  await run();

  await expect(count("Failed")).toHaveText("10,003");
  const notice = page.getByRole("note", { name: "The rejected rows are not all listed", exact: true });
  await expect(notice).toContainText("Showing the first 10,000 of 10,003 rejected rows, in file order.");
  await expect(notice).toContainText("The other 3 rejected rows were not kept");

  const body = rejected().getByRole("row");
  await expect(body).toHaveCount(51); // the header and a page of 50
  await page.getByRole("button", { name: "Load more rejected rows", exact: true }).click();
  await expect(body).toHaveCount(101);

  const { rows } = await downloadedCsv();
  expect(rows).toHaveLength(10_001); // the header and the 10,000 that were kept
  expect(rows[1]?.[0]).toBe("2"); // line 1 is the header
  expect(rows[10_000]?.[0]).toBe("10001");
});

test("a choice of skipping reports what it skipped, and a refusal keeps the rows it found", async () => {
  await upload(csv(["Email,Name", "old1@example.com,X", "old2@example.com,X", "fresh-skip@example.com,X", ""].join("\n")));
  await review("Email");
  await dedup(/^Skip it/).check();
  await run();
  await expect(count("Skipped")).toHaveText("2");
  await expect(count("Imported")).toHaveText("1");
  await expect(count("Updated")).toHaveText("0");

  // "Stop" meets an address that is already a contact: the import fails, writes no contact, and
  // the report says why in the server's own words, beside the rejected row it also found.
  await upload(csv(["Email,Name", "old1@example.com,X", "not-an-address,X", ""].join("\n")));
  await review("Email");
  await dedup(/^Stop/).check();
  await run();
  await expect(page.getByRole("heading", { name: "This import failed", exact: true })).toBeVisible();
  await expect(page.getByText("old1@example.com", { exact: false }).first()).toBeVisible();
  await expect(page.getByText("is already a contact", { exact: false })).toBeVisible();
  await expect(rejected().getByRole("row")).toHaveCount(2);
});

test("a separator the sniff got wrong is corrected on the confirmation step", async () => {
  // One comma and one semicolon in every row: the sniff ties and takes the comma.
  const file = ["Email;Name, Note", "sep1@example.com;A, one", "sep2@example.com;B, two", ""].join("\n");
  await upload(csv(file));
  await expect(page.getByText("separated by a comma", { exact: false })).toBeVisible();
  await review("Email;Name");
  await page.getByLabel("Separator", { exact: true }).selectOption({ label: "Semicolon" });
  await expect(will()).toContainText("separated by a semicolon");
  await run();
  await expect(count("Imported")).toHaveText("2");
  await expect(count("Failed")).toHaveText("0");
  // The counts alone do not tell the readings apart: the server takes `sep1@example.com;a` as an
  // address too, so a run that ignored the correction would also report two imports. The contact
  // that exists is what shows which separator the file was read with.
  const found = await admin.request.get(`/api/v1/workspaces/${workspaceId}/contacts?q=sep1%40example.com`);
  expect(found.ok()).toBe(true);
  const emails = ((await found.json()) as { data: { email: string }[] }).data.map((c) => c.email);
  expect(emails).toEqual(["sep1@example.com"]);
});

test("a header the sniff did not see is corrected on the confirmation step", async () => {
  const file = ["ada-h@example.com,Ada", "grace-h@example.com,Grace", "joan-h@example.com,Joan", ""].join("\n");
  await upload(csv(file));
  await expect(page.getByText("no header row", { exact: false })).toBeVisible();
  await review("Column 1");
  const header = page.getByLabel("The first row is a header", { exact: true });
  await expect(header).not.toBeChecked();
  await header.check();
  await expect(will()).toContainText("first row is skipped as a header");
  await run();
  // The first row was read as column names and dropped: three rows in the file, two imported.
  await expect(page.getByText("2 rows in the file")).toBeVisible();
  await expect(count("Imported")).toHaveText("2");
});

test("worst-case confirmation and reports keep the screens inside 320 px and axe-clean in both themes", async () => {
  test.setTimeout(120_000);
  const column = "c".repeat(200);
  const cell = "v".repeat(200);
  await page.setViewportSize({ width: 320, height: 640 });
  try {
    for (const theme of ["light", "dark"] as const) {
      await page.goto("/login");
      await page.evaluate((t) => localStorage.setItem("theme", t), theme);
      const measure = async (where: string) => {
        const widths = await page.evaluate(() => ({
          page: document.documentElement.scrollWidth,
          width: document.documentElement.clientWidth,
        }));
        expect(widths.page, `${where} (${theme}) scrolls sideways`).toBeLessThanOrEqual(widths.width);
        await expectNoViolations(new AxeBuilder({ page }), `${where} (${theme})`);
      };

      // A long column mapped to Time zone: its row is rejected, and the report carries the long
      // column's name and the long cell.
      await upload(csv(`${column},Email\n${cell},ada-w@example.com\n`, `${"f".repeat(200)}.csv`));
      await target("Email").selectOption({ label: "Email" });
      await target(column).selectOption({ label: "Time zone" });
      await page.getByRole("button", { name: "Review mapping", exact: true }).click();
      await listPicker().selectOption({ label: LONG_LIST });
      await expect(will()).toContainText(LONG_LIST);
      await measure("confirmation");
      await run();
      await expect(rejected().getByRole("cell", { name: column, exact: true })).toBeVisible();
      await expect(rejected().getByRole("cell", { name: cell, exact: true })).toBeVisible();
      await measure("report with rejected rows");

      // A refusal whose reason is a 254-character address.
      await upload(csv(`Email\n${LONG_EMAIL}\nnot-an-address\n`));
      await review("Email");
      await dedup(/^Stop/).check();
      await run();
      await expect(page.getByRole("heading", { name: "This import failed", exact: true })).toBeVisible();
      await expect(page.getByText(LONG_EMAIL, { exact: false }).first()).toBeVisible();
      await measure("failed report");
    }
  } finally {
    await page.setViewportSize({ width: 1280, height: 720 });
  }
});

test("another workspace's import is not found, and neither its report nor a 403 is shown", async () => {
  const other = await admin.request.post("/api/v1/workspaces", {
    data: { slug: "importco-other", name: "Other Co", timezone: "UTC" },
    headers: { "X-CSRF-Token": csrf },
  });
  expect(other.ok(), `create workspace -> ${other.status()}`).toBe(true);
  const otherId = ((await other.json()) as { id: string }).id;
  const made = await admin.request.post(`/api/v1/workspaces/${otherId}/imports`, {
    multipart: { file: { name: "other.csv", mimeType: "text/csv", buffer: Buffer.from(PEOPLE) } },
    headers: { "X-CSRF-Token": csrf },
  });
  expect(made.ok(), `upload -> ${made.status()}`).toBe(true);
  const importId = ((await made.json()) as { id: string }).id;

  // Through this workspace's own path, it is not there: 404, not 403.
  const via = await admin.request.get(`/api/v1/workspaces/${workspaceId}/imports/${importId}`);
  expect(via.status()).toBe(404);
  await page.goto(`/w/${SLUG}/imports/${importId}`);
  await expect(page.getByRole("heading", { name: "Import not found", exact: true })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Import report", exact: true })).toHaveCount(0);
  await expect(page.getByText("other.csv")).toHaveCount(0);
});

test("a one-column file reads as one column, not \"1 columns\"", async () => {
  await upload(csv("Email\nsolo-one@example.com\n"));
  await expect(page.getByText("1 column, separated by a comma", { exact: false })).toBeVisible();
  await page.getByRole("button", { name: "Review mapping", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Review the mapping", exact: true })).toBeVisible();
  await expect(will()).toContainText("1 of 1 column is imported");
});

/** The finished import's report, read by the screen, with `change` applied to what the server sent. */
async function withReport(change: (r: components["schemas"]["ImportReport"]) => components["schemas"]["ImportReport"]) {
  await page.route(`**/api/v1/workspaces/*/imports/*`, async (route: Route) => {
    if (route.request().method() !== "GET") return route.fallback();
    const res = await route.fetch();
    return route.fulfill({ response: res, json: change((await res.json()) as components["schemas"]["ImportReport"]) });
  });
}

test("a failed run that kept no rejected rows says so, and offers no download of nothing", async () => {
  await upload(csv(["Email", "zero-a@example.com", "not-an-address", "also bad", ""].join("\n")));
  await review("Email");
  await run();
  await expect(count("Failed")).toHaveText("2");
  await expect(page.getByRole("button", { name: /^Download the/ })).toBeVisible();

  // The savepoint path keeps `failed` and `total_rows` and none of the rows. A stub typed against
  // the generated schema, because that path cannot be reached with a real file.
  await withReport((r) => ({ ...r, errors_recorded: 0 }));
  try {
    await page.reload();
    await expect(page.getByRole("heading", { name: "Import report", exact: true })).toBeVisible();
    await expect(page.getByRole("note", { name: "No rejected rows were kept", exact: true })).toContainText(
      "2 rows were rejected. None were kept.",
    );
    await expect(page.getByRole("button", { name: /^Download the/ })).toHaveCount(0);
    await expect(rejected()).toHaveCount(0);
    await expect(page.getByText("Showing the first", { exact: false })).toHaveCount(0);

    // The mirror: no failed count but kept rows still on file. They are shown, not hidden.
    await page.unroute("**/api/v1/workspaces/*/imports/*");
    await withReport((r) => ({ ...r, failed: 0 }));
    await page.reload();
    await expect(page.getByRole("heading", { name: "Import report", exact: true })).toBeVisible();
    await expect(rejected().getByRole("row")).toHaveCount(3);
    await expect(page.getByText("No row was rejected.")).toHaveCount(0);
  } finally {
    await page.unroute("**/api/v1/workspaces/*/imports/*");
  }
});
