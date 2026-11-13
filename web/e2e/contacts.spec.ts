// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import { type BrowserContext, type Locator, type Page, expect, test } from "@playwright/test";

import { expectNoViolations } from "./support/axe";
import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/**
 * One workspace with 2,250 contacts, seeded once through the API, shared by every test here. That
 * is more than the 2,000 rows `maxPages: 20` lets the browser hold, which is the point: the last
 * test scrolls past the window and back. The tests run in order and share state (the tag one
 * writes), so this file is serial.
 */
test.describe.configure({ mode: "serial" });

const SLUG = "contactsco";
const TOTAL = 2250;
const FIRST = "aaa-first@example.test"; // oldest, and first by email
const LAST = "zzz-last@example.test"; // newest, and last by email
const CONTACTS = `/w/${SLUG}/contacts`;

let context: BrowserContext;
let page: Page;
let workspaceId = "";
let csrf = "";

const ids = { tag: "", single: "", double: "" };

async function api(method: "post" | "get", path: string, data?: unknown) {
  const url = `/api/v1/workspaces/${workspaceId}${path}`;
  const headers = { "X-CSRF-Token": csrf };
  const res =
    method === "get"
      ? await context.request.get(url)
      : await context.request.post(url, { data, headers });
  expect(res.ok(), `${method} ${path} -> ${res.status()}`).toBe(true);
  return (res.status() === 204 ? {} : await res.json()) as {
    id?: string;
    data?: { id: string; email: string }[];
  };
}

async function seed(): Promise<void> {
  const make = (n: string, first: string) =>
    api("post", "/contacts", { email: n, first_name: first, last_name: "Test" });
  await make(FIRST, "Aaa");
  const emails = Array.from(
    { length: TOTAL - 2 },
    (_, n) => `user${String(n + 1).padStart(4, "0")}@example.test`,
  );
  // Sixteen at a time: the server is local, but a thousand simultaneous requests are not the test.
  for (let at = 0; at < emails.length; at += 16) {
    await Promise.all(emails.slice(at, at + 16).map((email) => make(email, "User")));
  }
  await make(LAST, "Zzz");
}

