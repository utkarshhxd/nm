// SPDX-License-Identifier: AGPL-3.0-only

import { type BrowserContext, type Page, expect, test } from "@playwright/test";

import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

/**
 * The brand-kit screen against the real API (task W6).
 *
 * Three of these are the ones W5 recorded as untestable against its mock, because the mock could
 * not hold a key the screen does not know and could not change under a mounted form:
 *
 *  1. a workspace with no kit row gets an EMPTY kit, never a 404 — on day one that is every
 *     workspace, so a 404 here makes the screen unreachable;
 *  2. a newer kit arriving while the form is mounted REPLACES an untouched form;
 *  3. a save merges onto the LATEST kit, not the one the form was mounted with — which is only
 *     visible through a key the screen does not know, and so survives nothing but a real server.
 */
test.describe.configure({ mode: "serial" });

const SLUG = "brandkitco";

type Kit = {
  logo_url: string | null;
  palette: Record<string, unknown>;
  font_stack: string | null;
  footer: Record<string, unknown>;
};

let admin: BrowserContext;
let page: Page;
let workspaceId = "";
let csrf = "";

const base = () => `/api/v1/workspaces/${workspaceId}`;

async function apiKit(): Promise<Kit> {
  const res = await admin.request.get(`${base()}/brand-kit`);
  expect(res.ok(), `get brand kit -> ${res.status()}`).toBe(true);
  return (await res.json()) as Kit;
}

async function putKit(kit: Kit): Promise<void> {
  const res = await admin.request.put(`${base()}/brand-kit`, {
    data: kit,
    headers: { "X-CSRF-Token": csrf },
  });
  expect(res.ok(), `put brand kit -> ${res.status()} ${await res.text()}`).toBe(true);
}

/** What react-query listens to for "the tab came back": a stale query then asks again. */
const refocus = () =>
  page.evaluate(() => {
    window.dispatchEvent(new Event("visibilitychange"));
  });

const openKit = async (): Promise<void> => {
  await page.goto(`/w/${SLUG}/brand-kit`);
  await expect(page.getByRole("heading", { name: "Brand kit", exact: true })).toBeVisible();
  await expect(page.getByLabel("Logo address", { exact: true })).toBeVisible();
};

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
  await page.getByLabel("Workspace name", { exact: true }).fill("Brand Kit Co");
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

test("a_workspace_with_no_kit_row_gets_an_empty_kit_and_not_a_404", async () => {
  // The route's own answer, before the screen is asked anything: no row, 200, an empty kit.
  const res = await admin.request.get(`${base()}/brand-kit`);
  expect(res.status()).toBe(200);
  expect(await res.json()).toEqual({
    logo_url: null,
    palette: {},
    font_stack: null,
    footer: {},
  });

  // And the screen is reachable, which for a brand-new workspace it would not be on a 404.
  await openKit();
  await expect(page.getByLabel("Logo address", { exact: true })).toHaveValue("");
  await expect(page.getByRole("button", { name: "Save brand kit", exact: true })).toBeVisible();
});

test("a_newer_kit_arriving_while_the_form_is_open_replaces_an_untouched_form", async () => {
  await openKit();
  await expect(page.getByLabel("Font stack", { exact: true })).toHaveValue("");

  // Someone else saves a kit while this form sits there, untouched.
  await putKit({
    logo_url: "https://example.com/logo.png",
    palette: { accent: "#1a56db" },
    font_stack: "Georgia, serif",
    footer: { text: "Elsewhere Ltd" },
  });
  await refocus();

  await expect(page.getByLabel("Font stack", { exact: true })).toHaveValue("Georgia, serif");
  await expect(page.getByLabel("Logo address", { exact: true })).toHaveValue(
    "https://example.com/logo.png",
  );
  await expect(page.getByLabel("Accent colour", { exact: true })).toHaveValue("#1a56db");
});

test("a_save_merges_onto_the_latest_kit_and_a_key_this_screen_cannot_show_survives_it", async () => {
  // A kit holding a key no version of this screen knows: a newer server's, or a newer screen's.
  await putKit({
    logo_url: null,
    palette: { accent: "#1a56db" },
    font_stack: null,
    footer: {},
  });
  await openKit();
  await expect(page.getByLabel("Accent colour", { exact: true })).toHaveValue("#1a56db");

  // It gains the unknown key AFTER this form was mounted. A save that merged onto the kit as it
  // was at mount would drop it, and nothing on the screen would show that it had.
  await putKit({
    logo_url: null,
    palette: { accent: "#1a56db", sparkle: "aurora" },
    font_stack: null,
    footer: { seasonal_note: "winter" },
  });
  await refocus();
  // The form has seen the newer kit before the save is pressed.
  await expect(page.getByLabel("Accent colour", { exact: true })).toHaveValue("#1a56db");

  await page.getByLabel("Text colour", { exact: true }).fill("#111111");
  await page.getByRole("button", { name: "Save brand kit", exact: true }).click();
  await expect(page.getByText("Brand kit saved.")).toBeVisible();

  const saved = await apiKit();
  expect(saved.palette["text"]).toBe("#111111");
  expect(saved.palette["accent"]).toBe("#1a56db");
  expect(saved.palette["sparkle"]).toBe("aurora");
  expect(saved.footer["seasonal_note"]).toBe("winter");
});

test("the_servers_refusal_of_a_logo_scheme_is_what_the_operator_is_shown", async () => {
  await openKit();
  await page.getByLabel("Logo address", { exact: true }).fill("ftp://example.com/logo.png");
  await page.getByRole("button", { name: "Save brand kit", exact: true }).click();
  // Contract section 11: the inline flag and the server's refusal say the SAME sentence, because
  // two explanations of one rule teach an operator to ignore both. The screen flags the URL as it
  // is typed and sends it anyway — the gate is the server's — so both alerts are on screen here,
  // and the literal below is `BrandError::Logo`'s own text, not a constant this lane could drift.
  await expect(page.getByRole("alert")).toHaveText([
    "logo must be an http or https URL with no user name or password",
    "logo must be an http or https URL with no user name or password",
  ]);
  await expect(page.getByText("Brand kit saved.")).toBeHidden();
});
