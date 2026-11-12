// SPDX-License-Identifier: AGPL-3.0-only

// Guards two budgets. The shell (the entry chunk and the chunks it imports statically) stays under
// 150 KB compressed. Every other JS chunk — a lazily imported route, or a helper only routes
// share — stays under 50 KB compressed on its own, so the contacts route cannot quietly grow
// without bound just because the shell is not the one measuring it. CSS is a separate file.

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { gzipSync } from "node:zlib";

const LIMIT = 150 * 1024;
const CHUNK_LIMIT = 50 * 1024;
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

const gzipped = (file) => gzipSync(readFileSync(join(DIST, file)), { level: 9 }).length;

let failed = false;
let total = 0;
for (const key of chunks) {
  const file = manifest[key].file;
  const size = gzipped(file);
  console.log(`${String(size).padStart(8)}  ${file}`);
  total += size;
}

console.log(`${String(total).padStart(8)}  total gzipped initial JS (limit ${LIMIT})`);
if (total > LIMIT) {
  console.error(`initial JS is over budget by ${total - LIMIT} bytes`);
  failed = true;
}

for (const [key, { file }] of Object.entries(manifest)) {
  if (chunks.has(key) || !file.endsWith(".js")) continue;
  const size = gzipped(file);
  console.log(`${String(size).padStart(8)}  ${file} (lazy, limit ${CHUNK_LIMIT})`);
  if (size > CHUNK_LIMIT) {
    console.error(`${file} is over the per-chunk budget by ${size - CHUNK_LIMIT} bytes`);
    failed = true;
  }
}
if (failed) process.exit(1);
