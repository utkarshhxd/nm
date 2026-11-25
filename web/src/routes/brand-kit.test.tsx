// SPDX-License-Identifier: AGPL-3.0-only

// The brand-kit screen in a real browser, with the brand-kit route stubbed at the HTTP boundary
// the way `/api/v1/me` already is: W6 made `brand-kit-source.ts` call the server, and these tests
// have no server behind them. The stub stands in for the one refusal this screen cannot make
// itself — a logo URL the server will not take — and the screen flags such a URL while it is
// typed and still sends it. What only a real server can show (an unknown palette key surviving a
// save, a newer kit arriving under a mounted form) is in `e2e/brand-kit.spec.ts`.

import AxeBuilder from "@axe-core/playwright";
import { type Page, expect } from "@playwright/test";

import type { components } from "../api/schema";
import { test } from "../../e2e/support/app";
import { expectNoViolations } from "../../e2e/support/axe";
import { LOGO_MESSAGE, contrast, isColour, logoAllowed, resync, toDraft, toKit } from "./brand-kit-model";
import type { BrandKit } from "./brand-kit-source";

type Me = components["schemas"]["MeResponse"];

const WS = "11111111-1111-4111-8111-111111111111";
const FLAG = "Use a colour written as #rgb or #rrggbb, such as #1a56db.";
const EMPTY: BrandKit = { logo_url: null, palette: {}, font_stack: null, footer: {} };

/**
 * The brand-kit route, standing in for the server: one kit per page, `GET` answers with it and
 * `PUT` replaces it. A workspace with no row answers an EMPTY kit and never a 404 — the server's
 * own behaviour, pinned against the real one in `e2e/brand-kit.spec.ts`.
 *
 * The logo gate is the server's, and is the reason this stub exists at all: the screen sends a
 * URL it has already flagged, and what the operator is shown is the refusal that comes back.
 *
 * The returned handle is the other operator: `set` is what a save made in another window leaves
 * behind, and `read` is what the row holds afterwards. A route stub can hold a key this screen
 * does not know, which the W5 in-memory mock could not — that the real row holds one too is
 * pinned in `e2e/brand-kit.spec.ts`.
 */
type Kits = { set: (k: BrandKit) => void; read: () => BrandKit };

async function stubKit(page: Page): Promise<Kits> {
  let kit: BrandKit = structuredClone(EMPTY);
  await page.route(/\/api\/v1\/workspaces\/[^/]+\/brand-kit$/, async (route) => {
    if (route.request().method() === "PUT") {
      const sent = route.request().postDataJSON() as BrandKit;
      if (sent.logo_url !== null && !logoAllowed(sent.logo_url)) {
        // An RFC 9457 problem as `src/error.rs` writes one, so `problemMessage` reads it the way
        // it reads the server's.
        return route.fulfill({
          status: 422,
          contentType: "application/problem+json",
          body: JSON.stringify({
            type: "urn:nm:problem:validation",
            title: "Unprocessable Entity",
            status: 422,
            detail: "request validation failed",
            errors: [{ field: "logo_url", message: LOGO_MESSAGE }],
          }),
        });
      }
      kit = sent;
    }
    return route.fulfill({ json: kit });
  });
  return { set: (k) => (kit = structuredClone(k)), read: () => kit };
}

async function open(
  page: Page,
  app: string,
  role: "viewer" | "editor" | "admin" = "admin",
): Promise<Kits> {
  const me: Me = {
    csrf_token: "t",
    display_name: "Ada",
    email: "ada@example.com",
    id: "22222222-2222-4222-8222-222222222222",
    is_instance_admin: false,
    passkey_count: 0,
    totp_enabled: false,
    workspaces: [{ id: WS, name: "Acme", role, slug: "acme" }],
  };
  await page.route("**/api/v1/me", (route) => route.fulfill({ json: me }));
  const kits = await stubKit(page);
  await page.goto(`${app}/w/acme/brand-kit`);
  await expect(page.getByRole("heading", { name: "Brand kit", level: 2 })).toBeVisible();
  await expect(page.getByLabel("Logo address")).toBeVisible();
  return kits;
}

