// SPDX-License-Identifier: AGPL-3.0-only

// Writes .br and .gz copies of everything compressible in dist. The Rust server picks the
// variant the client accepts, so it never compresses a response at request time.

import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { extname, join } from "node:path";
import { brotliCompressSync, constants, gzipSync } from "node:zlib";

const DIST = "dist";
const COMPRESSIBLE = new Set([".js", ".css", ".html", ".svg", ".json"]);

function* walk(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(path);
    else yield path;
  }
}

let count = 0;
for (const path of walk(DIST)) {
  // `app.js.br` has extension `.br`, so a second run does not recompress its own output.
  if (!COMPRESSIBLE.has(extname(path))) continue;
  const raw = readFileSync(path);
  writeFileSync(
    `${path}.br`,
    brotliCompressSync(raw, {
      params: {
        [constants.BROTLI_PARAM_QUALITY]: 11,
        [constants.BROTLI_PARAM_SIZE_HINT]: raw.length,
      },
    }),
  );
  writeFileSync(`${path}.gz`, gzipSync(raw, { level: 9 }));
  count += 1;
}

console.log(`precompressed ${count} files in ${DIST}`);
