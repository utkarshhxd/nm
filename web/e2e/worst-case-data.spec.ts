// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, type Locator, type Page, expect, test } from "@playwright/test";

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
