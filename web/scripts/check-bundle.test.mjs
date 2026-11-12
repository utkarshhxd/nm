// SPDX-License-Identifier: AGPL-3.0-only

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";

const SCRIPT = resolve(import.meta.dirname, "check-bundle.mjs");

/**
 * Runs the real script in a throwaway directory holding a fake `dist/`. Random bytes do not
 * compress, so a file of N bytes is N gzipped bytes (plus a few of header) and the sizes below
 * are the sizes the script measures.
 */
function run(files) {
  const cwd = mkdtempSync(join(tmpdir(), "nm-bundle-"));
  try {
    const manifest = {
      "index.html": { file: "assets/index.js", isEntry: true, dynamicImports: Object.keys(files.lazy) },
    };
    write(cwd, "assets/index.js", files.shell);
    for (const [name, size] of Object.entries(files.lazy)) {
      manifest[name] = { file: `assets/${name}.js`, isDynamicEntry: true, imports: ["index.html"] };
      write(cwd, `assets/${name}.js`, size);
    }
    write(cwd, ".vite/manifest.json", JSON.stringify(manifest), true);
    const r = spawnSync(process.execPath, [SCRIPT], { cwd, encoding: "utf8" });
    return { status: r.status, out: r.stdout + r.stderr };
  } finally {
    rmSync(cwd, { recursive: true, force: true });
  }
}

function write(cwd, path, content, raw = false) {
  const file = join(cwd, "dist", path);
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, raw ? content : randomBytes(content));
}

const KB = 1024;

test("passes_when_the_shell_and_every_route_are_within_budget", () => {
  const r = run({ shell: 100 * KB, lazy: { contacts: 49 * KB, tags: 2 * KB } });
  assert.equal(r.status, 0, r.out);
});

test("fails_when_a_lazy_route_chunk_exceeds_50kb_gzipped", () => {
  const r = run({ shell: 100 * KB, lazy: { contacts: 51 * KB } });
  assert.notEqual(r.status, 0, r.out);
});

test("still_fails_when_the_shell_exceeds_150kb_gzipped", () => {
  const r = run({ shell: 151 * KB, lazy: { contacts: 1 * KB } });
  assert.notEqual(r.status, 0, r.out);
  assert.match(r.out, /initial JS/);
});

test("reports_which_chunk_broke_the_budget", () => {
  const r = run({ shell: 100 * KB, lazy: { contacts: 51 * KB, tags: 2 * KB } });
  const over = r.out.split("\n").filter((line) => /over|exceeds/i.test(line));
  assert.ok(over.some((line) => line.includes("contacts.js")), r.out);
  assert.ok(!over.some((line) => line.includes("tags.js")), r.out);
});
