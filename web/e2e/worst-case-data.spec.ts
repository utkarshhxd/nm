// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, type Locator, type Page, type Route, expect, test } from "@playwright/test";

import type { components } from "../src/api/schema";
import { axeProblems } from "./support/axe";
import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/**
 * Every screen against data built to break layout, in both themes. Each screen must have no
 * horizontal page scroll, no clipped text and no axe violation.
 *
 * "Clipped" means text a box hides without saying so: an `overflow: hidden` element whose
 * content is wider than it is and that has no ellipsis. A deliberate ellipsis is allowed only
 * when the full value is on the element as a `title`. "Offscreen" means an element that sticks
 * out of the viewport and is not inside a scroller that can reach it.
 *
 * Serial and in order: the first tests need an empty workspace and then exactly one contact, and
 * the rest need the worst-case data on top of that.
 */
test.describe.configure({ mode: "serial" });

const SLUG = "worstcase";
const THEMES = ["light", "dark"] as const;
type Theme = (typeof THEMES)[number];

// The server's limits: names, labels and options 100 characters, an address 254, a note 500.
const LONG_EMAIL = `${"a".repeat(241)}@example.test`;
/** The same worst case on the instance-wide screen, which is a different list entirely. */
const INSTANCE_EMAIL = `${"b".repeat(241)}@example.test`;
const LONG_NAME = "W".repeat(100); // the widest letter, and nowhere to break
const LONG_DOMAIN = `${"d".repeat(63)}.${"e".repeat(63)}.${"f".repeat(63)}.test`;
const LONG_NOTE = "n".repeat(500);
const MARKS = "́̂̃̀̄̈".repeat(5); // combining marks and nothing else
const ARABIC = { first: "محمد بن عبد الله", last: "العلي" };
const SOLO = "solo@example.test";
const FIELD_KEY = "interests";
const OPTIONS = Array.from({ length: 40 }, (_, n) =>
  n === 0 ? "O".repeat(100) : `Interest number ${n}`,
);
/**
 * A segment name at the server's limit. A different letter from `LONG_NAME` on purpose: the list
 * and the tag already carry that one, and a locator that matched all three would measure the
 * wrong screen and report nothing.
 */
const LONG_SEGMENT = "M".repeat(100);
/** A description at the server's limit, as two unbreakable runs with the one line break it allows. */
const LONG_DESCRIPTION = `${"q".repeat(499)}\n${"r".repeat(500)}`;

let admin: BrowserContext;
let page: Page;
let workspaceId = "";
let csrf = "";
const ids = { sink: "", list: "", tag: "", key: "" };

const base = () => `/api/v1/workspaces/${workspaceId}`;

async function send(method: "post" | "put", path: string, data?: unknown): Promise<{ id: string }> {
  const res = await admin.request[method](path, { data, headers: { "X-CSRF-Token": csrf } });
  expect(res.ok(), `${method} ${path} -> ${res.status()} ${await res.text()}`).toBe(true);
  return res.status() === 204 ? { id: "" } : ((await res.json()) as { id: string });
}
const post = (path: string, data?: unknown) => send("post", `${base()}${path}`, data);

/**
 * A fifty-rule document, which cannot be a flat list of fifty.
 *
 * `src/segments/rules.rs` allows `MAX_RULES = 50` but `MAX_RULES_PER_GROUP = 20` and
 * `MAX_DEPTH = 3`, so fifty rules have to nest: a root group holding three subgroups of 17, 17
 * and 16 leaves — three entries at the root, depth 2. Fifty nested two deep is a harder layout
 * than fifty in a row, which is why the count is not reduced to fit a flat shape.
 *
 * The third subgroup is the one that matters: it is where a workspace's own words reach the
 * builder — a 100-character list name and tag name inside a picker, a 100-character select
 * option, and a multi-select whose forty checkboxes include one 100 characters long. Those rules
 * also make the document one the server will not estimate (`filters_on_an_attribute`), so its
 * count is section 5's "too many to count exactly" every run rather than a number that moves
 * with the fixture.
 */
function fiftyRules(): unknown {
  const plain = (n: number) => ({ field: "email", op: "contains", value: `${n}@example.test` });
  const builtIn = [
    { field: "first_name", op: "eq", value: LONG_NAME },
    // The one empty string the server takes as a value, which the builder must not call unfinished.
    { field: "last_name", op: "neq", value: "" },
    { field: "timezone", op: "starts_with", value: "Europe/" },
    { field: "created_at", op: "before", value: "2030-01-01" },
    { field: "updated_at", op: "in_last_days", value: 36500 },
    { field: "suppressed", op: "is_true" },
  ];
  const theirWords = [
    { field: "list", op: "member_of", value: ids.list, status: "subscribed" },
    { field: "list", op: "not_member_of", value: ids.list },
    { field: "tag", op: "has", value: ids.tag },
    { field: "tag", op: "not_has", value: ids.tag },
    { field: "attr:long_text", op: "contains", value: LONG_NAME },
    { field: "attr:long_text", op: "is_not_set" },
    { field: "attr:long_select", op: "eq", value: OPTIONS[0] },
    { field: `attr:${FIELD_KEY}`, op: "has_any", value: OPTIONS },
  ];
  const group = (rules: unknown[]) => ({ group: { match: "all", rules } });
  return {
    match: "any",
    rules: [
      group(Array.from({ length: 17 }, (_, n) => plain(n))),
      group(Array.from({ length: 17 }, (_, n) => builtIn[n % builtIn.length])),
      group([...theirWords, ...Array.from({ length: 8 }, (_, n) => plain(100 + n))]),
    ],
  };
}

