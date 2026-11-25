// SPDX-License-Identifier: AGPL-3.0-only

// The brand-kit screen against its local mock (`brand-kit-source.ts`), in a real browser. The mock
// stands in for the server, including the one refusal this screen cannot make itself: a logo URL
// the server will not take. The screen flags such a URL while it is typed and still sends it.

import AxeBuilder from "@axe-core/playwright";
import { type Page, expect } from "@playwright/test";

import type { components } from "../api/schema";
import { test } from "../../e2e/support/app";
import { expectNoViolations } from "../../e2e/support/axe";
import { isColour, toKit } from "./brand-kit-model";
import type { BrandKit } from "./brand-kit-source";

type Me = components["schemas"]["MeResponse"];

const WS = "11111111-1111-4111-8111-111111111111";
const FLAG = "Use a colour written as #rgb or #rrggbb, such as #1a56db.";
const URL_FLAG = "Use an address that starts with http, https, mailto or tel.";

async function open(page: Page, app: string, role: "viewer" | "editor" | "admin" = "admin") {
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
  await page.goto(`${app}/w/acme/brand-kit`);
  await expect(page.getByRole("heading", { name: "Brand kit", level: 2 })).toBeVisible();
  await expect(page.getByLabel("Logo address")).toBeVisible();
}

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
  await expect(page.getByText(URL_FLAG)).toBeVisible();
  await page.getByRole("button", { name: "Save brand kit" }).click();
  // The flag did not stop the request: the answer below is the mock server's, not this screen's.
  const alert = page.getByRole("alert").filter({ hasText: "The logo address must start with http or https." });
  await expect(alert).toBeVisible();
  await expect(page.getByText("Brand kit saved.")).toHaveCount(0);
  // Nothing the operator typed is lost to the failure.
  await expect(page.getByLabel("Logo address")).toHaveValue("javascript:alert(1)");
  await expect(page.getByLabel("Text colour", { exact: true })).toHaveValue("#222222");
  await scan(page, "brand kit with a failed save");
});

test("a good save says so, and clears an earlier failure", async ({ page, app }) => {
  await open(page, app);
  const logo = page.getByLabel("Logo address");
  await logo.fill("ftp://x.example/l.png");
  await page.getByRole("button", { name: "Save brand kit" }).click();
  await expect(page.getByRole("alert").filter({ hasText: "must start with http" })).toBeVisible();
  await logo.fill("https://x.example/l.png");
  await expect(page.getByText(URL_FLAG)).toHaveCount(0);
  await page.getByRole("button", { name: "Save brand kit" }).click();
  await expect(page.getByRole("status").filter({ hasText: "Brand kit saved." })).toBeVisible();
  await expect(page.getByRole("alert").filter({ hasText: "must start with http" })).toHaveCount(0);
});

test("a viewer sees the kit read-only and no save button", async ({ page, app }) => {
  await open(page, app, "viewer");
  await expect(page.getByLabel("Logo address")).toBeDisabled();
  await expect(page.getByRole("button", { name: "Save brand kit" })).toHaveCount(0);
  await scan(page, "brand kit, read-only");
});
