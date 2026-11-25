// SPDX-License-Identifier: AGPL-3.0-only

/**
 * THE swap point for the brand-kit screen, and the only file task W6 changes. Everything the screen
 * knows about the server is the `BrandKit` type and the one `brandKitSource` export at the bottom.
 * When `GET`/`PUT` of the brand kit exist (task R7), W6 replaces this file's contents with a source
 * that calls the generated client and returns its `{ data, error }` as it comes, and deletes the
 * mock; nothing else changes. Calls made here cover the paths, so none is named anywhere else.
 *
 * The shape is the database's (migration 0032): one row per workspace, `palette` and `footer` JSON
 * objects (never null, never an array), `logo_url` and `font_stack` nullable text. The keys inside
 * `palette` and `footer` are this lane's guess (see `brand-kit-model.ts`); the server's renderer
 * decides the real ones.
 *
 * `get` answers with an empty kit for a workspace that has no row, so the screen never has to tell
 * "no kit yet" from "an empty one": W6 maps the server's answer for that case here.
 */

import { LOGO_MESSAGE, logoAllowed } from "./brand-kit-model";

export type BrandKit = {
  logo_url: string | null;
  // `unknown` values: a newer server may enrich a key this screen knows, or add its own.
  palette: Record<string, unknown>;
  font_stack: string | null;
  footer: Record<string, unknown>;
};

/** The `{ data, error }` an `openapi-fetch` call resolves to, so the swap changes no call site. */
export type Result = { data?: BrandKit; error?: unknown };

export type BrandKitSource = {
  get: (workspace: string) => Promise<Result>;
  put: (workspace: string, kit: BrandKit) => Promise<Result>;
};

// ---- Mock. Not the product: a stand-in with the same shape, deleted when the endpoints land. ----

const kits = new Map<string, BrandKit>();
const EMPTY: BrandKit = { logo_url: null, palette: {}, font_stack: null, footer: {} };

/** An RFC 9457 problem as the API writes one, so `problemMessage` reads it as it will the real one. */
const refuse = (message: string, field: string): Result => ({
  error: {
    type: "urn:nm:problem:validation",
    title: "Validation failed",
    status: 422,
    detail: "validation",
    errors: [{ field, message }],
  },
});

const mock: BrandKitSource = {
  get: async (workspace) => ({ data: structuredClone(kits.get(workspace) ?? EMPTY) }),
  put: async (workspace, kit) => {
    // The server is the gate for the scheme allowlist; the mock stands in for that gate.
    if (kit.logo_url !== null && !logoAllowed(kit.logo_url)) {
      return refuse(LOGO_MESSAGE, "logo_url");
    }
    kits.set(workspace, structuredClone(kit));
    return { data: structuredClone(kit) };
  },
};

/** W6: replace this export with the real source. */
export const brandKitSource: BrandKitSource = mock;