/** Loads `path` in `theme` the way a returning visitor has it: a stored choice, then a reload. */
async function visit(path: string, theme: Theme, ready: (p: Page) => Locator): Promise<void> {
  await page.goto(path);
  await page.evaluate((t) => localStorage.setItem("theme", t), theme);
  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
  await expect(ready(page)).toBeVisible();
  await expect(page.locator('[role="status"][aria-busy="true"]')).toHaveCount(0);
}

/** Runs in the page: everything that is wrong with the layout, as sentences. */
function layoutProblems(): string[] {
  const out: string[] = [];
  const root = document.documentElement;
  const width = root.clientWidth;
  if (root.scrollWidth > width) out.push(`the page scrolls sideways: ${root.scrollWidth} > ${width}`);
  if (document.body.scrollWidth > width) out.push(`the body scrolls sideways: ${document.body.scrollWidth} > ${width}`);

  const name = (el: Element) => `<${el.tagName.toLowerCase()}> "${(el.textContent ?? "").trim().slice(0, 30)}"`;
  const hides = (v: string) => v === "hidden" || v === "clip";
  const clippedBy = (el: Element) => {
    for (let up = el.parentElement; up && up !== root; up = up.parentElement) {
      const s = getComputedStyle(up);
      if (s.overflowX !== "visible") return true;
    }
    return false;
  };

  const reported = new Set<Element>();
  const inside = (el: Element) => {
    for (let up = el.parentElement; up; up = up.parentElement) if (reported.has(up)) return true;
    return false;
  };

  for (const el of document.body.querySelectorAll("*")) {
    if (el.closest(".sr-only, [aria-hidden='true']")) continue;
    const s = getComputedStyle(el);
    if (s.display === "none" || s.visibility === "hidden" || s.display === "contents") continue;
    const box = el.getBoundingClientRect();
    if (box.width === 0 && box.height === 0) continue;
    const text = (el.textContent ?? "").trim();

    // A modal is not a scroller. <dialog> carries a UA `overflow: auto`, so content that runs
    // out of one is a horizontal scrollbar *inside* the modal — which every check below reads
    // as "a scroller can reach it" and lets through. A table that must scroll goes in a
    // TableScroller, which is labelled and reachable; a label that must wrap wraps.
    if (el.tagName === "DIALOG" && el.scrollWidth > el.clientWidth + 1) {
      out.push(`the dialog scrolls sideways: ${el.scrollWidth} in ${el.clientWidth}`);
    }

    if ((box.right > width + 1 || box.left < -1) && !clippedBy(el)) {
      // Only the outermost box: its children are off the screen because it is.
      if (inside(el)) continue;
      reported.add(el);
      out.push(`offscreen: ${name(el)} spans ${Math.round(box.left)}..${Math.round(box.right)} of ${width}`);
    }
    // Its own text running out of a box that does not clip it, such as an unbroken address.
    if (!clippedBy(el) && !inside(el)) {
      const range = document.createRange();
      range.selectNodeContents(el);
      const run = range.getBoundingClientRect();
      if (run.right > width + 1 && [...el.childNodes].some((n) => n.nodeType === 3 && n.textContent?.trim())) {
        reported.add(el);
        out.push(`text runs offscreen: ${name(el)} to ${Math.round(run.right)} of ${width}`);
      }
    }
    if (!text) continue;
    const sideways = el.scrollWidth > el.clientWidth + 1 && hides(s.overflowX);
    const down = el.scrollHeight > el.clientHeight + 1 && hides(s.overflowY);
    if (sideways && s.textOverflow === "ellipsis") {
      if (!el.closest("[title]") && !el.querySelector("[title]")) out.push(`truncated with no title: ${name(el)}`);
    } else if (sideways || down) {
      out.push(`clipped: ${name(el)} is ${el.scrollWidth}x${el.scrollHeight} in ${el.clientWidth}x${el.clientHeight}`);
    }
  }
  return out;
}

/** The three checks, on whatever is on screen. */
async function sound(where: string): Promise<string[]> {
  const found = (await page.evaluate(layoutProblems)).map((p) => `${where}: ${p}`);
  found.push(...(await axeProblems(new AxeBuilder({ page }), where)));
  return found;
}

type Screen = {
  name: string;
  path: () => string;
  ready: (p: Page) => Locator;
  /** Run once the page is ready, to put the screen in the state being measured: open a dialog. */
  open?: (p: Page) => Promise<void>;
};
const heading = (name: string | RegExp) => (p: Page) => p.getByRole("heading", { name });
/**
 * The segments screen with its rows really on it, and nothing else.
 *
 * Not `heading("Segments")`. That `<h2>` sits outside the block the query renders into, so it is
 * still there when the query fails and the screen is an `ErrorState` — which carries
 * `role="alert"` and no `aria-busy` (`error-state.tsx`), so it passes both of `visit()`'s gates.
 * `heading("Segments")` would therefore gate **green** on a segments screen with no table on it at
 * all, and the harness would measure an error box and report nothing. The seeded name is in a
 * cell, so it is true only when the rows arrived.
 */