/** What react-query listens to for "the tab came back": a stale query then asks again. */
const refocus = (page: Page) =>
  page.evaluate(() => {
    window.dispatchEvent(new Event("visibilitychange"));
  });

const scan = (page: Page, label: string) =>
  expectNoViolations(new AxeBuilder({ page }), label, { requireRules: ["color-contrast"] });

test("isColour_takes_three_or_six_hex_digits_and_nothing_else", () => {
  for (const ok of ["#fff", "#1A56db", "#000000"]) expect(isColour(ok), ok).toBe(true);
  for (const bad of ["", "red", "fff", "#ffff", "#12345g", "#1a56db ", "rgb(0,0,0)"]) {
    expect(isColour(bad), bad).toBe(false);
  }
});

const BASE: BrandKit = {
  logo_url: "https://a.example/l.png",
  palette: { background: "#fff", text: "#111", extra: "#abc" },
  font_stack: "Georgia, serif",
  footer: { text: "Old", note: "kept" },
};
const DRAFT = { logo: "", background: "", text: "#222", accent: "", accent_text: "", font: "", footerText: "", footerAddress: "" };

test("toKit_omits_empty_values_keeps_keys_it_does_not_edit_and_is_always_objects", () => {
  const kit = toKit(BASE, DRAFT);
  // Empty becomes null or absent, never "": the database takes `null` for the text columns and the
  // renderer reads a missing palette key as "use the default".
  expect(kit.logo_url).toBeNull();
  expect(kit.font_stack).toBeNull();
  expect(kit.palette).toEqual({ text: "#222", extra: "#abc" });
  expect(kit.footer).toEqual({ note: "kept" });
  // `jsonb_typeof(...) = 'object'`: an empty kit is `{}`, not null and not `[]`.
  const empty = toKit({ logo_url: null, palette: {}, font_stack: null, footer: {} }, { ...DRAFT, text: "" });
  expect(empty.palette).toEqual({});
  expect(Array.isArray(empty.palette) || Array.isArray(empty.footer)).toBe(false);
  expect(toKit(BASE, { ...DRAFT, logo: "  https://b.example/x.png ", font: " Arial " })).toMatchObject({
    logo_url: "https://b.example/x.png",
    font_stack: "Arial",
  });
});

test("toDraft_reads_every_field_from_its_own_key", () => {
  const kit: BrandKit = {
    logo_url: "https://a.example/l.png",
    palette: { background: "#111111", text: "#222222", accent: "#333333", accent_text: "#444444" },
    font_stack: "Georgia",
    footer: { text: "T", address: "A" },
  };
  // Eight different values, so a field read from the wrong key shows up as a swap.
  expect(toDraft(kit)).toEqual({
    logo: "https://a.example/l.png",
    font: "Georgia",
    footerText: "T",
    footerAddress: "A",
    background: "#111111",
    text: "#222222",
    accent: "#333333",
    accent_text: "#444444",
  });
  expect(toDraft({ logo_url: null, palette: {}, font_stack: null, footer: {} })).toEqual(
    Object.fromEntries(Object.keys(toDraft(kit)).map((k) => [k, ""])),
  );
});

test("a_known_key_holding_a_non_string_is_shown_blank_and_left_alone_by_a_save", () => {
  // What a newer server might write under a key this screen knows: it must neither crash the
  // screen nor be dropped by a save that did not touch it.
  const richer = { light: "#fff", dark: "#000" };
  const kit: BrandKit = {
    logo_url: null,
    palette: { accent: richer, background: 5, text: null, accent_text: ["#fff"], other: [1] },
    font_stack: null,
    footer: { text: { html: "<b>x</b>" }, address: 7 },
  };
  const draft = toDraft(kit);
  expect(Object.values(draft).every((v) => v === "")).toBe(true);
  expect(toKit(kit, draft)).toEqual(kit);
  // The operator typing over one replaces it; the others stay.
  const edited = toKit(kit, { ...draft, accent: "#123456" });
  expect(edited.palette).toEqual({ ...kit.palette, accent: "#123456" });
  expect(edited.footer).toEqual(kit.footer);
});

