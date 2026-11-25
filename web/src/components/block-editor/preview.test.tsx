// SPDX-License-Identifier: AGPL-3.0-only

// The preview and the findings, in a real browser. What matters here is that the preview does what
// its labels say and says what it cannot do: spec 4.1, "a preview that lies".

import AxeBuilder from "@axe-core/playwright";
import { type Page, expect } from "@playwright/test";

import { EDITOR, test } from "../../../e2e/support/app";
import { expectNoViolations } from "../../../e2e/support/axe";

const FRAME = "Email preview";
const mode = (page: Page, name: string) => page.getByRole("button", { name, exact: true }).click();
const frame = (page: Page) => page.frameLocator(`iframe[title="${FRAME}"]`);
const open = async (page: Page, app: string) => {
  await page.goto(app + EDITOR);
  await expect(page.getByRole("textbox", { name: "Text block 2", exact: true })).toBeVisible();
  await expect(page.locator(`iframe[title="${FRAME}"]`)).toBeVisible();
};
const background = (page: Page) => frame(page).locator("body").evaluate((b) => getComputedStyle(b).backgroundColor);
// "rgb(r, g, b)" to its brightest channel: 0 is black, 255 is white.
const brightness = (rgb: string) => Math.max(...(rgb.match(/\d+/g) ?? ["0"]).slice(0, 3).map(Number));

test("all_four_modes_render_and_desktop_mobile_differ_in_width", async ({ page, app }) => {
  await open(page, app);
  const iframe = page.locator(`iframe[title="${FRAME}"]`);
  await mode(page, "Desktop");
  const wide = (await iframe.boundingBox())?.width ?? 0;
  await expect(frame(page).getByRole("heading", { name: "Welcome" })).toBeVisible();
  await mode(page, "Mobile");
  const narrow = (await iframe.boundingBox())?.width ?? 0;
  expect(narrow).toBeGreaterThan(300);
  expect(narrow).toBeLessThanOrEqual(380);
  expect(wide).toBeGreaterThan(narrow + 150);
  await mode(page, "Dark");
  await expect(frame(page).getByRole("heading", { name: "Welcome" })).toBeVisible();
  await mode(page, "Plain text");
  await expect(iframe).toHaveCount(0);
  const text = page.getByRole("region", { name: "Plain text version" });
  await expect(text).toContainText("Welcome");
  await expect(text).toContainText("Read more: https://example.com");
});

test("dark_mode_shows_the_authors_dark_styles_and_light_modes_never_do", async ({ page, app }) => {
  await open(page, app);
  await mode(page, "Desktop");
  expect(brightness(await background(page))).toBeGreaterThan(200);
  await mode(page, "Dark");
  expect(brightness(await background(page))).toBeLessThan(60);
  await mode(page, "Mobile");
  expect(brightness(await background(page))).toBeGreaterThan(200);
});

test("the_dark_preview_says_next_to_itself_what_it_covers_and_what_it_cannot", async ({ page, app }) => {
  await open(page, app);
  const iframe = page.locator(`iframe[title="${FRAME}"]`);
  await mode(page, "Dark");
  const label = page.getByTestId("dark-coverage");
  await expect(label).toBeVisible();
  // Read alone, it names what is shown and what is not.
  await expect(label).toContainText("dark styles you wrote");
  await expect(label).toContainText("Gmail");
  await expect(label).toContainText("Outlook");
  await expect(label).toContainText("cannot show");
  // Next to the preview, not elsewhere: directly above it, and the frame is described by it.
  const [l, f] = [await label.boundingBox(), await iframe.boundingBox()];
  expect(l && f && l.y + l.height <= f.y && f.y - (l.y + l.height) < 40).toBe(true);
  expect(await iframe.getAttribute("aria-describedby")).toBe(await label.getAttribute("id"));
  // It is not a tooltip or a closed disclosure.
  expect(await label.evaluate((e) => e.closest("details,[role=tooltip]"))).toBeNull();
  expect(await label.getAttribute("title")).toBeNull();
  // And it only appears for dark: the other modes make no dark claim.
  await mode(page, "Desktop");
  await expect(page.getByTestId("dark-coverage")).toHaveCount(0);
});

test("every_mode_says_the_preview_is_the_browser_not_an_email_client", async ({ page, app }) => {
  await open(page, app);
  for (const m of ["Desktop", "Mobile", "Dark", "Plain text"]) {
    await mode(page, m);
    await expect(page.getByTestId("preview-scope")).toContainText("your browser");
  }
});

test("the_preview_frame_cannot_run_scripts_from_the_email", async ({ page, app }) => {
  await open(page, app);
  const sandbox = (await page.locator(`iframe[title="${FRAME}"]`).getAttribute("sandbox")) ?? "missing";
  expect(sandbox).not.toBe("missing");
  expect(sandbox).not.toContain("allow-scripts");
});

