// SPDX-License-Identifier: AGPL-3.0-only

import { type APIRequestContext, expect, test } from "@playwright/test";

import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/**
 * The hosted signup page in a real browser: `GET /f/{workspace}/{slug}` and the submission.
 *
 * **The question this file exists to answer** is step 7 of M1d Task 9.
 * `src/http/security_headers.rs` sends `form-action 'self'` on every response, and whether a
 * browser checks a *redirect* after a form POST against that policy has differed by browser and
 * has changed over time. `POST /f/…` answers a form with an operator-set `redirect_url` with a
 * `303` to another origin. If a browser refuses it, the route has to render a page carrying a
 * link instead — widening `form-action` is not an option, because that would weaken the policy
 * for every page in the product to fix one route.
 *
 * It is decided here rather than from the specification because the specification is not what
 * ships in browsers.
 */

/**
 * Its own per-IP rate-limit budget. The whole suite shares one server on 127.0.0.1, and the
 * public pages are limited per IP — see the `NM_TRUST_PROXY_HEADERS` comment in
 * `support/server.ts`. This file spends **four** of `PUBLIC_PER_MINUTE`'s 60 against this
 * address: a page view and a submission, twice.
 */
test.use({ extraHTTPHeaders: { "x-forwarded-for": "198.51.100.9" } });

test.describe.configure({ mode: "serial" });

const SLUG = "formsco";

/**
 * The redirect target, and why it is this. `form-action 'self'` is about *origin*, so the target
 * has to be a different origin from the page under test — and it has to actually answer, or a
 * navigation that was allowed and a navigation that was blocked both end on an error page and
 * cannot be told apart. The suite is served from `http://localhost:8090`, so `127.0.0.1` on the
 * same port is a different origin served by the same process: cross-origin for the policy, and
 * reachable for the browser.
 */
const REDIRECT = "http://127.0.0.1:8090/?thanks=1";

let api: APIRequestContext;
let workspaceId = "";
let csrf = "";
let listId = "";

async function post(path: string, data: unknown): Promise<{ id: string }> {
  const res = await api.post(path, { data, headers: { "X-CSRF-Token": csrf } });
  expect(res.ok(), `post ${path} -> ${res.status()} ${await res.text()}`).toBe(true);
  return (await res.json()) as { id: string };
}

/** The statuses this list holds for `email`, read back through the API. */
async function statuses(email: string): Promise<string[]> {
  const found = (await (
    await api.get(`/api/v1/workspaces/${workspaceId}/contacts?q=${encodeURIComponent(email)}`)
  ).json()) as { data: { id: string }[] };
  if (found.data.length === 0) return [];
  const id = found.data[0]?.id ?? "";
  const lists = (await (
    await api.get(`/api/v1/workspaces/${workspaceId}/contacts/${id}/lists`)
  ).json()) as { data: { status: string }[] };
  return lists.data.map((l) => l.status);
}

test.beforeAll(async ({ playwright }) => {
  api = await playwright.request.newContext({ baseURL: "http://localhost:8090" });
  const signedIn = await api.post("/api/v1/auth/login", {
    data: { email: ADMIN_EMAIL, password: ADMIN_PASSWORD },
  });
  expect(signedIn.ok()).toBe(true);
  const me = (await (await api.get("/api/v1/me")).json()) as { csrf_token: string };
  csrf = me.csrf_token;
  workspaceId = (
    await post("/api/v1/workspaces", { slug: SLUG, name: "Forms Co", timezone: "Europe/London" })
  ).id;

  const base = `/api/v1/workspaces/${workspaceId}`;
  listId = (await post(`${base}/lists`, { name: "News", double_optin: false })).id;
  const fields = [{ target: { kind: "email" }, required: true, label: "Email address" }];
  await post(`${base}/forms`, {
    slug: "plain",
    name: "Plain",
    list_ids: [listId],
    fields,
    headline: "Join the list",
    button_label: "Join",
  });
  await post(`${base}/forms`, {
    slug: "redirecting",
    name: "Redirecting",
    list_ids: [listId],
    fields,
    headline: "Join the list",
    button_label: "Join",
    redirect_url: REDIRECT,
  });
});

test.afterAll(async () => {
  await api.dispose();
});

test("the built-in page is rendered, and the honeypot is not shown to a person", async ({
  page,
}) => {
  await page.goto(`/f/${SLUG}/plain`);
  await expect(page.getByRole("heading", { name: "Join the list" })).toBeVisible();

  // The decoy is in the markup and out of sight: a person never fills it in, so a filled one is
  // a script. It is positioned off-screen rather than `display: none`, which is the first thing
  // a script skips — so the assertion is where the browser actually laid it out, measured, and
  // not `toBeHidden`, which a 1px off-screen box does not satisfy.
  const decoy = page.getByLabel("Website", { exact: true });
  const box = await decoy.boundingBox();
  expect(box, "the honeypot must be in the page for a script to find").not.toBeNull();
  expect(box?.x ?? 0, `the honeypot is on screen at ${JSON.stringify(box)}`).toBeLessThan(-1000);
  await expect(decoy).toHaveAttribute("tabindex", "-1");

  await page.getByLabel("Email address", { exact: true }).fill("browser@example.test");
  await page.getByRole("button", { name: "Join", exact: true }).click();

  await expect(page.getByRole("heading", { name: "Thank you" })).toBeVisible();
  expect(await statuses("browser@example.test")).toEqual(["subscribed"]);
});

/**
 * Step 7, and its answer.
 *
 * **This browser refuses a cross-origin redirect after a form POST under `form-action 'self'`.**
 * Measured, not assumed: an earlier revision of `POST /f/…` answered `303 See Other` to
 * `redirect_url`, the POST went through, the subscription was written — and Chromium blocked the
 * navigation with
 *
 *     Sending form data to 'http://localhost:8090/f/formsco/redirecting' violates the following
 *     Content Security Policy directive: "form-action 'self'". The request has been blocked.
 *
 * naming the form's own URL rather than the redirect's, which is why nothing about the message
 * points at the redirect. The subscriber would have been left on a blank tab with their signup
 * silently complete. So the route renders the link below instead, and `form-action` was not
 * widened — that would weaken the policy for every page in the product to fix one route.
 *
 * This test is now the regression test for both halves: that the link is offered, and that
 * following it works. If a future browser stops checking redirects, that does not make this fail
 * — it would simply make the `303` possible again, and the place to find that out is here.
 */
test("the operator's URL is offered as a link the browser will follow", async ({ page }) => {
  const refusals: string[] = [];
  page.on("console", (message) => {
    if (/content security policy|form-action/i.test(message.text())) {
      refusals.push(message.text());
    }
  });

  await page.goto(`/f/${SLUG}/redirecting`);
  await page.getByLabel("Email address", { exact: true }).fill("redirected@example.test");
  await page.getByRole("button", { name: "Join", exact: true }).click();

  // The submission committed, and the browser stayed on a page that says so.
  await expect.poll(async () => statuses("redirected@example.test")).toEqual(["subscribed"]);
  await expect(page.getByRole("heading", { name: "Thank you" })).toBeVisible();
  expect(refusals, `the submission itself was refused: ${refusals.join(" | ")}`).toEqual([]);

  // A link is a navigation, not a form submission, so `form-action` does not apply to it: the
  // operator's own page is still where the subscriber ends up, in one click instead of none.
  await page.getByRole("link", { name: "Continue", exact: true }).click();
  await expect(page).toHaveURL(REDIRECT);
});
