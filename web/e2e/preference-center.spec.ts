// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import {
  type APIRequestContext,
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
 * This file's two per-IP rate-limit budgets, from RFC 5737's documentation range. No other spec
 * uses either address; one that reused them would inherit this file's spending.
 *
 * **Two addresses, because the two halves of this file are two browsers.** The limiter keys on the
 * address, so the JavaScript-off context carrying an address of its own costs nothing and keeps
 * the worse of the two margins at 10× rather than 6×.
 *
 * `ADDRESS` — **6** of `PUBLIC_PER_MINUTE`'s 60:
 *
 * 1. `GET /f/{ws}/{slug}` — the signup page, swept.
 * 2. `POST /f/{ws}/{slug}` — the signup, whose answer ("Check your email") is swept too.
 * 3. `GET /c/{token}` — the confirmation landing, swept.
 * 4. `GET /u/{token}` — the preference centre, swept, walked by keyboard.
 * 5. `GET /u/{token}` — the preference centre again, to change something on it.
 * 6. `POST /u/{token}` — the change.
 *
 * `QUIET` — **4**, everything with JavaScript switched off:
 *
 * 1. `GET /u/{token}` and 2. `POST /u/{token}` — the preference centre saves.
 * 3. `GET /f/{ws}/{slug}` and 4. `POST /f/{ws}/{slug}` — the signup form submits. This is the page
 *    an operator embeds on somebody else's site, so it is the one where a visitor with scripting
 *    off is least hypothetical.
 *
 * The last test asserts both counts off a context-level listener. Every sweep loops over two
 * colour schemes and three viewport widths **without reloading**: `prefers-color-scheme` and the
 * viewport are live, so a sweep of one page costs one request rather than six. That is the
 * difference between this file fitting in the budget and not.
 */
const ADDRESS = "198.51.100.12";
const QUIET = "198.51.100.13";
const FORWARDED = "x-forwarded-for";
const SPENT = 6;
const SPENT_QUIET = 4;

test.use({ extraHTTPHeaders: { [FORWARDED]: ADDRESS } });
test.describe.configure({ mode: "serial" });

const SLUG = "access";
const SUBSCRIBER = "reader@example.test";
/** A second address, so the JavaScript-off signup is a signup and not a repeat submission. */
const QUIET_SUBSCRIBER = "quiet-reader@example.test";
/**
 * The widest letter and nowhere to break it, at each ceiling the product allows: an operator-typed
 * string is the only thing on these pages long enough to reflow them.
 *
 * All three, because `page::STYLE`'s reflow rule claims to protect all three and a claim with one
 * case tested is a claim with two cases assumed. `lists::validate_name` caps a list name at 100,
 * `forms::routes::HEADLINE_MAX` a headline at 200 and `LABEL_MAX` a field label at 100 — so every
 * one of these is a string an operator can really type today.
 */
const LONG_LIST = "W".repeat(100);
const LONG_HEADLINE = "W".repeat(200);
const LONG_LABEL = "W".repeat(100);

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

let api: APIRequestContext;
/** The ordinary browser, on `ADDRESS`. */
let context: BrowserContext;
let page: Page;
/** The same browser with script execution switched off, on `QUIET`. */
let silent: BrowserContext;
let quiet: Page;
let csrf = "";
const ids = { ws: "", digest: "", long: "", contact: "" };
/** Every request to a public route each browser made, for the budget assertion. */
const spent: string[] = [];
const spentQuiet: string[] = [];

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

/** The consent log for a contact, newest first. An authenticated read: no public budget. */
async function consent(
  contact = ids.contact,
): Promise<{ action: string; source: string; ip: string | null }[]> {
  const got = (await (
    await api.get(`/api/v1/workspaces/${ids.ws}/contacts/${contact}/consent`)
  ).json()) as { data: { action: string; source: string; ip: string | null }[] };
  return got.data;
}

/** The id of the contact at `email` in this workspace, or "" if there is none. */
async function contactId(email: string): Promise<string> {
  const found = (await (
    await api.get(`/api/v1/workspaces/${ids.ws}/contacts?q=${encodeURIComponent(email)}`)
  ).json()) as { data: { id: string }[] };
  return found.data[0]?.id ?? "";
}

