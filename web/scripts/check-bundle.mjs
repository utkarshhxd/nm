// SPDX-License-Identifier: AGPL-3.0-only

// Guards the Global Constraints budget: initial JS under 150 KB compressed. Only the entry
// chunk and the chunks it imports statically count — a lazily imported route is fetched when
// the user navigates to it, and the CSS is a separate file.

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { gzipSync } from "node:zlib";

const LIMIT = 150 * 1024;
const DIST = "dist";
const ENTRY = "index.html";

const manifest = JSON.parse(readFileSync(join(DIST, ".vite", "manifest.json"), "utf8"));
if (!manifest[ENTRY]) {
  console.error(`no "${ENTRY}" entry in the Vite manifest; run pnpm build first`);
  process.exit(1);
}

const chunks = new Set();
function collect(key) {
  if (chunks.has(key)) return;
  chunks.add(key);
  for (const imported of manifest[key].imports ?? []) collect(imported);
}
collect(ENTRY);

let total = 0;
for (const key of chunks) {
  const file = manifest[key].file;
  const size = gzipSync(readFileSync(join(DIST, file)), { level: 9 }).length;
  console.log(`${String(size).padStart(8)}  ${file}`);
  total += size;
}

console.log(`${String(total).padStart(8)}  total gzipped initial JS (limit ${LIMIT})`);
if (total > LIMIT) {
  console.error(`over budget by ${total - LIMIT} bytes`);
  process.exit(1);
}