test("a_newer_kit_replaces_an_untouched_form_and_never_one_the_operator_has_typed_in", () => {
  const was: BrandKit = { logo_url: null, palette: { text: "#111" }, font_stack: null, footer: {} };
  const now: BrandKit = { logo_url: "https://a.example/l.png", palette: { text: "#222" }, font_stack: "Georgia", footer: {} };
  expect(resync(toDraft(was), was, now)).toEqual(toDraft(now));
  const typed = { ...toDraft(was), font: "Arial" };
  expect(resync(typed, was, now)).toBe(typed);
});

test("a_whitespace_only_field_clears_a_string_value_like_an_empty_one", () => {
  const kit: BrandKit = { logo_url: "https://a.example/l.png", palette: { text: "#111" }, font_stack: "Georgia", footer: { text: "x" } };
  const out = toKit(kit, { ...toDraft(kit), text: "   ", logo: " 	", font: "  ", footerText: " " });
  expect(out).toEqual({ logo_url: null, palette: {}, font_stack: null, footer: {} });
});

test("the form renders every field with a label and passes axe", async ({ page, app }) => {
  await open(page, app);
  for (const name of [
    "Logo address",
    "Background colour",
    "Text colour",
    "Accent colour",
    "Text on the accent colour",
    "Font stack",
    "Footer text",
    "Postal address",
  ]) {
    await expect(page.getByLabel(name, { exact: true })).toBeVisible();
  }
  await expect(page.getByRole("button", { name: "Save brand kit" })).toBeVisible();
  await scan(page, "brand kit");
});

test("a colour is flagged while it is wrong, cleared when it is right, and never blocks typing", async ({ page, app }) => {
  await open(page, app);
  const bg = page.getByLabel("Background colour", { exact: true });
  // A valid shorthand is accepted; one more character makes it wrong, and the box still holds it.
  await bg.fill("#1a5");
  await expect(bg).not.toHaveAttribute("aria-invalid", "true");
  await bg.pressSequentially("6");
  await expect(bg).toHaveValue("#1a56");
  await expect(bg).toHaveAttribute("aria-invalid", "true");
  await expect(page.getByText(FLAG)).toBeVisible();
  await expect(bg).toHaveAccessibleDescription(/Use a colour written as/);
  await scan(page, "brand kit with a flagged colour");
  await bg.pressSequentially("db");
  await expect(bg).not.toHaveAttribute("aria-invalid", "true");
  await expect(page.getByText(FLAG)).toHaveCount(0);
});

test("a logo URL is flagged inline but the save is still sent, and the server's refusal is shown", async ({ page, app }) => {
  await open(page, app);
  await page.getByLabel("Logo address").fill("javascript:alert(1)");
  await page.getByLabel("Text colour", { exact: true }).fill("#222222");
  // One sentence for one rule: the flag and the server's refusal say the same thing, so the
  // refusal is told apart by where it is: the field's own error, then a second alert for the save.
  await expect(page.locator("#bk-logo-error")).toHaveText(LOGO_MESSAGE);
  await expect(page.getByRole("alert")).toHaveCount(1);
  await page.getByRole("button", { name: "Save brand kit" }).click();
  // The server's answer is the field's own sentence word for word: two explanations of one rule
  // teach an operator to ignore both.
  await expect(page.getByRole("alert")).toHaveText([LOGO_MESSAGE, LOGO_MESSAGE]);
  await expect(page.getByText("Brand kit saved.")).toHaveCount(0);
  // Nothing the operator typed is lost to the failure.
  await expect(page.getByLabel("Logo address")).toHaveValue("javascript:alert(1)");
  await expect(page.getByLabel("Text colour", { exact: true })).toHaveValue("#222222");
  await scan(page, "brand kit with a failed save");
});

test("a logo is http or https only: mailto and tel are flagged, a relative path is flagged, https is not", async ({ page, app }) => {
  await open(page, app);
  const logo = page.getByLabel("Logo address");
  for (const bad of ["mailto:a@b.example", "tel:+15555550100", "/logo.png", "logo.png"]) {
    await logo.fill(bad);
    await expect(page.locator("#bk-logo-error"), bad).toHaveText(LOGO_MESSAGE);
  }
  for (const good of ["https://a.example/l.png", "http://a.example/l.png"]) {
    await logo.fill(good);
    await expect(page.locator("#bk-logo-error"), good).toHaveCount(0);
  }
});

