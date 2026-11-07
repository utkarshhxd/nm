// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import { type Page, expect, test } from "@playwright/test";
import { Secret, TOTP } from "otpauth";

import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

const PW = ADMIN_PASSWORD;

/**
 * The 6-digit code for `secret` at a point in time. The server's skew is one step either side,
 * so the code for `now - 30s` is a valid *earlier* step: enrolling with it and then signing in
 * with the code for `now` proves both that the previous window is accepted and that a spent
 * step does not block the next one.
 */
function codeAt(secret: string, atMillis: number): string {
  const totp = new TOTP({
    algorithm: "SHA1",
    digits: 6,
    period: 30,
    secret: Secret.fromBase32(secret),
  });
  return totp.generate({ timestamp: atMillis });
}

/**
 * `exact: true` on every `getByRole` name below: Playwright matches an accessible name by
 * case-insensitive substring by default, and the sign-in screen carries both "Sign in" and
 * "Sign in with a passkey".
 */
async function signInWithPassword(page: Page): Promise<void> {
  await page.goto("/login");
  await page.getByLabel("Email").fill(ADMIN_EMAIL);
  await page.getByLabel("Password").fill(PW);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  // Signing in is a POST and then a full page load. Waiting for the app here is what stops the
  // next `goto` from racing it and landing back on this screen with no session.
  await expect(page.getByText(ADMIN_EMAIL)).toBeVisible();
}

test("password sign-in reaches the app", async ({ page }) => {
  await page.goto("/login");
  await page.getByLabel("Email").fill("admin@example.com");
  await page.getByLabel("Password").fill(PW);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(page.getByText("admin@example.com")).toBeVisible();
});

test("a wrong password is shown on the form", async ({ page }) => {
  await page.goto("/login");
  await page.getByLabel("Email").fill(ADMIN_EMAIL);
  await page.getByLabel("Password").fill("not-the-password");
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  await expect(page.getByRole("alert")).toHaveText("Email or password is incorrect");
  await expect(page).toHaveURL(/\/login$/);
});

test("TOTP enrolment, then sign-in needs a code", async ({ page }) => {
  await signInWithPassword(page);
  await page.goto("/account/security");

  await page.getByRole("button", { name: "Set up", exact: true }).click();
  const secret = (await page.getByTestId("totp-secret").innerText()).trim();
  await page.getByLabel("Six-digit code").fill(codeAt(secret, Date.now() - 30_000));
  await page.getByRole("button", { name: "Confirm", exact: true }).click();

  await expect(page.getByTestId("recovery-code")).toHaveCount(10);
  await page.getByRole("button", { name: "I've saved these", exact: true }).click();
  await expect(page.getByTestId("recovery-code")).toHaveCount(0);

  await page.goto("/");
  await page.getByRole("button", { name: "Sign out", exact: true }).click();
  await expect(page).toHaveURL(/\/login$/);

  await page.getByLabel("Email").fill(ADMIN_EMAIL);
  await page.getByLabel("Password").fill(PW);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  const code = page.getByLabel("Six-digit code");
  await expect(code).toBeVisible();
  await code.fill(codeAt(secret, Date.now()));
  await page.getByRole("button", { name: "Verify", exact: true }).click();
  await expect(page.getByText(ADMIN_EMAIL)).toBeVisible();

  // Puts the shared account back as the other tests expect it, and covers the password prompt
  // the brief requires on the way out.
  await page.goto("/account/security");
  await page.getByRole("button", { name: "Turn off", exact: true }).click();
  await page.getByLabel("Password", { exact: true }).fill(PW);
  await page.getByRole("button", { name: "Disable two-factor sign-in", exact: true }).click();
  await expect(page.getByRole("button", { name: "Set up", exact: true })).toBeVisible();
});

test("a newcomer accepts an invitation and lands in the app", async ({ page, request }) => {
  // The invite is set up over the API: the screen under test is the invitee's, and Task 16 owns
  // the admin screens that would otherwise create it.
  await request.post("/api/v1/auth/login", { data: { email: ADMIN_EMAIL, password: PW } });
  const me = await (await request.get("/api/v1/me")).json();
  const csrf = { "x-csrf-token": me.csrf_token as string };

  const workspace = await (
    await request.post("/api/v1/workspaces", {
      headers: csrf,
      data: { name: "Acme", slug: "acme" },
    })
  ).json();
  const invite = await (
    await request.post(`/api/v1/workspaces/${workspace.id}/invites`, {
      headers: csrf,
      data: { email: "newcomer@example.com", role: "editor" },
    })
  ).json();

  await page.goto(new URL(invite.invite_url as string).pathname);
  await expect(page.getByText("Acme")).toBeVisible();
  await page.getByLabel("Your name").fill("New Comer");
  await page.getByLabel("Password").fill("newcomer-password");
  await page.getByRole("button", { name: "Accept", exact: true }).click();
  // The whole sentence: the invited address is on the invitation screen too, so matching the
  // address alone would pass without ever leaving it.
  await expect(page.getByText("Signed in as newcomer@example.com")).toBeVisible();
});

test("auth screens have no serious accessibility violations", async ({ page }) => {
  await signInWithPassword(page);

  // `ready` is named, not just "some heading": a security page that quietly bounced to /login
  // would otherwise have this test scanning the sign-in screen twice and passing. It is also
  // the last thing each screen renders, so nothing is scanned while still loading.
  const screens = [
    { path: "/login", ready: "Sign in" },
    { path: "/account/security", ready: "Passkeys" },
  ];

  for (const { path, ready } of screens) {
    await page.goto(path);
    await expect(page.getByRole("heading", { name: ready, exact: true })).toBeVisible();
    const { violations } = await new AxeBuilder({ page }).analyze();
    const serious = violations
      .filter((v) => v.impact === "serious" || v.impact === "critical")
      .map((v) => `${v.id} (${v.impact}): ${v.nodes.map((n) => n.target.join(" ")).join(", ")}`);
    expect(serious, `accessibility violations on ${path}`).toEqual([]);
  }
});