test("a_button_says_in_words_that_classic_outlook_on_windows_draws_it_square", async ({ page, app }) => {
  await open(page, app);
  const note = page.getByTestId("outlook-button-note");
  await expect(note).toHaveCount(0);
  await page.getByRole("textbox", { name: "Button block 3.2.1 label" }).click();
  await expect(note).toBeVisible();
  await expect(note).toContainText("classic Outlook on Windows");
  await expect(note).toContainText("square");
  await expect(note).toContainText("preview cannot show");
  // Only where a Button is being edited.
  await page.getByRole("button", { name: "Select Heading block 1" }).click();
  await expect(note).toHaveCount(0);
});

test("findings_appear_as_the_document_changes_and_go_when_fixed_with_no_save", async ({ page, app }) => {
  await open(page, app);
  const findings = page.getByRole("list", { name: "Findings" });
  await expect(page.getByTestId("findings-count")).toHaveText("No findings.");
  await page.getByRole("button", { name: "Add image", exact: true }).click();
  await expect(findings).toContainText("Image has no description");
  await expect(findings).toContainText("Image has no address");
  await expect(page.getByTestId("findings-count")).toHaveText("2 findings.");
  await page.getByLabel("Description").fill("A lighthouse");
  await expect(findings).not.toContainText("Image has no description");
  await page.getByLabel("Image address").fill("https://example.com/i.png");
  await expect(page.getByTestId("findings-count")).toHaveText("No findings.");
  // A link the email client would refuse.
  await page.getByRole("textbox", { name: "Button block 3.2.1 label" }).click();
  await page.getByLabel("Link address").fill("javascript:alert(1)");
  await expect(findings).toContainText("Button link starts with javascript");
  await expect(findings.getByText("link check")).toBeVisible();
});

test("a_finding_takes_you_to_its_block", async ({ page, app }) => {
  await open(page, app);
  await page.getByRole("button", { name: "Add image", exact: true }).click();
  await page.getByRole("button", { name: "Select Heading block 1" }).click();
  await expect(page.getByRole("group", { name: "Heading properties" })).toBeVisible();
  await page.getByRole("button", { name: "Show block 4" }).first().click();
  await expect(page.getByRole("group", { name: "Image properties" })).toBeVisible();
});

test("a_failed_check_is_reported_and_never_shown_as_no_findings", async ({ page, app }) => {
  await page.goto(app + EDITOR + "?preview=fail");
  await expect(page.getByRole("alert").filter({ hasText: "Preview and checks unavailable" })).toBeVisible();
  await expect(page.getByText("No findings")).toHaveCount(0);
  await expect(page.locator(`iframe[title="${FRAME}"]`)).toHaveCount(0);
});

  // axe cannot enter the sandboxed frame (it reports `frame-tested`, "could not decide"), so the frame
// is scanned as what it is, a standalone document: the exact srcdoc the preview is showing.
for (const m of ["Desktop", "Dark", "Plain text"]) {
  test(`the_preview_${m.replace(" ", "_").toLowerCase()}_has_no_violations_and_nothing_undecided`, async ({ page, app }) => {
    await open(page, app);
    await page.getByRole("button", { name: "Add image", exact: true }).click(); // gives it a finding to render
    await mode(page, m);
    await expect(page.getByRole("region", { name: "Preview and checks" })).toBeVisible();
    await expect(page.getByTestId("findings-count")).toHaveText("2 findings.");
    // Landmarks and a region are rules about a web page. An email is table markup with no <main>,
    // so those two are off for the email document only; contrast and everything else stay on.
    const scan = (p: Page, include?: string) => {
      const b = new AxeBuilder({ page: p });
      return include ? b.include(include).exclude("iframe") : b.disableRules(["landmark-one-main", "region"]);
    };
    // "Could not decide" is not a pass: for the two rules a dark preview is about, nothing may be
    // undecided, and contrast must have actually passed somewhere.
    const undecided = async (p: Page, include?: string) => {
      const { incomplete, passes } = await scan(p, include).analyze();
      expect(passes.some((r) => r.id === "color-contrast")).toBe(true);
      return incomplete.filter((r) => r.id === "color-contrast" || r.id === "label-content-name-mismatch").map((r) => `${r.id}: ${r.nodes.map((n) => n.target.join(" ")).join(", ")}`);
    };
    await expectNoViolations(scan(page, '[aria-label="Preview and checks"]'), `preview chrome, ${m}`);
    expect(await undecided(page, '[aria-label="Preview and checks"]')).toEqual([]);
    if (m === "Plain text") return;
    const srcdoc = await page.locator(`iframe[title="${FRAME}"]`).getAttribute("srcdoc");
    const doc = await page.context().newPage();
    await doc.setContent(srcdoc ?? "");
    await expectNoViolations(scan(doc), `email document, ${m}`, { requireRules: ["color-contrast"] });
    expect(await undecided(doc)).toEqual([]);
    await doc.close();
  });
}