const segmentRow = (p: Page) => p.getByRole("cell", { name: LONG_SEGMENT, exact: true });

/**
 * A dialog is a screen too, and the one place a workspace's own words — a 100-character field
 * label, a 254-character address — reach a 32rem box. Opening it here is what puts it through
 * `layoutProblems` and axe in both themes at all three sizes.
 */
const opens = (button: string, title: string) => async (p: Page) => {
  await p.getByRole("button", { name: button, exact: true }).click();
  await expect(p.getByRole("dialog", { name: title, exact: true })).toBeVisible();
  // A dialog has queries of its own (the custom field definitions): measure the form, not a skeleton.
  await expect(p.locator('[role="status"][aria-busy="true"]')).toHaveCount(0);
};
/**
 * The import wizard's second half, which has no URL of its own for any of its states: the mapping
 * lives in the tab that uploaded the file, and the report belongs to an import that has to be run
 * to exist. So each state is reached by doing what an operator does, in the page, every time —
 * a new import per visit, which is why this is an `open` and not a `path`.
 */
const LONG_COLUMN = "C".repeat(200);
const LONG_CELL = "v".repeat(200);
const importUpload = (text: string) => async (p: Page) => {
  await p.getByLabel("CSV file", { exact: true }).setInputFiles({
    name: `${"f".repeat(200)}.csv`,
    mimeType: "text/csv",
    buffer: Buffer.from(text),
  });
  await p.getByRole("button", { name: "Upload", exact: true }).click();
  await expect(p.getByRole("heading", { name: "Map the columns", exact: true })).toBeVisible();
};
/** The long column goes to Time zone, so its cell is rejected and the report shows both. */
const importReview = (text: string) => async (p: Page) => {
  await importUpload(text)(p);
  await p.getByLabel("Import column Email as", { exact: true }).selectOption({ label: "Email" });
  await p.getByLabel(`Import column ${LONG_COLUMN} as`, { exact: true }).selectOption({ label: "Time zone" });
  await p.getByRole("button", { name: "Review mapping", exact: true }).click();
  await expect(p.getByRole("heading", { name: "Review the mapping", exact: true })).toBeVisible();
  await p.getByLabel("Add every contact to a list", { exact: true }).selectOption({ label: LONG_NAME });
  // The long list name is in the sentence that says what will happen, not only in the picker.
  await expect(p.getByRole("region", { name: "What will happen", exact: true })).toContainText(LONG_NAME);
};
const importRun = (text: string) => async (p: Page) => {
  await importReview(text)(p);
  await p.getByRole("button", { name: "Run the import", exact: true }).click();
  await expect(p.getByRole("heading", { name: "Import report", exact: true })).toBeVisible();
  // The rejected rows are a query of their own: measure the table, not the skeleton before it.
  const rejected = p.getByRole("table", { name: "Rejected rows", exact: true });
  await expect(rejected).toBeVisible();
  // The long values are in cells the screen renders: the column's name and the cut cell.
  await expect(rejected.getByRole("cell", { name: LONG_COLUMN, exact: true }).first()).toBeVisible();
  await expect(rejected.getByRole("cell", { name: LONG_CELL, exact: true }).first()).toBeVisible();
  await expect(p.locator('[role="status"][aria-busy="true"]')).toHaveCount(0);
};
const IMPORT_FILE = `Email,${LONG_COLUMN}\nada@example.test,${LONG_CELL}\n`;
/** The server keeps 10,000 error rows and counts every failed one: 3 more than that is the notice. */
const CAPPED_FILE = `Email,${LONG_COLUMN}\n${Array.from({ length: 10_003 }, (_, n) => `cap${n}@example.test,${LONG_CELL}`).join("\n")}\n`;