test("a good save says so, a failure goes as soon as the operator edits, and what was saved is what comes back", async ({ page, app }) => {
  await open(page, app);
  // The nav link needs a page to leave for and return from.
  await page.route(/\/api\/v1\/workspaces\/[^/]+\/sending-domains(\?.*)?$/, (route) => route.fulfill({ json: { data: [] } }));
  const logo = page.getByLabel("Logo address");
  await logo.fill("ftp://x.example/l.png");
  await page.getByRole("button", { name: "Save brand kit" }).click();
  await expect(page.getByRole("alert")).toHaveCount(2);
  // Editing clears the stale save error at once, before anything is sent again.
  await logo.fill("https://x.example/l.png");
  await expect(page.getByRole("alert")).toHaveCount(0);
  await page.getByLabel("Text colour", { exact: true }).fill("#222222");
  await page.getByLabel("Font stack").fill("Georgia, serif");
  await page.getByLabel("Footer text").fill("You are on our list.");
  await page.getByLabel("Postal address").fill("1 Main St");
  await page.getByRole("button", { name: "Save brand kit" }).click();
  await expect(page.getByRole("status").filter({ hasText: "Brand kit saved." })).toBeVisible();
  // ...and the confirmation does not outlive the next edit.
  await page.getByLabel("Font stack").fill("Georgia, serif;");
  await expect(page.getByText("Brand kit saved.")).toHaveCount(0);
  await page.getByLabel("Font stack").fill("Georgia, serif");
  // Leave the screen and come back: the form is filled from what the source holds.
  await page.getByRole("link", { name: "Sending domains" }).click();
  await expect(page.getByRole("heading", { name: "Sending domains", level: 2, exact: true })).toBeVisible();
  await page.getByRole("link", { name: "Brand kit" }).click();
  await expect(page.getByLabel("Logo address")).toHaveValue("https://x.example/l.png");
  await expect(page.getByLabel("Text colour", { exact: true })).toHaveValue("#222222");
  await expect(page.getByLabel("Font stack")).toHaveValue("Georgia, serif");
  await expect(page.getByLabel("Footer text")).toHaveValue("You are on our list.");
  await expect(page.getByLabel("Postal address")).toHaveValue("1 Main St");
});

// The postal address is what every email must legally say: changing it is an admin's job. This
// disables controls only; the server is the gate.
for (const role of ["viewer", "editor"] as const) {
  test(`${role}: sees the kit read-only and no save button`, async ({ page, app }) => {
    await open(page, app, role);
    await expect(page.getByLabel("Logo address")).toBeDisabled();
    await expect(page.getByRole("button", { name: "Save brand kit" })).toHaveCount(0);
    await scan(page, `brand kit, read-only for a ${role}`);
  });
}

test("contrast_is_the_wcag_ratio_and_reads_three_digit_and_either_case", () => {
  expect(contrast("#000", "#FFFFFF")).toBeCloseTo(21, 5);
  expect(contrast("#fff", "#fff")).toBeCloseTo(1, 5);
  // The standard boundary: #767676 on white is 4.54, one step lighter is 4.48.
  expect(contrast("#767676", "#fff")).toBeGreaterThanOrEqual(4.5);
  expect(contrast("#777777", "#fff")).toBeLessThan(4.5);
  expect(contrast("#fff", "#767676")).toBe(contrast("#767676", "#fff"));
});

// "a workspace with no kit gets an empty one, not a 404" was pinned here against the W5 mock.
// It is the server's behaviour, not this screen's, so it moved to `e2e/brand-kit.spec.ts`, where
// the real route answers it. The form below still proves the screen is usable in that state.

test("a workspace with no kit opens an empty, editable form", async ({ page, app }) => {
  await open(page, app);
  for (const name of ["Logo address", "Background colour", "Text colour", "Font stack", "Footer text", "Postal address"]) {
    await expect(page.getByLabel(name, { exact: true })).toHaveValue("");
  }
  await expect(page.getByRole("alert")).toHaveCount(0);
});

const WARN = /contrast/i;

