// SPDX-License-Identifier: AGPL-3.0-only

// Fails if any production dependency ships under a license outside the Global Constraints
// list. Anything else needs a one-line justification added here first.

import { execFileSync } from "node:child_process";

const ALLOWED = new Set([
  "MIT",
  "Apache-2.0",
  "BSD-2-Clause",
  "BSD-3-Clause",
  "ISC",
  "Zlib",
  "MPL-2.0",
  "Unicode-3.0",
  "GPL-3.0-only",
  "GPL-3.0-or-later",
  "AGPL-3.0-only",
  "AGPL-3.0-or-later",
  // Unlicense: OSI-approved public-domain dedication, for `isbot` (via @tanstack/react-router).
  "Unlicense",
]);

const output = execFileSync("pnpm", ["licenses", "list", "--prod", "--json"], {
  encoding: "utf8",
  stdio: ["ignore", "pipe", "inherit"],
});

// pnpm groups packages by license string: { "MIT": [{ name, versions, ... }, ...], ... }.
const byLicense = JSON.parse(output);
let failed = false;
for (const [license, packages] of Object.entries(byLicense)) {
  const names = packages.map((pkg) => pkg.name).join(", ");
  if (ALLOWED.has(license)) {
    console.log(`ok   ${license}: ${names}`);
  } else {
    console.error(`DENY ${license}: ${names}`);
    failed = true;
  }
}

if (failed) {
  console.error("licenses outside global-constraints.md; see web/scripts/check-licenses.mjs");
  process.exit(1);
}