const SCREENS: Screen[] = [
  { name: "home", path: () => "/", ready: heading("NM") },
  { name: "new workspace", path: () => "/workspaces/new", ready: heading("Create workspace") },
  { name: "contacts", path: () => `/w/${SLUG}/contacts`, ready: (p) => p.getByRole("grid", { name: "Contacts" }) },
  {
    name: "contact detail",
    path: () => `/w/${SLUG}/contacts/${ids.sink}`,
    ready: heading(LONG_EMAIL),
  },
  { name: "lists", path: () => `/w/${SLUG}/lists`, ready: heading("Lists") },
  { name: "list detail", path: () => `/w/${SLUG}/lists/${ids.list}`, ready: heading(LONG_NAME) },
  { name: "tags", path: () => `/w/${SLUG}/tags`, ready: heading("Tags") },
  { name: "custom fields", path: () => `/w/${SLUG}/custom-fields`, ready: heading("Custom fields") },
  { name: "segments", path: () => `/w/${SLUG}/segments`, ready: segmentRow },
  { name: "suppressions", path: () => `/w/${SLUG}/suppressions`, ready: heading("Suppressions") },
  {
    name: "instance suppressions",
    path: () => "/admin/suppressions",
    ready: heading("Instance suppressions"),
  },
  { name: "members", path: () => `/w/${SLUG}/members`, ready: heading("Members") },
  { name: "api keys", path: () => `/w/${SLUG}/api-keys`, ready: heading("API keys") },
  { name: "audit log", path: () => `/w/${SLUG}/audit-log`, ready: heading("Audit log") },
  { name: "account security", path: () => "/account/security", ready: heading("Security") },
  // The dialogs. Spec §14 asks the break harness over every new component, and until these
  // lines existed it had never seen one: the defect it found first was a 100-character custom
  // field label scrolling the New contact dialog sideways, on desktop.
  {
    name: "new contact dialog",
    path: () => `/w/${SLUG}/contacts`,
    ready: (p) => p.getByRole("grid", { name: "Contacts" }),
    open: opens("New contact", "New contact"),
  },
  {
    name: "edit contact dialog",
    path: () => `/w/${SLUG}/contacts/${ids.sink}`,
    ready: heading(LONG_EMAIL),
    open: opens("Edit contact", "Edit contact"),
  },
  {
    name: "delete contact dialog",
    path: () => `/w/${SLUG}/contacts/${ids.sink}`,
    ready: heading(LONG_EMAIL),
    open: opens("Delete contact", "Delete this contact"),
  },
  {
    name: "erase contact dialog",
    path: () => `/w/${SLUG}/contacts/${ids.sink}`,
    ready: heading(LONG_EMAIL),
    open: opens("Erase contact", "Erase this contact"),
  },
  {
    name: "new list dialog",
    path: () => `/w/${SLUG}/lists`,
    ready: heading("Lists"),
    open: opens("New list", "New list"),
  },
  {
    name: "edit list dialog",
    path: () => `/w/${SLUG}/lists/${ids.list}`,
    ready: heading(LONG_NAME),
    open: opens("Edit list", "Edit list"),
  },
  {
    name: "delete list dialog",
    path: () => `/w/${SLUG}/lists/${ids.list}`,
    ready: heading(LONG_NAME),
    open: opens("Delete list", "Delete this list"),
  },
  // The rule builder, which is the one place in the product where a workspace's own words meet a
  // nested, repeating control inside a drawer. The edit form opens on the fifty-rule document;
  // the new form opens on one empty rule, which is the other extreme of the same box.
  {
    name: "new segment dialog",
    path: () => `/w/${SLUG}/segments`,
    ready: segmentRow,
    open: opens("New segment", "New segment"),
  },
  {
    name: "edit segment dialog",
    path: () => `/w/${SLUG}/segments`,
    ready: segmentRow,
    open: opens(`Edit ${LONG_SEGMENT}`, "Edit segment"),
  },
  {
    name: "delete segment dialog",
    path: () => `/w/${SLUG}/segments`,
    ready: segmentRow,
    open: opens(`Delete ${LONG_SEGMENT}`, "Delete this segment"),
  },
  // The bulk confirmations: the only dialogs whose copy carries a list name, and the only ones
  // that need a selection before they exist at all.
  {
    name: "bulk unsubscribe dialog",
    path: () => `/w/${SLUG}/contacts`,
    ready: (p) => p.getByRole("grid", { name: "Contacts" }),
    open: async (p) => {
      await p.getByRole("checkbox", { name: `Select ${LONG_EMAIL}`, exact: true }).check();
      await p
        .getByLabel("List to subscribe to or unsubscribe from", { exact: true })
        .selectOption({ label: LONG_NAME });
      await opens("Unsubscribe from list", "Unsubscribe from this list")(p);
    },
  },
  {
    name: "bulk tag dialog",
    path: () => `/w/${SLUG}/contacts`,
    ready: (p) => p.getByRole("grid", { name: "Contacts" }),
    open: async (p) => {
      await p.getByRole("checkbox", { name: `Select ${LONG_EMAIL}`, exact: true }).check();
      await p.getByLabel("Tag to add or remove", { exact: true }).selectOption({ label: LONG_NAME });
      await opens("Add tag", "Tag these contacts")(p);
    },
  },
  {
    name: "bulk delete dialog",
    path: () => `/w/${SLUG}/contacts`,
    ready: (p) => p.getByRole("grid", { name: "Contacts" }),
    open: async (p) => {
      await p.getByRole("checkbox", { name: `Select ${LONG_EMAIL}`, exact: true }).check();
      await opens("Delete contacts", "Delete these contacts")(p);
    },
  },
  {
    name: "export contacts dialog",
    path: () => `/w/${SLUG}/contacts`,
    ready: (p) => p.getByRole("grid", { name: "Contacts" }),
    open: opens("Export CSV", "Export these contacts"),
  },
  // The import wizard (M1c section 13): the upload, then every state of the screen it leads to.
  { name: "import upload", path: () => `/w/${SLUG}/imports`, ready: heading("Import contacts") },
  {
    name: "import mapping",
    path: () => `/w/${SLUG}/imports`,
    ready: heading("Import contacts"),
    open: importUpload(IMPORT_FILE),
  },
  {
    name: "import confirmation",
    path: () => `/w/${SLUG}/imports`,
    ready: heading("Import contacts"),
    open: importReview(IMPORT_FILE),
  },
  {
    name: "import report",
    path: () => `/w/${SLUG}/imports`,
    ready: heading("Import contacts"),
    open: importRun(IMPORT_FILE),
  },
  {
    name: "import report, rejected rows cut at the cap",
    path: () => `/w/${SLUG}/imports`,
    ready: heading("Import contacts"),
    open: async (p) => {
      await importRun(CAPPED_FILE)(p);
      await expect(p.getByRole("note", { name: "The rejected rows are not all listed", exact: true })).toBeVisible();
    },
  },
];