test.beforeAll(async ({ browser }) => {
  test.setTimeout(300_000);
  context = await browser.newContext();
  page = await context.newPage();
  await page.goto("/login");
  await page.getByLabel("Email", { exact: true }).fill(ADMIN_EMAIL);
  await page.getByLabel("Password", { exact: true }).fill(ADMIN_PASSWORD);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(page.getByText(`Signed in as ${ADMIN_EMAIL}`)).toBeVisible();

  await page.goto("/workspaces/new");
  await page.getByLabel("Slug", { exact: true }).fill(SLUG);
  await page.getByLabel("Workspace name", { exact: true }).fill("Contacts Co");
  await page.getByLabel("Time zone", { exact: true }).selectOption("Europe/London");
  await page.getByRole("button", { name: "Create workspace", exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/w/${SLUG}/members$`));

  const me = (await (await context.request.get("/api/v1/me")).json()) as {
    csrf_token: string;
    workspaces: { id: string; slug: string }[];
  };
  csrf = me.csrf_token;
  workspaceId = me.workspaces.find((w) => w.slug === SLUG)?.id ?? "";
  expect(workspaceId).not.toBe("");

  ids.tag = (await api("post", "/tags", { name: "vip" })).id ?? "";
  ids.single = (await api("post", "/lists", { name: "Announcements", double_optin: false })).id ?? "";
  ids.double = (await api("post", "/lists", { name: "Newsletter", double_optin: true })).id ?? "";
  await seed();
});

test.afterAll(async () => {
  await context.close();
});

const scroller = () => page.locator("[data-scroller]");
const rows = () => page.locator("tbody tr[aria-rowindex]");
const grid = () => page.getByRole("grid", { name: "Contacts", exact: true });
const result = () => page.getByTestId("bulk-result");

async function open(search = ""): Promise<void> {
  await page.goto(CONTACTS + search);
  await expect(grid()).toBeVisible();
  await expect(rows().first()).toBeVisible();
}

const indexes = (list: Locator) =>
  list.evaluateAll((trs) => trs.map((tr) => Number(tr.getAttribute("aria-rowindex"))));

test("renders_and_scrolls_past_the_first_page", async () => {
  await open();
  await expect(page.getByRole("heading", { name: "Contacts", exact: true })).toBeVisible();
  await expect(rows().first()).toContainText(LAST);
  // Virtualised: a page is 100 rows and far fewer than that are in the DOM.
  expect(await rows().count()).toBeLessThan(80);

  await expect
    .poll(
      async () => {
        await scroller().evaluate((el) => {
          el.scrollTop = el.scrollHeight;
        });
        return Math.max(...(await indexes(rows())));
      },
      { timeout: 20_000 },
    )
    .toBeGreaterThan(102);
});

test("sorting_by_email_reorders_the_rows", async () => {
  await open();
  await expect(rows().first()).toContainText(LAST);

  const email = grid().getByRole("button", { name: "Email", exact: true });
  await email.click();
  await expect(rows().first()).toContainText(FIRST);
  await expect(grid().getByRole("columnheader", { name: "Email" })).toHaveAttribute(
    "aria-sort",
    "ascending",
  );
  await expect(page).toHaveURL(/sort=email/);

  await email.click();
  await expect(rows().first()).toContainText(LAST);
  await expect(grid().getByRole("columnheader", { name: "Email" })).toHaveAttribute(
    "aria-sort",
    "descending",
  );
  // Only one column is ever sorted.
  await expect(grid().locator("th[aria-sort]")).toHaveCount(1);
});

test("search_filters_the_rows", async () => {
  await open();
  const queries: string[] = [];
  page.on("request", (req) => {
    const q = new URL(req.url(), "http://x").searchParams.get("q");
    if (q !== null) queries.push(q);
  });

  const search = page.getByRole("searchbox", { name: "Search contacts", exact: true });
  await search.fill("aa");
  await expect(page.getByText("Type at least 3 characters to search")).toBeVisible();
  // Past the 250 ms debounce: a 2-character `q` is a 422, so it must never have been sent.
  await page.waitForTimeout(700);
  expect(queries).toEqual([]);

  await search.fill("aaa");
  await expect(rows()).toHaveCount(1);
  await expect(rows().first()).toContainText(FIRST);
  expect(queries).toEqual(["aaa"]);
  await expect(page).toHaveURL(/q=aaa/);

  await search.fill("zzzzzz-nobody");
  await expect(page.getByRole("heading", { name: "No contacts match" })).toBeVisible();
  page.removeAllListeners("request");
});

test("select_all_matching_then_exclude_one_bulk_tags_the_rest", async () => {
  await open();
  const bodies: { selection_token?: string; excluded_ids?: string[]; ids?: string[] }[] = [];
  await page.route("**/contacts/bulk/tag", async (route) => {
    bodies.push(route.request().postDataJSON());
    await route.continue();
  });

  await grid().getByRole("checkbox", { name: "Select all loaded contacts" }).check();
  await expect(page.getByText("100 contacts selected", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Select all contacts matching this view" }).click();
  // The real count, not an estimate, and from now on the grid may say how many rows there are.
  await expect(
    page.getByText(`Here is what you selected: all ${TOTAL.toLocaleString("en-US")} contacts.`),
  ).toBeVisible();
  await expect(grid()).toHaveAttribute("aria-rowcount", String(TOTAL + 1));

  const first = rows().first().getByRole("checkbox");
  const excluded = (await first.getAttribute("aria-label"))?.replace("Select ", "") ?? "";
  await first.uncheck();
  await expect(
    page.getByText(`${(TOTAL - 1).toLocaleString("en-US")} of ${TOTAL.toLocaleString("en-US")} contacts`),
  ).toBeVisible();

  await page.getByLabel("Tag to add or remove", { exact: true }).selectOption({ label: "vip" });
  await page.getByRole("button", { name: "Add tag", exact: true }).click();
  await page
    .getByRole("dialog", { name: "Tag these contacts", exact: true })
    .getByRole("button", { name: `Tag ${(TOTAL - 1).toLocaleString("en-US")} contacts`, exact: true })
    .click();
  await expect(result()).toContainText(`Tagged ${(TOTAL - 1).toLocaleString("en-US")} contacts`);
  await page.unroute("**/contacts/bulk/tag");

  // A predicate and one exclusion went over the wire; 2,249 ids did not.
  expect(bodies).toHaveLength(1);
  expect(typeof bodies[0]?.selection_token).toBe("string");
  expect(bodies[0]?.excluded_ids).toHaveLength(1);
  expect(bodies[0]?.ids).toBeUndefined();

  const tagged = async (q: string) =>
    (await api("get", `/contacts?tag_id=${ids.tag}&q=${encodeURIComponent(q)}`)).data ?? [];
  expect(await tagged(excluded)).toEqual([]);
  expect((await tagged(FIRST)).length + (await tagged(LAST)).length).toBeGreaterThan(0);

  // Re-ordering the grid is not a change of filter: the count somebody paid five seconds for
  // still describes these same 2,250 rows, so a column header must not take it away. The view is
  // too big for its last page to be in, so this figure can only come from `matched` — which is
  // what makes the assertion discriminate at all.
  await page.getByRole("button", { name: "Email", exact: true }).click();
  await expect(page).toHaveURL(/sort=email/);
  await expect(grid()).toHaveAttribute("aria-rowcount", String(TOTAL + 1));
});

test("a_frozen_set_wider_than_a_page_pages_and_says_so_without_inventing_a_figure", async () => {
  test.setTimeout(120_000);
  // The case R2 was written for. Everything below needs a selection that does not fit on one
  // page, which is also the only shape in which the sentence's figure is unknowable.
  const snapshots: (string | null)[] = [];
  const watch = (r: import("@playwright/test").Request) => {
    const url = new URL(r.url());
    if (r.method() === "GET" && url.pathname.endsWith("/contacts")) {
      snapshots.push(url.searchParams.get("snapshot_id"));
    }
  };

  await open();
  await grid().getByRole("checkbox", { name: "Select all loaded contacts" }).check();
  await page.getByRole("button", { name: "Select all contacts matching this view" }).click();
  await expect(
    page.getByText(`Here is what you selected: all ${TOTAL.toLocaleString("en-US")} contacts.`),
  ).toBeVisible();

  // Page two of the frozen set, and every page of it, carries the snapshot: a next page fetched
  // without it would quietly go back to the live filter halfway down the grid.
  page.on("request", watch);
  await expect
    .poll(
      async () => {
        await scroller().evaluate((el) => {
          el.scrollTop = el.scrollHeight;
        });
        return Math.max(...(await indexes(rows())));
      },
      { timeout: 30_000 },
    )
    .toBeGreaterThan(102);
  page.removeListener("request", watch);
  expect(snapshots.length, "no page was fetched, so this proves nothing").toBeGreaterThan(0);
  expect(
    snapshots.every((id) => id !== null),
    `a page of the frozen set was fetched without it: ${snapshots.join(", ")}`,
  ).toBe(true);

  // Now narrow it to more than one page. The view's size is unknown until its last page is in, so
  // there is no "N of M" to give — but the selection's size and the reason are both known, and
  // withholding them is the one answer R2 rules out.
  await page.getByRole("searchbox", { name: "Search contacts", exact: true }).fill("user1");
  await expect(rows().first()).toContainText("user1");
  await expect(
    page.getByText(
      `${TOTAL.toLocaleString("en-US")} contacts are selected. Filtering changes what is shown here, not what is selected.`,
    ),
  ).toBeVisible();
  // And no figure invented to fill the gap.
  await expect(page.getByText("Showing")).toHaveCount(0);
});

test("subscribing_to_a_double_opt_in_list_says_pending_and_never_resurrects", async () => {
  await open("?q=aaa");
  await expect(rows()).toHaveCount(1);
  await grid().getByRole("checkbox", { name: "Select all loaded contacts" }).check();
  await page.getByRole("button", { name: "Select all contacts matching this view" }).click();

  await page.getByLabel("List to subscribe to or unsubscribe from", { exact: true }).selectOption({ label: "Newsletter" });
  await page.getByRole("button", { name: "Subscribe to list", exact: true }).click();
  // `pending` is not mailable and nothing in this milestone confirms it: never "subscribed".
  await expect(result()).toContainText("pending");
  await expect(result()).toContainText("cannot be mailed");
  await expect(result()).not.toContainText(/subscribed 1/i);

  // The same again: nobody is added, and the two numbers say so rather than hiding the gap.
  await page.getByRole("button", { name: "Subscribe to list", exact: true }).click();
  await expect(result()).toContainText("0 of 1");
  await expect(result()).toContainText("already on the list");
});

test("ticked_rows_are_subscribed_by_id_and_an_unsubscribed_one_is_skipped", async () => {
  // Three contacts, one of whom has already unsubscribed from the list: ticking rows sends ids,
  // and the server's bulk rule (never revive an opt-out) applies to them as to a filter.
  const [one, two, three] = ["user1001", "user1002", "user1003"];
  const found = async (q: string) => (await api("get", `/contacts?q=${q}@`)).data?.[0]?.id ?? "";
  const gone = await found(three ?? "");
  await api("post", `/contacts/${gone}/lists`, { list_id: ids.single });
  await api("post", `/lists/${ids.single}/members/${gone}/unsubscribe`); // keeps the row

  await open("?q=user100");
  const bodies: { selection_token?: string; ids?: string[] }[] = [];
  await page.route("**/contacts/bulk/subscribe", async (route) => {
    bodies.push(route.request().postDataJSON());
    await route.continue();
  });
  for (const who of [one, two, three]) {
    await page.getByRole("checkbox", { name: `Select ${who}@example.test` }).check();
  }
  await expect(page.getByText("3 contacts selected", { exact: true })).toBeVisible();
  await page.getByLabel("List to subscribe to or unsubscribe from", { exact: true }).selectOption({ label: "Announcements" });
  await page.getByRole("button", { name: "Subscribe to list", exact: true }).click();
  await expect(result()).toContainText("2 of 3 selected contacts were subscribed");
  await expect(result()).toContainText("already on the list");
  await page.unroute("**/contacts/bulk/subscribe");

  expect(bodies).toHaveLength(1);
  expect(bodies[0]?.ids).toHaveLength(3);
  expect(bodies[0]?.selection_token).toBeUndefined();
});

test("grid_is_one_tab_stop_and_arrow_keys_move_the_row", async () => {
  await open();
  await expect(grid()).toHaveAttribute("role", "grid");
  // Exactly one row is in the tab order, and nothing inside any row is.
  await expect(page.locator('tbody tr[tabindex="0"]')).toHaveCount(1);
  expect(
    await page.locator("tbody tr").evaluateAll((trs) =>
      trs.flatMap((tr) =>
        [...tr.querySelectorAll<HTMLElement>("a, button, input, select, [tabindex]")]
          .filter((el) => el.tabIndex >= 0)
          .map((el) => el.tagName),
      ),
    ),
  ).toEqual([]);

  const active = page.locator('tbody tr[tabindex="0"]');
  const start = Number(await active.getAttribute("aria-rowindex"));
  await active.focus();
  await page.keyboard.press("ArrowDown");
  await expect(page.locator(":focus")).toHaveAttribute("aria-rowindex", String(start + 1));
  await expect(page.locator('tbody tr[tabindex="0"]')).toHaveAttribute(
    "aria-rowindex",
    String(start + 1),
  );
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowUp");
  await expect(page.locator(":focus")).toHaveAttribute("aria-rowindex", String(start + 1));
  await expect(page.locator('tbody tr[tabindex="-1"]').first()).toBeAttached();

  // Space selects the focused row, and the checkbox follows.
  await page.keyboard.press("Space");
  await expect(page.locator(":focus").getByRole("checkbox")).toBeChecked();
  await page.keyboard.press("Escape");
  await expect(page.locator(":focus").getByRole("checkbox")).not.toBeChecked();

  // Tab leaves the grid; it does not walk through 100 rows of checkboxes.
  await page.keyboard.press("Tab");
  expect(await page.evaluate(() => !!document.activeElement?.closest("tbody"))).toBe(false);
  // And coming back lands on the row that was last active.
  await page.keyboard.press("Shift+Tab");
  await expect(page.locator(":focus")).toHaveAttribute("aria-rowindex", String(start + 1));
});

test("aria_rowindex_is_set_on_every_rendered_row", async () => {
  await open();
  // More rows exist than are loaded, so the total is an estimate at best: -1 is "unknown".
  await expect(grid()).toHaveAttribute("aria-rowcount", "-1");
  await expect(grid()).toHaveAttribute("aria-colcount", "4");

  const check = async () => {
    const trs = await page.locator("tbody tr:not([role=presentation])").count();
    const list = await indexes(rows());
    expect(list.length).toBe(trs);
    expect(list.every((n) => Number.isInteger(n) && n >= 2)).toBe(true);
    // Contiguous, 1-based with the header row as 1.
    expect(list).toEqual(list.map((_, n) => (list[0] ?? 0) + n));
    // The spacer rows are not rows.
    expect(await page.locator("tbody tr[role=presentation][aria-rowindex]").count()).toBe(0);
  };
  await check();
  await scroller().evaluate((el) => {
    el.scrollTop = 1500;
  });
  await expect.poll(async () => Math.min(...(await indexes(rows())))).toBeGreaterThan(20);
  await check();
});

test("axe_reports_no_violations", async () => {
  await open();
  await page.getByRole("checkbox", { name: "Select all loaded contacts" }).check();
  await expectNoViolations(new AxeBuilder({ page }), "the contacts grid, every row selected");
});

/**
 * Interaction to Next Paint against the project's 200 ms target, on the heaviest page in the
 * product. INP is the worst interaction's duration (the 98th percentile once there are 50 or
 * more, and this is far fewer), so the largest `event` entry with an `interactionId` is it.
 * The CPU is throttled 4x, the usual stand-in for a mid-range phone, so a pass here is not a pass
 * on a developer's machine only. Chromium rounds durations to 8 ms and drops those under the
 * observer's 16 ms floor; an interaction that fast is not near 200 ms either way.
 */
test("inp_on_the_contacts_route_stays_under_200ms", async () => {
  const INP_TARGET_MS = 200;
  const cdp = await context.newCDPSession(page);
  await open();
  expect(await page.evaluate(() => PerformanceObserver.supportedEntryTypes.includes("event"))).toBe(true);
  await page.evaluate(() => {
    type Timing = PerformanceEventTiming & { interactionId: number };
    const seen: Timing[] = [];
    (window as unknown as { __inp: Timing[] }).__inp = seen;
    new PerformanceObserver((list) => {
      for (const entry of list.getEntries() as Timing[]) {
        if (entry.interactionId > 0) seen.push(entry);
      }
    }).observe({ type: "event", durationThreshold: 16, buffered: true } as PerformanceObserverInit);
  });
  await cdp.send("Emulation.setCPUThrottlingRate", { rate: 4 });
  try {
    // What a person does here: tick a row, tick all, sort, search, move through the grid.
    // Clicked, not checked: the select-all box is indeterminate after a single tick, and Playwright
    // insists a checkbox ends up where it asked.
    const all = grid().getByRole("checkbox", { name: "Select all loaded contacts" });
    await rows().first().getByRole("checkbox").click();
    await rows().first().getByRole("checkbox").click();
    await all.click();
    await all.click();
    await grid().getByRole("button", { name: "Email", exact: true }).click();
    await expect(rows().first()).toContainText(FIRST);
    await page.getByRole("searchbox", { name: "Search contacts", exact: true }).pressSequentially("user", { delay: 50 });
    await expect(rows().first()).toContainText("user");
    await rows().first().focus();
    for (let n = 0; n < 10; n++) await page.keyboard.press("ArrowDown");
    // Let the observer's queue and the last paint flush.
    await page.waitForTimeout(500);
  } finally {
    await cdp.send("Emulation.setCPUThrottlingRate", { rate: 1 });
  }
  const durations = await page.evaluate(() =>
    (window as unknown as { __inp: PerformanceEventTiming[] }).__inp.map((e) => e.duration),
  );
  // An observer that recorded nothing would otherwise read as an INP of 0 and pass.
  expect(durations.length, "the observer saw no interactions").toBeGreaterThan(0);
  const inp = Math.max(...durations);
  test.info().annotations.push({ type: "inp", description: `${inp} ms over ${durations.length} slow events` });
  expect(inp, `INP ${inp} ms against a target of ${INP_TARGET_MS} ms`).toBeLessThan(INP_TARGET_MS);
});

test("the_browser_holds_at_most_2000_rows_however_far_you_scroll", async () => {
  test.setTimeout(180_000);
  await open();

  // Down past 2,000 rows: the first pages must be dropped, not accumulated.
  await expect
    .poll(
      async () => {
        await scroller().evaluate((el) => {
          el.scrollTop = el.scrollHeight;
        });
        return Math.max(...(await indexes(rows())));
      },
      { timeout: 120_000, intervals: [250] },
    )
    .toBeGreaterThan(TOTAL - 2);
  // The rows the table holds, counted from its own height (the spacer rows stand in for the
  // unrendered ones): a window of 2,000 at most, not all 2,250.
  const held = await page.evaluate(() => {
    const row = document.querySelector("tbody tr[aria-rowindex]")?.getBoundingClientRect().height ?? 1;
    const body = document.querySelector("tbody")?.getBoundingClientRect().height ?? 0;
    return Math.round(body / row);
  });
  expect(held).toBeLessThanOrEqual(2000);
  expect(held).toBeGreaterThan(1500);

  // And back up: the evicted pages are fetched again, in order, down to the first row.
  await expect
    .poll(
      async () => {
        await scroller().evaluate((el) => {
          el.scrollTop = 0;
        });
        return Math.min(...(await indexes(rows())));
      },
      { timeout: 120_000, intervals: [250] },
    )
    .toBe(2);
  await expect(rows().first()).toContainText(LAST);
});
