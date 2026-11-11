// SPDX-License-Identifier: AGPL-3.0-only

// Fails if any production dependency ships under a license outside the Global Constraints
// list. Anything else needs a one-line justification added here first.

import { execFileSync } from "node:child_process";
import { pathToFileURL } from "node:url";

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

const names = (packages) => packages.map((pkg) => pkg.name).join(", ");

/**
 * The licenses in `byLicense` that this project may not ship, as printable lines.
 * Empty means the report is clean.
 *
 * pnpm groups packages by license string: { "MIT": [{ name, versions, ... }, ...], ... }.
 */
export function problems(byLicense) {
  const entries = Object.entries(byLicense);
  // Fail closed. An empty report means pnpm found nothing to inspect — no node_modules, or a
  // flag that stopped meaning what it meant — and a supply-chain gate that passes having read
  // nothing is worse than no gate at all, because CI goes green on it.
  if (entries.length === 0) {
    return ["pnpm reported no production dependencies; refusing to pass on an empty report"];
  }
  return entries
    .filter(([license]) => !ALLOWED.has(license))
    .map(([license, packages]) => `DENY ${license}: ${names(packages)}`);
}

// Only when run as a program: `node --test` imports this file for its unit tests.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const byLicense = JSON.parse(
    execFileSync("pnpm", ["licenses", "list", "--prod", "--json"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "inherit"],
    }),
  );

  for (const [license, packages] of Object.entries(byLicense)) {
    if (ALLOWED.has(license)) console.log(`ok   ${license}: ${names(packages)}`);
  }

  const found = problems(byLicense);
  if (found.length > 0) {
    for (const line of found) console.error(line);
    console.error("licenses outside global-constraints.md; see web/scripts/check-licenses.mjs");
    process.exit(1);
  }
}