async function everyScreen(screens: Screen[], suffix = ""): Promise<string[]> {
  const found: string[] = [];
  for (const theme of THEMES) {
    for (const s of screens) {
      await visit(s.path(), theme, s.ready);
      await s.open?.(page);
      found.push(...(await sound(`${s.name} (${theme}${suffix})`)));
    }
  }
  return found;
}

test.beforeAll(async ({ browser }) => {
  test.setTimeout(120_000);
  admin = await browser.newContext();
  page = await admin.newPage();

  await page.goto("/login");
  await page.getByLabel("Email", { exact: true }).fill(ADMIN_EMAIL);
  await page.getByLabel("Password", { exact: true }).fill(ADMIN_PASSWORD);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(page.getByText(`Signed in as ${ADMIN_EMAIL}`)).toBeVisible();

  await page.goto("/workspaces/new");
  await page.getByLabel("Slug", { exact: true }).fill(SLUG);
  await page.getByLabel("Workspace name", { exact: true }).fill("Worst Case Co");
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
});

test.afterAll(async () => {
  await admin.close();
});

const rows = () => page.locator("tbody tr[aria-rowindex]");
const grid = () => page.getByRole("grid", { name: "Contacts", exact: true });
const box = async (l: Locator) => (await l.boundingBox()) ?? { x: 0, y: 0, width: 0, height: 0 };

test("zero_rows_shows_the_empty_state_not_an_empty_grid", async () => {
  test.setTimeout(120_000);
  const empty = [
    { name: "contacts", path: `/w/${SLUG}/contacts`, title: "No contacts yet" },
    { name: "lists", path: `/w/${SLUG}/lists`, title: "No lists yet" },
    { name: "tags", path: `/w/${SLUG}/tags`, title: "No tags yet" },
    { name: "custom fields", path: `/w/${SLUG}/custom-fields`, title: "No custom fields yet" },
    { name: "suppressions", path: `/w/${SLUG}/suppressions`, title: "No suppressions in this workspace" },
  ];
  const found: string[] = [];
  for (const theme of THEMES) {
    for (const e of empty) {
      await visit(e.path, theme, (p) => p.getByRole("heading", { name: e.title, exact: true }));
      // The sentence, not a header row over nothing.
      await expect(page.getByRole("grid")).toHaveCount(0);
      await expect(page.getByRole("table")).toHaveCount(0);
      found.push(...(await sound(`${e.name} empty (${theme})`)));
    }
  }
  expect(found).toEqual([]);
});

test("one_row_does_not_collapse_the_virtualiser", async () => {
  await post("/contacts", { email: SOLO, first_name: "Only", last_name: "One" });
  const found: string[] = [];
  for (const theme of THEMES) {
    await visit(`/w/${SLUG}/contacts`, theme, () => grid());
    await expect(rows()).toHaveCount(1);
    await expect(rows().first()).toContainText(SOLO);
    // The spacer rows are not rows, and the one real row has its full height inside the scroller.
    const row = await box(rows().first());
    const scroller = await box(page.locator("[data-scroller]"));
    expect(row.height).toBeGreaterThanOrEqual(30);
    expect(row.y + row.height).toBeLessThanOrEqual(scroller.y + scroller.height + 1);
    await expect(grid()).toHaveAttribute("aria-rowcount", "2");
    found.push(...(await sound(`one row (${theme})`)));
  }
  expect(found).toEqual([]);
});

