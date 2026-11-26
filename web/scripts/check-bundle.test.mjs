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
    // `uses` and `dyn` map a chunk to the chunks it imports statically and dynamically.
    const edges = (name) => ({
      imports: [...(name in files.lazy ? ["index.html"] : []), ...(files.uses?.[name] ?? [])],
      dynamicImports: files.dyn?.[name] ?? [],
    });
    for (const [name, size] of Object.entries(files.lazy)) {
      manifest[name] = { file: `assets/${name}.js`, isDynamicEntry: true, ...edges(name) };
      write(cwd, `assets/${name}.js`, size);
    }
    // Chunks only routes import (a manualChunks split): in the manifest, but not entries.
    for (const [name, size] of Object.entries(files.split ?? {})) {
      manifest[name] = { file: `assets/${name}.js`, ...edges(name) };
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

test("fails_when_a_route_split_into_two_chunks_exceeds_50kb_between_them", () => {
  const r = run({
    shell: 100 * KB,
    lazy: { editor: 30 * KB },
    split: { "editor-core": 30 * KB },
    uses: { editor: ["editor-core"] },
  });
  assert.notEqual(r.status, 0, r.out);
  assert.match(r.out, /editor\.js and what it loads is over the lazy-chunk budget/);
});

test("passes_when_two_routes_share_a_chunk_and_each_closure_fits", () => {
  const r = run({
    shell: 100 * KB,
    lazy: { contacts: 20 * KB, tags: 20 * KB },
    split: { shared: 25 * KB },
    uses: { contacts: ["shared"], tags: ["shared"] },
  });
  assert.equal(r.status, 0, r.out);
});

test("fails_when_a_framework_split_chains_route_to_a_to_b_and_the_route_imports_only_a", () => {
  const r = run({
    shell: 100 * KB,
    lazy: { editor: 1 * KB },
    split: { a: 30 * KB, b: 30 * KB },
    uses: { editor: ["a"], a: ["b"] },
  });
  assert.notEqual(r.status, 0, r.out);
  assert.match(r.out, /editor\.js and what it loads is over the lazy-chunk budget/);
});

test("counts_a_diamond_dependency_once", () => {
  // Four 12 KB chunks are 48 KB, under budget; counting c once per path (five chunks) is 60 KB.
  const r = run({
    shell: 100 * KB,
    lazy: { route: 12 * KB },
    split: { a: 12 * KB, b: 12 * KB, c: 12 * KB },
    uses: { route: ["a", "b"], a: ["c"], b: ["c"] },
  });
  assert.equal(r.status, 0, r.out);
});

test("fails_when_a_route_dynamically_imports_several_chunks_that_add_up_past_the_budget", () => {
  const r = run({
    shell: 100 * KB,
    lazy: { editor: 1 * KB },
    split: { core: 30 * KB, blocks: 30 * KB },
    dyn: { editor: ["core", "blocks"] },
  });
  assert.notEqual(r.status, 0, r.out);
  assert.match(r.out, /editor\.js and what it loads is over the lazy-chunk budget/);
});
