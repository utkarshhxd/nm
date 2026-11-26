// SPDX-License-Identifier: AGPL-3.0-only

// Guards two budgets. The shell (the entry chunk and the chunks it imports statically) stays under
// 150 KB compressed. Every other JS chunk is measured together with what it loads: the chunk, every
// chunk it imports statically, and every chunk it (or those) import dynamically, each counted once
// and minus the shell. That total stays under 50 KB compressed. Measuring chunks one by one would
// let a route, or a framework, through in two pieces with the gate green.
// It over-counts on purpose: a dynamically imported chunk that a user may never load still counts,
// and a helper shared by several routes counts in full against each. It does not model load order,
// and it measures JS only; CSS is a separate file.

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

// Everything `key` pulls in, following static imports and, with `dynamic`, dynamic ones too.
// The shell's own dynamic imports (every route) are never followed.
function collect(key, { dynamic = false, skip = new Set() } = {}, into = new Set()) {
  if (into.has(key) || skip.has(key)) return into;
  into.add(key);
  const { imports = [], dynamicImports = [] } = manifest[key];
  for (const next of dynamic ? [...imports, ...dynamicImports] : imports) {
    collect(next, { dynamic, skip }, into);
  }
  return into;
}
const chunks = collect(ENTRY);

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
  const loaded = [...collect(key, { dynamic: true, skip: chunks })];
  const size = loaded.reduce((sum, k) => sum + gzipped(manifest[k].file), 0);
  console.log(`${String(size).padStart(8)}  ${file} (lazy, ${loaded.length} chunks, limit ${CHUNK_LIMIT})`);
  if (size > CHUNK_LIMIT) {
    console.error(`${file} and what it loads is over the lazy-chunk budget by ${size - CHUNK_LIMIT} bytes`);
    failed = true;
  }
}
if (failed) process.exit(1);