test("seed_the_worst_case_data", async () => {
  test.setTimeout(120_000);
  await post("/custom-fields", {
    key: FIELD_KEY,
    label: "I".repeat(100),
    kind: "multi_select",
    options: OPTIONS,
  });
  // One per control shape the contact form has, each with a label at the server's limit: the
  // `Field` path (an input), and the `Labelled` path (a select).
  await post("/custom-fields", { key: "long_text", label: LONG_NAME, kind: "text" });
  await post("/custom-fields", { key: "long_select", label: LONG_NAME, kind: "select", options: OPTIONS });
  ids.list = (await post("/lists", { name: LONG_NAME, double_optin: false })).id;
  await post("/lists", { name: "Announcements", double_optin: true });
  ids.tag = (await post("/tags", { name: LONG_NAME })).id;

  ids.sink = (
    await post("/contacts", {
      email: LONG_EMAIL,
      first_name: LONG_NAME,
      last_name: LONG_NAME,
      attributes: { [FIELD_KEY]: OPTIONS },
    })
  ).id;
  await post(`/contacts/${ids.sink}/lists`, { list_id: ids.list });
  await send("put", `${base()}/contacts/${ids.sink}/tags/${ids.tag}`);

  await post("/contacts", { email: "marks@example.test", first_name: MARKS, last_name: MARKS });
  await post("/contacts", { email: "rtl@example.test", first_name: ARABIC.first, last_name: ARABIC.last });
  await post("/contacts", { email: "hebrew@example.test", first_name: "שלום", last_name: "עולם" });

  // Suppressions: the longest reason with the longest note on the longest address, a whole
  // domain, one that has been lifted, and an erasure tombstone.
  await post("/suppressions", { scope: "address", email: LONG_EMAIL, reason: "unsubscribed_all", note: LONG_NOTE });
  await post("/suppressions", {
    scope: "domain",
    domain: LONG_DOMAIN,
    reason: "unsubscribed_all",
    note: "first line\nsecond line",
  });
  const lifted = await post("/suppressions", { scope: "address", email: "lifted@example.test", reason: "complained" });
  await post(`/suppressions/${lifted.id}/lift`);
  const erased = await post("/contacts", { email: "erased@example.test" });
  await post(`/contacts/${erased.id}/erase`);

  // Instance-wide, which no workspace screen lists: the same longest address and note again.
  await send("post", "/api/v1/admin/suppressions", {
    scope: "address",
    email: INSTANCE_EMAIL,
    reason: "unsubscribed_all",
    note: LONG_NOTE,
  });
  await send("post", "/api/v1/admin/suppressions", {
    scope: "domain",
    domain: LONG_DOMAIN,
    reason: "manual",
    note: "first line\nsecond line",
  });

  // Members and keys with the longest names, so the older M0 screens have something to break on.
  const invite = (await post("/invites", { email: `${"m".repeat(241)}@example.test`, role: "editor" })) as unknown as {
    invite_url: string;
  };
  const guest = await (await page.context().browser()!.newContext()).newPage();
  try {
    await guest.goto(new URL(invite.invite_url).pathname);
    await guest.getByLabel("Your name", { exact: true }).fill(LONG_NAME);
    await guest.getByLabel("Password", { exact: true }).fill("worst-case-password");
    await guest.getByRole("button", { name: "Accept", exact: true }).click();
    await expect(guest.getByText("Signed in as")).toBeVisible();
  } finally {
    await guest.context().close();
  }
  ids.key = (await post("/api-keys", { name: LONG_NAME, role: "viewer" })).id;

  // Segments last: the rule document names the list, the tag and the custom fields above.
  await post("/segments", {
    name: LONG_SEGMENT,
    description: LONG_DESCRIPTION,
    rules: fiftyRules(),
  });
  // One ordinary segment beside it, because an exact count is the only state that offers
  // "Select contacts" (section 14) and so the only one whose actions cell holds three buttons.
  // The fifty-rule document above is `no_estimate` and never will be.
  await post("/segments", {
    name: "Everyone at the long domain",
    description: "",
    rules: {
      match: "all",
      rules: [{ field: "email_domain", op: "contains", value: "example.test" }],
    },
  });
});

test("a_254_character_email_does_not_overflow_the_row", async () => {
  const found: string[] = [];
  for (const theme of THEMES) {
    await visit(`/w/${SLUG}/contacts`, theme, () => grid());
    const row = rows().filter({ hasText: LONG_EMAIL });
    await expect(row).toHaveCount(1);
    const scroller = await box(page.locator("[data-scroller]"));
    const own = await box(row);
    const other = await box(rows().filter({ hasText: SOLO }));
    // Inside its scroller, and no taller than a row that has an ordinary address.
    expect(own.x + own.width).toBeLessThanOrEqual(scroller.x + scroller.width + 1);
    expect(own.height).toBe(other.height);
    found.push(...(await sound(`contacts, 254-character email (${theme})`)));

    await visit(`/w/${SLUG}/contacts/${ids.sink}`, theme, heading(LONG_EMAIL));
    found.push(...(await sound(`contact detail, 254-character email (${theme})`)));
  }
  expect(found).toEqual([]);
});

test("a_name_of_only_combining_marks_renders", async () => {
  const found: string[] = [];
  for (const theme of THEMES) {
    await visit(`/w/${SLUG}/contacts`, theme, () => grid());
    const row = rows().filter({ hasText: "marks@example.test" });
    await expect(row).toHaveCount(1);
    // The marks are in the cell as text, and they did not make the row any taller.
    await expect(row.locator("td").nth(1)).toHaveText(`${MARKS} ${MARKS}`.trim());
    expect((await box(row)).height).toBe((await box(rows().filter({ hasText: SOLO }))).height);
    found.push(...(await sound(`combining marks (${theme})`)));
  }
  expect(found).toEqual([]);
});

test("rtl_name_does_not_break_column_alignment", async () => {
  const found: string[] = [];
  for (const theme of THEMES) {
    await visit(`/w/${SLUG}/contacts`, theme, () => grid());
    const rtl = rows().filter({ hasText: "rtl@example.test" });
    await expect(rtl).toHaveCount(1);
    await expect(rows().filter({ hasText: "hebrew@example.test" })).toHaveCount(1);
    // Every row's cells start at the same x and have the same width as the header's.
    const columns = async (r: Locator) => r.locator("td").evaluateAll((tds) =>
      tds.map((td) => `${Math.round(td.getBoundingClientRect().left)}+${Math.round(td.getBoundingClientRect().width)}`),
    );
    const reference = await columns(rows().filter({ hasText: SOLO }));
    expect(await columns(rtl)).toEqual(reference);
    expect(await columns(rows().filter({ hasText: "hebrew@example.test" }))).toEqual(reference);
    found.push(...(await sound(`right-to-left names (${theme})`)));
  }
  expect(found).toEqual([]);
});

