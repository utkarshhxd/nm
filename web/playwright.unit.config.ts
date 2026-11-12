// SPDX-License-Identifier: AGPL-3.0-only

import { defineConfig, devices } from "@playwright/test";

// Component tests: a real browser, but no database and no `nm` server, so no globalSetup.
// Each test file asks for the `app` fixture in e2e/support/app.ts, which builds what it needs.
export default defineConfig({
  testDir: "src",
  workers: 1,
  fullyParallel: false,
  forbidOnly: !!process.env["CI"],
  reporter: "list",
  use: {
    trace: "off",
    video: "off",
    screenshot: "off",
    // The dev container runs as root and has a small /dev/shm.
    launchOptions: { args: ["--no-sandbox", "--disable-dev-shm-usage"] },
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