/** Counts what a context's pages send to `/c/`, `/u/` and `/f/`, into that context's own ledger. */
function meter(ctx: BrowserContext, into: string[]): void {
  ctx.on("request", (request) => {
    const path = new URL(request.url()).pathname;
    if (/^\/[cuf]\//.test(path)) into.push(`${request.method()} ${path.slice(0, 12)}…`);
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
        // `color-contrast` by name on every scan, not just "no violations". These pages are
        // written entirely in `oklch()` — the same thing that forced the focus ring to be
        // measured through a canvas — and a contrast rule that cannot resolve a colour lands in
        // `incomplete`, which reads exactly like a clean page. This sweep's colour evidence is
        // worth nothing unless that rule actually passed.
        found.push(
          ...(await axeProblems(new AxeBuilder({ page: p }), at, {
            requireRules: ["color-contrast"],
          })),
        );
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

test.beforeAll(async ({ playwright, browser }) => {
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
      { target: { kind: "first_name" }, required: false, label: LONG_LABEL },
    ],
    headline: LONG_HEADLINE,
    button_label: "Join",
  });

  context = await browser.newContext();
  meter(context, spent);
  page = await context.newPage();

  // `page.context().setJavaScriptEnabled(false)` does not exist in Playwright — it is a Puppeteer
  // method. `javaScriptEnabled` is a context option, so the quiet browser is a context of its own,
  // and it claims its own address so its spending does not come out of the margin above.
  silent = await browser.newContext({
    javaScriptEnabled: false,
    extraHTTPHeaders: { [FORWARDED]: QUIET },
  });
  meter(silent, spentQuiet);
  quiet = await silent.newPage();
});

test.afterAll(async () => {
  await silent.close();
  await context.close();
  await api.dispose();
});

/* ------------------------------------------------------------------------------- the pages --- */

test("the_signup_form_is_sound_and_its_decoy_is_not_in_the_tab_order", async () => {
  const found = await page.goto(`/f/${SLUG}/join`);
  expect(found?.status()).toBe(200);
  await expect(page.getByRole("heading", { name: LONG_HEADLINE, exact: true })).toBeVisible();
  const problems = await sweep(page, "signup form");

  // The decoy, and both halves of what it depends on.
  //
  // It is laid out off-screen rather than hidden, because `display: none` is the first thing a
  // script skips and a skipped decoy catches nothing. So the assertion is where the browser
  // actually put it — a real box, a long way to the left — and not `toBeHidden()`, which a 1px
  // off-screen box does not satisfy. `public-form.spec.ts` measures the same thing for the same
  // reason; it is repeated here because this file's claim about the decoy is an accessibility
  // claim, and because nothing in Rust pins what `class="hp"` does, only that it is emitted.
  //
  // The other half is that a keyboard never reaches it: a tabbable decoy is one a person fills in
  // and has their signup silently discarded, and it would show up below as a fourth stop.
  const decoy = page.getByLabel("Leave this field empty", { exact: true });
  await expect(decoy).toHaveCount(1);
  const box = await decoy.boundingBox();
  expect(box, "the decoy must be laid out, so a script finds it").not.toBeNull();
  expect(box?.x ?? 0, `the decoy is on screen at ${JSON.stringify(box)}`).toBeLessThan(-1000);

  const order = await walk(page, 3);
  expect(order.map((stop) => stop.what)).toEqual(["Email address", LONG_LABEL, "Join"]);
  problems.push(...dimRings(order, "signup form (light)"));
  await page.emulateMedia({ colorScheme: "dark" });
  // Names as well as count: `walk` asserting its length alone would be satisfied by three presses
  // that never left the first control.
  const dark = await walk(page, 3);
  expect(dark.map((stop) => stop.what).sort()).toEqual(
    ["Email address", LONG_LABEL, "Join"].sort(),
  );
  problems.push(...dimRings(dark, "signup form (dark)"));
  await page.emulateMedia({ colorScheme: "light" });

  // And the signup itself, whose answer is a public page too.
  await page.getByLabel("Email address", { exact: true }).fill(SUBSCRIBER);
  await page.getByRole("button", { name: "Join", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Check your email", exact: true })).toBeVisible();
  problems.push(...(await sweep(page, "signup accepted")));

  expect(problems).toEqual([]);

  ids.contact = await contactId(SUBSCRIBER);
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
  const controls = [
    "Weekly digest",
    LONG_LIST,
    "Save preferences",
    "Unsubscribe from everything",
  ];
  const order = await walk(page, 4);
  expect(order.map((stop) => stop.what)).toEqual(controls);
  problems.push(...dimRings(order, "preference centre (light)"));
  await page.emulateMedia({ colorScheme: "dark" });
  // Sorted, not ordered: the walk above left the focus at the end of the page, so this one starts
  // in the middle and comes back round. It is still asserted by name — `walk` asserting only that
  // it took four stops would be satisfied by four presses that never left the first checkbox, and
  // `dimRings` would then report four clean rings on one control.
  const dark = await walk(page, 4);
  expect(dark.map((stop) => stop.what).sort()).toEqual([...controls].sort());
  problems.push(...dimRings(dark, "preference centre (dark)"));
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
  // AND THE HEADING DOES NOT FOLLOW IT. A heading describes what is under it (WCAG 2.4.6), and
  // what is under this one is a form for changing preferences, not a receipt. If `page::shell`
  // ever goes back to rendering one string into both, a screen reader hears "Your preferences are
  // saved" twice in a row — as the document's name, then as the heading of a form that has saved
  // nothing yet — and this is what says so.
  await expect(
    page.getByRole("heading", { name: "Your email preferences", exact: true }),
  ).toBeVisible();
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

/**
 * THE CONTROL FOR THE TWO TESTS BELOW, and it is run before them.
 *
 * `javaScriptEnabled: false` is a flag whose effect these pages cannot show: there is no script on
 * any of them to fail to run, so a context in which the flag quietly stopped working would pass
 * every assertion in those tests and they would be JavaScript tests wearing the wrong name. The
 * same `data:` page is opened in both browsers and must come out differently, because a control
 * that reads the same either way is not a control. It costs no server request and no budget.
 */
const CANARY = "data:text/html,<title>quiet</title><script>document.title='script ran'</script>";

test("the_quiet_browser_runs_no_script_and_the_ordinary_one_does", async () => {
  const ran = await context.newPage();
  const did_not = await silent.newPage();
  try {
    await ran.goto(CANARY);
    await expect(ran, "the ordinary browser did not run the canary's script").toHaveTitle(
      "script ran",
    );
    await did_not.goto(CANARY);
    await expect(did_not, "the quiet browser still runs script").toHaveTitle("quiet");
  } finally {
    await ran.close();
    await did_not.close();
  }
});

test("the_preference_centre_still_works_with_javascript_switched_off", async () => {
  await quiet.goto(unsubscribePath(ids.ws, ids.contact));
  await expect(
    quiet.getByRole("heading", { name: "Your email preferences", exact: true }),
  ).toBeVisible();

  // Nothing on this page is script, so nothing about it should change: the box is ticked by the
  // browser, the form is posted by the browser, and the server renders the answer.
  await quiet.getByLabel("Weekly digest", { exact: true }).check();
  await quiet.getByRole("button", { name: "Save preferences", exact: true }).click();
  await expect(quiet).toHaveTitle("Your preferences are saved");
  await expect(quiet.getByRole("status")).toHaveText("You will now get Weekly digest.");
  await expect(quiet.getByLabel("Weekly digest", { exact: true })).toBeChecked();

  // The membership is `subscribed` again; the consent row for it is `resubscribed`, which is the
  // log's word for the same transition and is pinned in Rust. Its address is `QUIET`, which is
  // what proves this browser's own header reached the server — the budget below rests on it.
  expect(new Map(await memberships())).toEqual(
    new Map([
      ["digest", "subscribed"],
      ["long", "subscribed"],
    ]),
  );
  const latest = (await consent())[0];
  expect([latest?.action, latest?.source, latest?.ip]).toEqual([
    "resubscribed",
    "preference_center",
    QUIET,
  ]);
});

/**
 * The one the coordinator's review asked for, and the one that matters most of the three.
 *
 * `/f/{workspace}/{slug}` is the page an operator **embeds on somebody else's site**, reached by
 * visitors whose browser the operator knows nothing about. A signup that needs script to submit is
 * a signup that silently does not happen, and `src/public/page.rs` promises these pages work
 * without it. Nothing else on this branch tests that for the form.
 */
test("the_signup_form_still_submits_with_javascript_switched_off", async () => {
  await quiet.goto(`/f/${SLUG}/join`);
  await expect(quiet.getByRole("heading", { name: LONG_HEADLINE, exact: true })).toBeVisible();
  await quiet.getByLabel("Email address", { exact: true }).fill(QUIET_SUBSCRIBER);
  await quiet.getByRole("button", { name: "Join", exact: true }).click();
  await expect(quiet.getByRole("heading", { name: "Check your email", exact: true })).toBeVisible();

  // The answer page is the same for every submitter, so it says nothing about whether this one
  // was written. The database does.
  const signed_up = await contactId(QUIET_SUBSCRIBER);
  expect(signed_up, "the scriptless submission reached this workspace").not.toBe("");
  const asked = (await consent(signed_up))[0];
  expect([asked?.action, asked?.source, asked?.ip]).toEqual(["requested", "form", QUIET]);
});

test("this_file_spends_what_it_says_it_spends", async () => {
  // The whole suite shares one `PUBLIC_PER_MINUTE` window per address, refusals counted, and once
  // tripped it stays tripped for the minute. If either number moves, update the ledger at the top
  // of this file **and** the margin recorded at `PUBLIC_PER_MINUTE` in `src/http/rate_limit.rs`,
  // where this file is the worst spender on the branch.
  expect(spent, `on ${ADDRESS}: ${spent.join(", ")}`).toHaveLength(SPENT);
  expect(spentQuiet, `on ${QUIET}: ${spentQuiet.join(", ")}`).toHaveLength(SPENT_QUIET);
});
