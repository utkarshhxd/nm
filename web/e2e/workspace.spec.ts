// SPDX-License-Identifier: AGPL-3.0-only

import AxeBuilder from "@axe-core/playwright";
import { type Page, expect, test } from "@playwright/test";

import { ADMIN_EMAIL, ADMIN_PASSWORD } from "./support/server";

const TEAMMATE = "teammate@example.com";
/** Only ever exists inside the per-run database that global teardown drops. */
const TEAMMATE_PASSWORD = "teammate-password";

/**
 * Every test here makes its own workspace, with a slug no other spec uses. The suite shares one
 * server and one instance admin, and `auth.spec.ts` already creates `acme` for that admin, so a
 * test that assumed "the admin has no workspace" — or reused a slug — would pass or fail
 * depending on which file ran first.
 */

/** `exact: true` on every name: Playwright matches an accessible name by substring by default. */
async function signIn(page: Page, email: string, password: string): Promise<void> {
  await page.goto("/login");
  await page.getByLabel("Email", { exact: true }).fill(email);
  await page.getByLabel("Password", { exact: true }).fill(password);
  await page.getByRole("button", { name: "Sign in", exact: true }).click();
  // Signing in is a POST and then a full page load; waiting here stops the next `goto` racing it.
  await expect(page.getByText(`Signed in as ${email}`)).toBeVisible();
}