test("a_multi_select_field_with_40_values_wraps", async () => {
  const found: string[] = [];
  for (const theme of THEMES) {
    await visit(`/w/${SLUG}/contacts/${ids.sink}`, theme, heading(LONG_EMAIL));
    const items = page.locator("ul", { has: page.locator("li >> text=Interest number 39") }).locator("li");
    await expect(items).toHaveCount(40);
    const tops = await items.evaluateAll((lis) => new Set(lis.map((li) => Math.round(li.getBoundingClientRect().top))).size);
    expect(tops, "40 badges on more than one line").toBeGreaterThan(1);
    found.push(...(await sound(`multi-select, 40 values (${theme})`)));

    await visit(`/w/${SLUG}/custom-fields`, theme, heading("Custom fields"));
    found.push(...(await sound(`custom fields, 40 options (${theme})`)));
  }
  expect(found).toEqual([]);
});

test("a_list_name_at_the_length_limit_does_not_push_the_count_offscreen", async () => {
  const found: string[] = [];
  for (const theme of THEMES) {
    await visit(`/w/${SLUG}/lists/${ids.list}`, theme, heading(LONG_NAME));
    const count = page.getByTestId("mailable-count");
    await expect(count).toBeVisible();
    const at = await box(count);
    const width = page.viewportSize()?.width ?? 0;
    expect(at.x).toBeGreaterThanOrEqual(0);
    expect(at.x + at.width).toBeLessThanOrEqual(width);
    found.push(...(await sound(`list detail (${theme})`)));

    await visit(`/w/${SLUG}/lists`, theme, heading("Lists"));
    found.push(...(await sound(`lists (${theme})`)));
    // The same name is an option in the contacts toolbar's two list selects and the tag one.
    await visit(`/w/${SLUG}/contacts`, theme, () => grid());
    found.push(...(await sound(`contacts toolbar with a ${LONG_NAME.length}-character list (${theme})`)));
  }
  expect(found).toEqual([]);
});

test("the_longest_suppression_reason_fits_its_badge", async () => {
  const found: string[] = [];
  for (const theme of THEMES) {
    await visit(`/w/${SLUG}/suppressions`, theme, heading("Suppressions"));
    await page.getByRole("checkbox", { name: /lifted/i }).check();
    await expect(page.getByText(/Lifted [A-Z][a-z]{2} \d/)).toBeVisible();
    await expect(page.getByRole("cell", { name: "Unsubscribed from everything" }).first()).toBeVisible();
    // Every badge shows all of its words.
    const badges = await page.locator("td span.rounded-full").evaluateAll((all) =>
      all.map((b) => b.scrollWidth <= b.clientWidth + 1),
    );
    expect(badges.length).toBeGreaterThan(0);
    expect(badges.every(Boolean)).toBe(true);
    found.push(...(await sound(`suppressions (${theme})`)));

    await visit(`/w/${SLUG}/contacts/${ids.sink}`, theme, heading(LONG_EMAIL));
    await expect(page.getByText("Not mailable")).toBeVisible();
    await expect(page.getByText("Unsubscribed from everything")).toBeVisible();
    found.push(...(await sound(`contact detail, suppressed (${theme})`)));
  }
  expect(found).toEqual([]);
});

test("a_50_rule_segment_does_not_burst_the_rule_builder", async () => {
  test.setTimeout(180_000);
  // Pinned, because the scroller assertion below is about desktop: nothing else in this file sets
  // the viewport back on its way out except the three sweeps, and a future test that leaves it
  // narrow would red that assertion for a reason that has nothing to do with its own change.
  await page.setViewportSize({ width: 1280, height: 720 });
  const found: string[] = [];
  for (const theme of THEMES) {
    await visit(`/w/${SLUG}/segments`, theme, segmentRow);
    // A 100-character name and a 1000-character description wrap rather than widening the table:
    // at this width its scroller has nothing to scroll, so nobody has to scroll to read a name.
    const scroller = page.getByRole("region", { name: "Segments table", exact: true });
    const over = await scroller.evaluate((el) => el.scrollWidth - el.clientWidth);
    expect(over, "the segments table fits its scroller on desktop").toBeLessThanOrEqual(1);
    found.push(...(await sound(`segments list, ${LONG_SEGMENT.length}-character name (${theme})`)));

    await opens(`Edit ${LONG_SEGMENT}`, "Edit segment")(page);
    // Every one of the fifty rules is really drawn, and drawn open. `layoutProblems` skips
    // `display: none` and zero-size boxes, so a builder that collapsed its groups would measure
    // almost nothing and report nothing — a closed accordion fits on a phone.
    const rules = page.getByRole("group", { name: /^Rule \d+\.\d+$/ });
    await expect(rules).toHaveCount(50);
    await expect(page.getByRole("group", { name: /^Group \d+$/ })).toHaveCount(3);
    const drawn = await rules.evaluateAll(
      (all) => all.filter((el) => el.getBoundingClientRect().height > 0).length,
    );
    expect(drawn, "all fifty rules are open, not a collapsed accordion").toBe(50);
    // The count area is a sentence rather than a skeleton, so what follows measures the form and
    // not a placeholder. It is this sentence every run, and not a number that moves with the
    // fixture, because the `attr:` rules above make the document one the server refuses to
    // estimate (`filters_on_an_attribute`) before it touches the database at all.
    await expect(page.getByTestId("preview-count")).toHaveText("too many to count exactly");
    found.push(...(await sound(`edit segment, 50 rules two deep (${theme})`)));
  }
  expect(found).toEqual([]);
});

