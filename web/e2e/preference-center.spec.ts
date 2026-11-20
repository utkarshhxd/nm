// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import {
  type APIRequestContext,
  type Browser,
  type BrowserContext,
  type Page,
  expect,
  test,
} from "@playwright/test";

import { axeProblems } from "./support/axe";
import { confirmPath, unsubscribePath } from "./support/public-link";
import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/**
 * The three public pages — the signup form, the confirmation landing, the preference centre — as
 * a person meets them when the usual assumptions fail.
 *
 * Every other spec on this branch drives these routes as a protocol. This one drives them as a
 * **surface**: at 200 % zoom, at the 320 CSS pixels WCAG 2.2 AA's reflow criterion names, in both
 * colour schemes, by keyboard alone, and with JavaScript switched off. Nobody on the far side of
 * these failures can file a bug: they do not unsubscribe, and the product breaks a law on their
 * behalf.
 *
 * **A clean axe run is the floor.** It means no automatically detectable violation; it does not
 * mean usable. The four things it cannot see are checked here by driving the page — the focus ring
 * measured where it actually lands, the tab order read back, the saved page's announcement, and
 * reflow at the narrow widths.
 */

/**
 * This file's own per-IP rate-limit budget, from RFC 5737's documentation range. No other spec
 * uses this address; one that reused it would inherit this file's spending.
 *
 * **Eight** of `PUBLIC_PER_MINUTE`'s 60, and the last test asserts the browser sent exactly that:
 *
 * 1. `GET /f/{ws}/{slug}` — the signup page, swept.
 * 2. `POST /f/{ws}/{slug}` — the signup, whose answer ("Check your email") is swept too.
 * 3. `GET /c/{token}` — the confirmation landing, swept.
 * 4. `GET /u/{token}` — the preference centre, swept, walked by keyboard.
 * 5. `GET /u/{token}` — the preference centre again, to change something on it.
 * 6. `POST /u/{token}` — the change.
 * 7. `GET /u/{token}` — with JavaScript off.
 * 8. `POST /u/{token}` — with JavaScript off.
 *
 * Every sweep loops over two colour schemes and three viewport widths **without reloading**:
 * `prefers-color-scheme` and the viewport are live, so a sweep of one page costs one request
 * rather than six. That is the difference between this file fitting in the budget and not.
 */
const ADDRESS = "198.51.100.12";
const FORWARDED = "x-forwarded-for";
const SPENT = 8;

test.use({ extraHTTPHeaders: { [FORWARDED]: ADDRESS } });
test.describe.configure({ mode: "serial" });

const SLUG = "access";
const SUBSCRIBER = "reader@example.test";
/** The widest letter and nowhere to break it: an operator-typed name is what reflows these pages. */
const LONG_LIST = "W".repeat(100);

/**
 * 1280 is the suite's ordinary window. 640 is 200 % zoom in it. 320 is what WCAG 2.2 AA 1.4.10
 * actually asks for, and what 200 % zoom on a phone-sized window reaches — the width an email
 * client's in-app browser hands these pages.
 */
const SIZES = [
  { name: "100% zoom", width: 1280, height: 720 },
  { name: "200% zoom", width: 640, height: 360 },
  { name: "320px", width: 320, height: 640 },
];
const THEMES = ["light", "dark"] as const;

let browser: Browser;
let api: APIRequestContext;
let context: BrowserContext;
let page: Page;
let csrf = "";
const ids = { ws: "", digest: "", long: "", contact: "" };
/** Every request to a public route this file's browsers made, for the budget assertion. */
const spent: string[] = [];

async function post(path: string, data: unknown): Promise<{ id: string }> {
  const res = await api.post(path, { data, headers: { "X-CSRF-Token": csrf } });
  expect(res.ok(), `post ${path} -> ${res.status()} ${await res.text()}`).toBe(true);
  return (await res.json()) as { id: string };
}

/** The statuses this contact's memberships hold, read back through the authenticated API. */
async function memberships(): Promise<[string, string][]> {
  const got = (await (
    await api.get(`/api/v1/workspaces/${ids.ws}/contacts/${ids.contact}/lists`)
  ).json()) as { data: { list_id: string; status: string }[] };
  return got.data.map((m) => [m.list_id === ids.digest ? "digest" : "long", m.status]);
}

/** The consent log for this contact, newest first. An authenticated read: no public budget. */
async function consent(): Promise<{ action: string; source: string; ip: string | null }[]> {
  const got = (await (
    await api.get(`/api/v1/workspaces/${ids.ws}/contacts/${ids.contact}/consent`)
  ).json()) as { data: { action: string; source: string; ip: string | null }[] };
  return got.data;
}