async function createWorkspace(page: Page, slug: string, name: string): Promise<void> {
  await page.goto("/workspaces/new");
  await page.getByLabel("Slug", { exact: true }).fill(slug);
  await page.getByLabel("Workspace name", { exact: true }).fill(name);
  await page.getByLabel("Time zone", { exact: true }).selectOption("Europe/London");
  await page.getByRole("button", { name: "Create workspace", exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/w/${slug}/members$`));
}

test("admin creates a workspace and a teammate joins by invite as editor", async ({ browser }) => {
  const adminContext = await browser.newContext();
  const guestContext = await browser.newContext();
  const admin = await adminContext.newPage();
  const guest = await guestContext.newPage();

  try {
    await signIn(admin, ADMIN_EMAIL, ADMIN_PASSWORD);
    await createWorkspace(admin, "globex", "Globex");
    await expect(admin.getByRole("heading", { name: "Members", exact: true })).toBeVisible();

    // A slug the signed-in admin is not a member of must not say whether it exists elsewhere.
    await admin.goto("/w/no-such-workspace/members");
    await expect(admin.getByText("Workspace not found")).toBeVisible();
    await admin.goto("/w/globex/members");

    await admin.getByLabel("Email", { exact: true }).fill(TEAMMATE);
    await admin.getByLabel("Role", { exact: true }).selectOption("editor");
    await admin.getByRole("button", { name: "Send invite", exact: true }).click();
    const link = (await admin.getByTestId("invite-url").innerText()).trim();
    await expect(
      admin.getByRole("listitem").filter({ hasText: TEAMMATE }).getByRole("button", {
        name: `Revoke invitation for ${TEAMMATE}`,
        exact: true,
      }),
    ).toBeVisible();

    await guest.goto(new URL(link).pathname);
    await guest.getByLabel("Your name", { exact: true }).fill("Tea Mate");
    await guest.getByLabel("Password", { exact: true }).fill(TEAMMATE_PASSWORD);
    await guest.getByRole("button", { name: "Accept", exact: true }).click();
    // The whole sentence: the invited address is on the invitation screen too.
    await expect(guest.getByText(`Signed in as ${TEAMMATE}`)).toBeVisible();
    await expect(guest).toHaveURL(/\/w\/globex\/members$/);

    const guestRow = guest.getByRole("row").filter({ hasText: TEAMMATE });
    await expect(guestRow.getByRole("combobox")).toHaveValue("editor");
    // `canAssign` is false for an editor, so the control is disabled. The server is still the
    // one that refuses the change; this only checks that the UI does not invite the attempt.
    await expect(guestRow.getByRole("combobox")).toBeDisabled();
    // An editor gets neither of the admin-only screens in the navigation, and not the invite
    // card either: both of its endpoints are `Role::Admin`, so it could only 403 on load.
    await expect(guest.getByRole("link", { name: "API keys", exact: true })).toHaveCount(0);
    await expect(guest.getByRole("link", { name: "Audit log", exact: true })).toHaveCount(0);
    await expect(guest.getByRole("heading", { name: "Invite someone", exact: true })).toHaveCount(
      0,
    );

    await admin.reload();
    const adminRow = admin.getByRole("row").filter({ hasText: TEAMMATE });
    await adminRow.getByRole("combobox").selectOption("admin");
    await expect(adminRow.getByRole("combobox")).toHaveValue("admin");

    await admin.goto("/w/globex/audit-log");
    await expect(admin.getByRole("heading", { name: "Audit log", exact: true })).toBeVisible();
    // The actor column resolves the user id to an address; that is the whole point of the
    // join added to `GET /workspaces/{id}/audit-log` for this screen.
    const accepted = admin.getByRole("row").filter({ hasText: "invite.accepted" });
    await expect(accepted).toContainText(TEAMMATE);

    // The sole owner cannot leave. The message is the brief's, verbatim.
    await admin.goto("/w/globex/members");
    await admin.getByRole("button", { name: `Leave ${ADMIN_EMAIL}`, exact: true }).click();
    const confirm = admin.getByRole("button", { name: `Confirm leave ${ADMIN_EMAIL}`, exact: true });
    // Arming replaces the button that was pressed, so the confirmation has to take the focus
    // with it or a keyboard user is dropped on <body> half way through.
    await expect(confirm).toBeFocused();
    await confirm.click();
    await expect(admin.getByRole("alert")).toHaveText(
      "A workspace needs at least one owner. Make someone else an owner first.",
    );
    await expect(admin.getByRole("row").filter({ hasText: ADMIN_EMAIL })).toBeVisible();
  } finally {
    await adminContext.close();
    await guestContext.close();
  }
});

test("an API key is shown once and stops working when revoked", async ({ page, request }) => {
  await signIn(page, ADMIN_EMAIL, ADMIN_PASSWORD);
  await createWorkspace(page, "keyco", "Key Co");

  await page.getByRole("link", { name: "API keys", exact: true }).click();
  await expect(page.getByRole("heading", { name: "API keys", exact: true })).toBeVisible();
  await page.getByLabel("Name", { exact: true }).fill("Deploy bot");
  await page.getByLabel("Role", { exact: true }).selectOption("editor");
  await page.getByRole("button", { name: "Create key", exact: true }).click();

  const key = (await page.getByTestId("api-key-value").innerText()).trim();
  // Asserted as a boolean, never as a matcher on `key`: a failing matcher prints the subject,
  // and this value must not reach a Playwright report.
  expect(key.startsWith("nm_"), "the created key starts with nm_").toBe(true);
  const prefix = key.split("_")[1] ?? "";

  await page.getByRole("button", { name: "I've stored it", exact: true }).click();
  await expect(page.getByTestId("api-key-value")).toHaveCount(0);
  await expect(page.getByText(`nm_${prefix}`, { exact: true })).toBeVisible();
  // The whole key is gone from the page, not merely hidden — asserted on a boolean, because a
  // locator built from `key` puts the secret in the failure message and so in the report.
  expect((await page.content()).includes(key), "the key is gone from the page").toBe(false);

  // `request` has never signed in, so it holds no cookie: everything below is the key's own
  // authority. A key may read its own workspace and nothing else (T12), which is also how the
  // id is obtained here without a session.
  const auth = { headers: { Authorization: `Bearer ${key}` } };
  const mine = await request.get("/api/v1/workspaces", auth);
  expect(mine.status()).toBe(200);
  const id = ((await mine.json()) as { data: { id: string }[] }).data[0]?.id ?? "";
  expect(await (await request.get(`/api/v1/workspaces/${id}`, auth)).status()).toBe(200);

  await page.getByRole("button", { name: "Revoke Deploy bot", exact: true }).click();
  await page.getByRole("button", { name: "Confirm revoke Deploy bot", exact: true }).click();
  await expect(page.getByText(`nm_${prefix}`, { exact: true })).toHaveCount(0);

  expect(await (await request.get(`/api/v1/workspaces/${id}`, auth)).status()).toBe(401);
});

test("workspace screens have no serious accessibility violations", async ({ page }) => {
  await signIn(page, ADMIN_EMAIL, ADMIN_PASSWORD);
  await createWorkspace(page, "axeco", "Axe Co");

  // `ready` is named, not just "some heading": a screen that quietly bounced elsewhere would
  // otherwise be scanned twice under another screen's name and pass.
  const screens = [
    { path: "/w/axeco/members", ready: "Members" },
    { path: "/w/axeco/api-keys", ready: "API keys" },
    { path: "/w/axeco/audit-log", ready: "Audit log" },
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

test("a list that fails to load says so", async ({ page }) => {
  await signIn(page, ADMIN_EMAIL, ADMIN_PASSWORD);
  await createWorkspace(page, "failco", "Fail Co");

  // Deterministic, not a timing trick: the response is replaced before the page asks for it.
  // Without the `isError` branch this screen renders a card holding nothing but its heading,
  // which reads exactly like a workspace with no members.
  await page.route("**/api/v1/workspaces/*/members*", (route) =>
    route.fulfill({
      status: 500,
      contentType: "application/problem+json",
      body: JSON.stringify({ type: "urn:nm:problem:internal", detail: "internal error" }),
    }),
  );
  await page.reload();
  await expect(
    page.getByText("The members could not be loaded. Reload the page to try again."),
  ).toBeVisible();
});
