// SPDX-License-Identifier: AGPL-3.0-only

import { defineConfig, devices } from "@playwright/test";

import { BASE_URL } from "./e2e/support/server";

export default defineConfig({
  testDir: "e2e",
  globalSetup: "./e2e/support/server.ts",
  // One server, one account, and tests that change that account's second factor: serial.
  workers: 1,
  fullyParallel: false,
  forbidOnly: !!process.env["CI"],
  reporter: "list",
  use: {
    baseURL: BASE_URL,
    // A TOTP secret and ten recovery codes cross these screens. A trace, a video or a failure
    // screenshot would write them to disk, so none of the three is ever recorded.
    trace: "off",
    video: "off",
    screenshot: "off",
    // The dev container runs as root and has a small /dev/shm.
    launchOptions: { args: ["--no-sandbox", "--disable-dev-shm-usage"] },
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