/**
 * The one page-level count alert, which is the only M1b state on this screen a healthy workspace
 * never shows — and both of its sentences, because they are two different facts: the batch that
 * failed outright, and the batch that arrived with rows in it that could not be counted.
 *
 * It needs no `SCREENS` entry, which is what round 0 got wrong: `visit()` gating `ready` before
 * `open` is a fact about `SCREENS`, and nine tests in this file reach `sound()` without it. The
 * list and the counts are two separate queries, so with the counts intercepted the rows still
 * render and `segmentRow` is still a true `ready`; `unroute` in a `finally` is the whole of the
 * leak problem.
 */
test("the_count_alert_says_which_failure_it_is_without_widening_the_page", async () => {
  const COUNTS = "**/segments/counts*";
  const failures = [
    {
      what: "the whole batch failed",
      // A real problem document, so the client reads it the way it reads the server's.
      route: (r: Route) =>
        r.fulfill({
          status: 503,
          contentType: "application/problem+json",
          body: JSON.stringify({
            type: "urn:nm:problem:internal",
            title: "Service Unavailable",
            status: 503,
          }),
        }),
      says: "The counts could not be loaded.",
    },
    {
      what: "some rows in it could not be counted",
      // Every id the page asked for, echoed back as `failed`, so this needs no segment id of its
      // own. `failed` is the state that means "not this time", and the rows answer it with "—".
      //
      // Typed against the generated schema, not hand-written: a stub is a promise about what the
      // server sends, and an untyped one goes on passing against a shape the server has stopped
      // sending. With this annotation that change fails `pnpm -C web typecheck` instead.
      route: (r: Route) => {
        const asked = new URL(r.request().url()).searchParams.get("ids") ?? "";
        const rows: components["schemas"]["SegmentCount"][] = asked
          .split(",")
          .map((id) => ({ id, state: "failed", count: null }));
        return r.fulfill({
          status: 200,
          contentType: "application/json",
          body: JSON.stringify(rows),
        });
      },
      says: "Some of these counts could not be worked out this time.",
    },
  ];
  const found: string[] = [];
  try {
    // The alert is a sentence with a ghost button inline in it, so the narrow case is the one that
    // can break — and the three widths every `SCREENS` screen gets, so a reader comparing the two
    // does not have to work out whether the difference was deliberate. 640 is 200 % zoom.
    for (const width of [320, 640, 1280]) {
      await page.setViewportSize({ width, height: 640 });
      for (const failure of failures) {
        await page.route(COUNTS, failure.route);
        try {
          for (const theme of THEMES) {
            await visit(`/w/${SLUG}/segments`, theme, segmentRow);
            await expect(page.getByRole("alert").filter({ hasText: failure.says })).toBeVisible();
            await expect(
              page.getByRole("button", { name: "Count the segments again", exact: true }),
            ).toBeVisible();
            // One failure is one event: said once for the page, never once per row. Twenty-five
            // identically named Retry buttons came off this screen already (Task 10 round 3).
            await expect(page.getByRole("alert")).toHaveCount(1);
            found.push(...(await sound(`count alert, ${failure.what} (${theme}, ${width}px)`)));
          }
        } finally {
          await page.unroute(COUNTS);
        }
      }
    }
  } finally {
    await page.setViewportSize({ width: 1280, height: 720 });
  }
  expect(found).toEqual([]);
});

test("the_older_m0_screens_hold_up_under_the_new_text_scale", async () => {
  test.setTimeout(180_000);
  const m0 = SCREENS.filter((s) => ["members", "api keys", "audit log", "account security", "home"].includes(s.name));
  const found = await everyScreen(m0);
  const anon = await (await page.context().browser()!.newContext()).newPage();
  try {
    await anon.goto("/login");
    for (const theme of THEMES) {
      await anon.evaluate((t) => localStorage.setItem("theme", t), theme);
      await anon.reload();
      await expect(anon.getByRole("heading", { name: "Sign in" })).toBeVisible();
      expect(await anon.evaluate(layoutProblems)).toEqual([]);
      found.push(...(await axeProblems(new AxeBuilder({ page: anon }), `login (${theme})`)));
    }
  } finally {
    await anon.context().close();
  }
  expect(found).toEqual([]);
});

test("every_screen_in_both_themes_on_desktop", async () => {
  test.setTimeout(300_000);
  expect(await everyScreen(SCREENS)).toEqual([]);
});

test("the_contacts_grid_holds_up_at_320px_wide", async () => {
  test.setTimeout(300_000);
  await page.setViewportSize({ width: 320, height: 640 });
  try {
    expect(await everyScreen(SCREENS, ", 320px")).toEqual([]);
  } finally {
    await page.setViewportSize({ width: 1280, height: 720 });
  }
});

test("the_contacts_grid_holds_up_at_200_percent_zoom", async () => {
  test.setTimeout(300_000);
  // 200% browser zoom on a 1280x720 window is a 640x360 CSS-pixel viewport.
  await page.setViewportSize({ width: 640, height: 360 });
  try {
    expect(await everyScreen(SCREENS, ", 200% zoom")).toEqual([]);
  } finally {
    await page.setViewportSize({ width: 1280, height: 720 });
  }
});