test("a pair that is rendered together and fails contrast is warned about, with the ratio, and still saves", async ({ page, app }) => {
  await open(page, app);
  await page.getByLabel("Background colour", { exact: true }).fill("#ffffff");
  await page.getByLabel("Text colour", { exact: true }).fill("#aaaaaa");
  const warning = page.getByText(/Text and background colours/);
  await expect(warning).toContainText("contrast 2.32:1");
  await expect(warning).toContainText("4.5:1");
  await scan(page, "brand kit with a contrast warning");
  await page.getByRole("button", { name: "Save brand kit" }).click();
  await expect(page.getByRole("status").filter({ hasText: "Brand kit saved." })).toBeVisible();
  await page.getByLabel("Text colour", { exact: true }).fill("#222222");
  await expect(page.getByText(WARN)).toHaveCount(0);
});

test("the accent pair is checked, and colours that are never shown together are not", async ({ page, app }) => {
  await open(page, app);
  await page.getByLabel("Accent colour", { exact: true }).fill("#1a56db");
  await page.getByLabel("Text on the accent colour", { exact: true }).fill("#1a5fdb");
  await expect(page.getByText(/Accent and accent text colours/)).toBeVisible();
  // Background and accent are not a rendered pair; one half missing or invalid is not a pair.
  await page.getByLabel("Text on the accent colour", { exact: true }).fill("#fff");
  await page.getByLabel("Background colour", { exact: true }).fill("#1a56db");
  await page.getByLabel("Text colour", { exact: true }).fill("#1a");
  await expect(page.getByText(WARN)).toHaveCount(0);
});

test("a_pair_just_under_the_line_never_reads_as_the_line", async ({ page, app }) => {
  // #777777 on white is 4.478:1. Rounded to one place that is "4.5:1, below the 4.5:1", a warning
  // that argues with itself; it is shown rounded down, to two places.
  await open(page, app);
  await page.getByLabel("Background colour", { exact: true }).fill("#ffffff");
  await page.getByLabel("Text colour", { exact: true }).fill("#777777");
  const warning = page.getByText(/Text and background colours/);
  await expect(warning).toContainText("contrast 4.47:1, below the 4.5:1");
  // One step lighter than the line is the line: #767676 is 4.54:1 and passes.
  await page.getByLabel("Text colour", { exact: true }).fill("#767676");
  await expect(page.getByText(WARN)).toHaveCount(0);
});

test("a_newer_kit_arriving_under_the_open_form_replaces_it_when_nothing_was_typed", async ({
  page,
  app,
}) => {
  // Only `resync` itself was pinned before this: nothing said the FORM is replaced when a newer
  // kit lands under it, so deleting the call left every test green.
  const kits = await open(page, app);
  await expect(page.getByLabel("Font stack", { exact: true })).toHaveValue("");
  kits.set({
    logo_url: "https://a.example/l.png",
    palette: { accent: "#1a56db" },
    font_stack: "Georgia, serif",
    footer: {},
  });
  await refocus(page);
  await expect(page.getByLabel("Font stack", { exact: true })).toHaveValue("Georgia, serif");
  await expect(page.getByLabel("Accent colour", { exact: true })).toHaveValue("#1a56db");
});

test("a_save_merges_onto_the_latest_kit_so_a_key_added_under_the_form_survives", async ({
  page,
  app,
}) => {
  // The kit gains a key this screen cannot show AFTER the form was mounted. A save that merged
  // onto the kit as it was at mount would drop it, and nothing on screen would say so.
  const kits = await open(page, app);
  kits.set({
    logo_url: null,
    palette: { accent: "#1a56db", sparkle: "aurora" },
    font_stack: null,
    footer: { seasonal_note: "winter" },
  });
  await refocus(page);
  await expect(page.getByLabel("Accent colour", { exact: true })).toHaveValue("#1a56db");

  await page.getByLabel("Text colour", { exact: true }).fill("#111111");
  await page.getByRole("button", { name: "Save brand kit" }).click();
  await expect(page.getByRole("status").filter({ hasText: "Brand kit saved." })).toBeVisible();

  const saved = kits.read();
  expect(saved.palette).toEqual({ accent: "#1a56db", sparkle: "aurora", text: "#111111" });
  expect(saved.footer).toEqual({ seasonal_note: "winter" });
});
