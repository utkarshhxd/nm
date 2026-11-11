// SPDX-License-Identifier: AGPL-3.0-only

/**
 * A worker-scoped `app` fixture: the production build of the real `index.html` plus one extra
 * page, `/e2e/support/sample/index.html`, that renders every primitive. It is a *build*, served
 * by `vite preview`, because a dev server injects CSS from JavaScript and so always flashes
 * unstyled; the pre-paint theme test would be measuring Vite, not us.
 *
 * Needs no database and no `nm` binary, so it also backs the component tests in `src/`.
 */

import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { test as base } from "@playwright/test";
import { build, preview } from "vite";

const WEB = resolve(import.meta.dirname, "../..");

/**
 * The CSP `src/http/security_headers.rs` sends, with no `script-src`, so scripts fall back to
 * `default-src 'self'`: an inline script is blocked. Served here too so a theme script that only
 * works inline fails this suite instead of failing in production. Keep it equal to the Rust one.
 */
const CSP =
  "default-src 'self'; img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; " +
  "frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

export const SAMPLE = "/e2e/support/sample/index.html";

export const test = base.extend<object, { app: string }>({
  app: [
    async ({}, use) => {
      const outDir = await mkdtemp(join(tmpdir(), "nm-ui-"));
      await build({
        root: WEB,
        logLevel: "error",
        build: {
          outDir,
          emptyOutDir: true,
          rollupOptions: {
            input: {
              main: resolve(WEB, "index.html"),
              sample: resolve(WEB, "e2e/support/sample/index.html"),
            },
          },
        },
      });
      const server = await preview({
        root: WEB,
        logLevel: "error",
        build: { outDir },
        preview: { headers: { "Content-Security-Policy": CSP } },
      });
      const url = server.resolvedUrls?.local[0];
      if (!url) throw new Error("vite preview did not report an address");
      try {
        await use(url.replace(/\/$/, ""));
      } finally {
        await server.close();
        await rm(outDir, { recursive: true, force: true });
      }
    },
    { scope: "worker", timeout: 120_000 },
  ],
});

export { expect } from "@playwright/test";