/** Counts what a context's pages send to `/c/`, `/u/` and `/f/`. */
function meter(ctx: BrowserContext): void {
  ctx.on("request", (request) => {
    const path = new URL(request.url()).pathname;
    if (/^\/[cuf]\//.test(path)) spent.push(`${request.method()} ${path.slice(0, 12)}…`);
  });
}

/* ------------------------------------------------------------------ in-page measurements --- */

/**
 * Reflow, as WCAG 2.2 AA 1.4.10 means it: no second scrollbar, and nothing pushed out of the
 * viewport. `web/e2e/worst-case-data.spec.ts` has a much larger version of this for the admin
 * grid; these pages are a `<main>` with a form in it, and the two things that go wrong are an
 * operator-typed string with nowhere to break and a control wider than the column.
 *
 * `.hp` is skipped: the signup form's honeypot is positioned at -10000px **on purpose**, so a
 * script finds it and a person never does (`src/public/forms.rs`). It is the one box on these
 * pages that is off-screen by design.
 */
function reflowProblems(): string[] {
  const out: string[] = [];
  const root = document.documentElement;
  const width = root.clientWidth;
  if (root.scrollWidth > width) {
    out.push(`the page scrolls sideways: ${root.scrollWidth} > ${width}`);
  }
  for (const el of document.body.querySelectorAll("*")) {
    if (el.closest(".hp")) continue;
    const style = getComputedStyle(el);
    if (style.display === "none" || style.visibility === "hidden") continue;
    const box = el.getBoundingClientRect();
    if (box.width === 0 && box.height === 0) continue;
    if (box.right > width + 1 || box.left < -1) {
      const text = (el.textContent ?? "").trim().slice(0, 20);
      out.push(
        `offscreen: <${el.tagName.toLowerCase()}> "${text}" spans ` +
          `${Math.round(box.left)}..${Math.round(box.right)} of ${width}`,
      );
    }
  }
  return out;
}

/**
 * What is focused, and whether its focus ring can be seen where it is drawn.
 *
 * axe has no rule for this: it reads the DOM, and a ring is a painted outline whose contrast is
 * against whatever is behind it — here the `<main>` surface, because `outline-offset` draws it
 * outside the element. The colours are read through a canvas rather than parsed, because
 * `page::STYLE` writes them in `oklch()` and the canvas is the browser's own converter; an
 * assignment the canvas cannot parse leaves the sentinel behind and is reported as unmeasurable
 * rather than silently scored.
 */
function focusRing(): { what: string; ratio: number; width: number } {
  const el = document.activeElement as HTMLElement | null;
  if (!el || el === document.body) return { what: "(nothing focused)", ratio: 0, width: 0 };

  const named = () => {
    const wrapping = el.closest("label");
    if (wrapping) return wrapping.textContent ?? "";
    const id = el.getAttribute("id");
    const labelled = id ? document.querySelector(`label[for="${CSS.escape(id)}"]`) : null;
    return labelled?.textContent ?? el.textContent ?? el.tagName;
  };
  // 120, so a list name at `lists::validate_name`'s 100-character ceiling arrives whole: the tab
  // order is asserted against these strings and a truncated one would never match.
  const what = named().trim().slice(0, 120);

  const canvas = document.createElement("canvas");
  canvas.width = 1;
  canvas.height = 1;
  const ctx = canvas.getContext("2d");
  if (!ctx) return { what, ratio: 0, width: 0 };
  const rgb = (css: string): [number, number, number] | null => {
    ctx.fillStyle = "#010203";
    ctx.fillRect(0, 0, 1, 1);
    ctx.fillStyle = css;
    ctx.fillRect(0, 0, 1, 1);
    const pixel = ctx.getImageData(0, 0, 1, 1).data;
    const r = pixel[0] ?? 1;
    const g = pixel[1] ?? 2;
    const b = pixel[2] ?? 3;
    // Unparseable, or fully transparent: either way the sentinel is still there.
    return r === 1 && g === 2 && b === 3 ? null : [r, g, b];
  };
  const luminance = ([r, g, b]: [number, number, number]) => {
    const linear = (v: number) => {
      const s = v / 255;
      return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
    };
    return 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
  };

  const style = getComputedStyle(el);
  const width = Number.parseFloat(style.outlineWidth) || 0;
  if (style.outlineStyle === "none" || width === 0) return { what, ratio: 0, width: 0 };

  let behind: [number, number, number] | null = null;
  for (let up: HTMLElement | null = el.parentElement; up && !behind; up = up.parentElement) {
    behind = rgb(getComputedStyle(up).backgroundColor);
  }
  const ring = rgb(style.outlineColor);
  if (!ring || !behind) return { what, ratio: 0, width };
  const lighter = Math.max(luminance(ring), luminance(behind)) + 0.05;
  const darker = Math.min(luminance(ring), luminance(behind)) + 0.05;
  return { what, ratio: Math.round((lighter / darker) * 100) / 100, width };
}

/* ------------------------------------------------------------------------------ the sweep --- */

/**
 * axe and reflow on whatever is on screen, in both colour schemes at all three widths, with no
 * reload. The media query in `page::STYLE` is the only theme mechanism these pages have — there is
 * no script and no `data-theme` — so `emulateMedia` is the whole of it.
 *
 * The two themes really are two pages: asserted, because a sweep that silently measured the same
 * rendering twice would report a clean dark theme it never saw.
 */
async function sweep(p: Page, where: string): Promise<string[]> {
  const found: string[] = [];
  const backgrounds = new Set<string>();
  try {
    for (const theme of THEMES) {
      await p.emulateMedia({ colorScheme: theme });
      backgrounds.add(await p.evaluate(() => getComputedStyle(document.body).backgroundColor));
      for (const size of SIZES) {
        await p.setViewportSize({ width: size.width, height: size.height });
        const at = `${where} (${theme}, ${size.name})`;
        found.push(...(await p.evaluate(reflowProblems)).map((problem) => `${at}: ${problem}`));
        found.push(...(await axeProblems(new AxeBuilder({ page: p }), at)));
      }
    }
  } finally {
    await p.setViewportSize({ width: 1280, height: 720 });
    await p.emulateMedia({ colorScheme: "light" });
  }
  expect(backgrounds.size, `${where}: the two colour schemes rendered identically`).toBe(2);
  return found;
}

/** One stop on a tab walk: what the press landed on, and the ring it drew there. */
type Stop = { what: string; ratio: number; width: number };

/**
 * Tab until `steps` controls have been reached, reporting each and the ring it drew there.
 *
 * Tab continues from wherever focus was last left, so a second walk over the same page starts in
 * the middle and passes through one press that lands on nothing — the browser, not the page,
 * because past the last control focus leaves the document. That press is dropped and the walk
 * keeps going; it is not a stop a person can see a ring on. Blurring first does **not** reset the
 * sequential focus navigation starting point in Chromium, which is what this replaced.
 *
 * The length is asserted, so a walk that silently reached nothing cannot hand `dimRings` an empty
 * list to find no dim rings in.
 */
async function walk(p: Page, steps: number): Promise<Stop[]> {
  const seen: Stop[] = [];
  for (let press = 0; press < steps * 2 && seen.length < steps; press++) {
    await p.keyboard.press("Tab");
    const stop = await p.evaluate(focusRing);
    if (stop.what !== "(nothing focused)") seen.push(stop);
  }
  expect(seen.map((stop) => stop.what), "the tab walk reached every control").toHaveLength(steps);
  return seen;
}

/**
 * Every ring in `seen` is at least 2px wide and at least 3:1 against what is behind it — WCAG 2.2
 * AA's 1.4.11 and 2.4.13 floor for a focus indicator.
 */
function dimRings(seen: Stop[], where: string): string[] {
  return seen
    .filter((stop) => stop.width < 2 || stop.ratio < 3)
    .map((stop) => `${where}: the ring on "${stop.what}" is ${stop.width}px at ${stop.ratio}:1`);
}

/* ----------------------------------------------------------------------------------- setup --- */

test.beforeAll(async ({ playwright, browser: launched }) => {
  browser = launched;
  api = await playwright.request.newContext({ baseURL: "http://localhost:8090" });
  expect(
    (await api.post("/api/v1/auth/login", { data: { email: ADMIN_EMAIL, password: ADMIN_PASSWORD } }))
      .ok(),
  ).toBe(true);
  csrf = ((await (await api.get("/api/v1/me")).json()) as { csrf_token: string }).csrf_token;

  ids.ws = (
    await post("/api/v1/workspaces", { slug: SLUG, name: "Access Co", timezone: "Europe/London" })
  ).id;
  const base = `/api/v1/workspaces/${ids.ws}`;
  // Double opt-in, so submitting the form leaves a pending membership for the confirmation link
  // to act on, which is what Step 1 of this task is about.
  ids.digest = (
    await post(`${base}/lists`, { name: "Weekly digest", double_optin: true, visibility: "public" })
  ).id;
  // Public and not on the form: it is on the preference centre because invariant 10 puts every
  // public list there, and its name is the reflow case.
  ids.long = (
    await post(`${base}/lists`, { name: LONG_LIST, double_optin: false, visibility: "public" })
  ).id;
  await post(`${base}/forms`, {
    slug: "join",
    name: "Join",
    list_ids: [ids.digest],
    fields: [
      { target: { kind: "email" }, required: true, label: "Email address" },
      { target: { kind: "first_name" }, required: false, label: "First name" },
    ],
    headline: "Join the list",
    button_label: "Join",
  });

  context = await browser.newContext();
  meter(context);
  page = await context.newPage();
});

test.afterAll(async () => {
  await context.close();
  await api.dispose();
});

/* ------------------------------------------------------------------------------- the pages --- */

test("the_signup_form_is_sound_and_its_decoy_is_not_in_the_tab_order", async () => {
  const found = await page.goto(`/f/${SLUG}/join`);
  expect(found?.status()).toBe(200);
  await expect(page.getByRole("heading", { name: "Join the list", exact: true })).toBeVisible();
  const problems = await sweep(page, "signup form");

  // The decoy is in the page for a script to fill in, and a keyboard never reaches it. Both
  // halves: a `display:none` field is the first thing a script skips, and a field a person can
  // tab into is one they will fill in and have their signup silently discarded.
  await expect(page.getByLabel("Leave this field empty", { exact: true })).toHaveCount(1);
  const order = await walk(page, 3);
  expect(order.map((stop) => stop.what)).toEqual(["Email address", "First name", "Join"]);
  problems.push(...dimRings(order, "signup form (light)"));
  await page.emulateMedia({ colorScheme: "dark" });
  problems.push(...dimRings(await walk(page, 3), "signup form (dark)"));
  await page.emulateMedia({ colorScheme: "light" });

  // And the signup itself, whose answer is a public page too.
  await page.getByLabel("Email address", { exact: true }).fill(SUBSCRIBER);
  await page.getByRole("button", { name: "Join", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Check your email", exact: true })).toBeVisible();
  problems.push(...(await sweep(page, "signup accepted")));

  expect(problems).toEqual([]);

  const search = (await (
    await api.get(`/api/v1/workspaces/${ids.ws}/contacts?q=${encodeURIComponent(SUBSCRIBER)}`)
  ).json()) as { data: { id: string }[] };
  ids.contact = search.data[0]?.id ?? "";
  expect(ids.contact, "the submission reached this workspace").not.toBe("");
  expect(await memberships()).toEqual([["digest", "pending"]]);
});

test("the_confirmation_landing_is_sound_and_confirms_the_membership", async () => {
  await page.goto(confirmPath(ids.ws, ids.contact, ids.digest));
  await expect(page.getByRole("heading", { name: "You're subscribed", exact: true })).toBeVisible();
  expect(await memberships()).toEqual([["digest", "subscribed"]]);
  expect(await sweep(page, "confirmation")).toEqual([]);
});

test("the_preference_centre_is_sound_and_reachable_by_keyboard_alone", async () => {
  await page.goto(unsubscribePath(ids.ws, ids.contact));
  await expect(
    page.getByRole("heading", { name: "Your email preferences", exact: true }),
  ).toBeVisible();
  const problems = await sweep(page, "preference centre");

  // Reading order and tab order are the same order, and it ends on the two things a person came
  // here to press. A checkbox nobody can reach is a list nobody can leave.
  const order = await walk(page, 4);
  expect(order.map((stop) => stop.what)).toEqual([
    "Weekly digest",
    LONG_LIST,
    "Save preferences",
    "Unsubscribe from everything",
  ]);
  problems.push(...dimRings(order, "preference centre (light)"));
  await page.emulateMedia({ colorScheme: "dark" });
  // Rotated, because the tab walk above left the focus at the end of the page: this pass is about
  // every ring being visible in the dark scheme, which does not depend on where it starts.
  problems.push(...dimRings(await walk(page, 4), "preference centre (dark)"));
  await page.emulateMedia({ colorScheme: "light" });

  expect(problems).toEqual([]);
});

test("unticking_a_list_stops_it_and_the_page_says_which_one", async () => {
  await page.goto(unsubscribePath(ids.ws, ids.contact));
  await page.getByLabel("Weekly digest", { exact: true }).uncheck();
  await page.getByLabel(LONG_LIST, { exact: true }).check();
  await page.getByRole("button", { name: "Save preferences", exact: true }).click();

  // WHAT AXE CANNOT SEE. There is no script here, so the form posts and the browser navigates: a
  // live region that is already in the document when it loads is not an update, and the thing a
  // screen reader announces on arrival is the document's title. Leaving the title as "Your email
  // preferences" would announce the page the person just left — the same words, as though nothing
  // had happened — so the title is the result, and the region names the lists.
  await expect(page).toHaveTitle("Your preferences are saved");
  await expect(page.getByRole("status")).toHaveText(
    `You will now get ${LONG_LIST}. You have unsubscribed from Weekly digest.`,
  );
  await expect(page.getByLabel("Weekly digest", { exact: true })).not.toBeChecked();
  await expect(page.getByLabel(LONG_LIST, { exact: true })).toBeChecked();
  expect(await sweep(page, "preference centre, saved")).toEqual([]);

  expect(new Map(await memberships())).toEqual(
    new Map([
      ["digest", "unsubscribed"],
      ["long", "subscribed"],
    ]),
  );

  // AND THE PROOF THAT THIS FILE'S CLAIMED ADDRESS REACHED THE SERVER, which the budget recorded
  // at `PUBLIC_PER_MINUTE` rests on. A limiter counter cannot prove it — a fresh window reads the
  // same number whichever address it belongs to — but a public route writes the client address
  // into the consent rows it creates, and that is the server saying what it saw.
  //
  // Sorted, not ordered: both rows are written in one transaction and `now()` is transaction-start
  // time, so their `created_at` tie and the order they come back in says nothing.
  const written = (await consent())
    .filter((row) => row.source === "preference_center")
    .map((row) => `${row.action} ${row.ip}`)
    .sort();
  expect(written).toEqual([`subscribed ${ADDRESS}`, `unsubscribed ${ADDRESS}`]);
});

test("the_preference_centre_still_works_with_javascript_switched_off", async () => {
  // `page.context().setJavaScriptEnabled(false)` does not exist in Playwright — it is a Puppeteer
  // method. `javaScriptEnabled` is a context option, so the context is built with it.
  const silent = await browser.newContext({
    javaScriptEnabled: false,
    extraHTTPHeaders: { [FORWARDED]: ADDRESS },
  });
  meter(silent);
  try {
    const quiet = await silent.newPage();
    await quiet.goto(unsubscribePath(ids.ws, ids.contact));
    await expect(
      quiet.getByRole("heading", { name: "Your email preferences", exact: true }),
    ).toBeVisible();

    // Nothing on these pages is script, so nothing about them should change: the box is ticked
    // by the browser, the form is posted by the browser, and the server renders the answer.
    await quiet.getByLabel("Weekly digest", { exact: true }).check();
    await quiet.getByRole("button", { name: "Save preferences", exact: true }).click();
    await expect(quiet).toHaveTitle("Your preferences are saved");
    await expect(quiet.getByRole("status")).toHaveText("You will now get Weekly digest.");
    await expect(quiet.getByLabel("Weekly digest", { exact: true })).toBeChecked();
    // AND THE CONTROL, because `javaScriptEnabled: false` is a flag whose effect this page cannot
    // show: there is no script on it to fail to run, so a context where the flag quietly stopped
    // working would pass every assertion above and this test would be a JavaScript test wearing
    // the wrong name. A `data:` page costs no public budget and no server request.
    const canary = await silent.newPage();
    await canary.goto(
      "data:text/html,<title>quiet</title><script>document.title='script ran'</script>",
    );
    await expect(canary, "the context still runs script").toHaveTitle("quiet");
  } finally {
    await silent.close();
  }

  // The membership is `subscribed` again; the consent row for it is `resubscribed`, which is the
  // log's word for the same transition and is pinned in Rust.
  expect(new Map(await memberships())).toEqual(
    new Map([
      ["digest", "subscribed"],
      ["long", "subscribed"],
    ]),
  );
  expect((await consent())[0]?.action, "a resubscribe through the preference centre").toBe(
    "resubscribed",
  );
});

test("this_file_spends_what_it_says_it_spends", async () => {
  // The whole suite shares one `PUBLIC_PER_MINUTE` window per address, refusals counted, and once
  // tripped it stays tripped for the minute. If this number moves, update the ledger at the top of
  // this file **and** the margin recorded at `PUBLIC_PER_MINUTE` in `src/http/rate_limit.rs`,
  // which is the worst spender on the branch and is this one.
  expect(spent, `the public requests this file made were: ${spent.join(", ")}`).toHaveLength(SPENT);
});
