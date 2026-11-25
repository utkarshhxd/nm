// SPDX-License-Identifier: AGPL-3.0-only

/**
 * THE brand-kit screen's one view of the server: the `BrandKit` type and the two calls below.
 * Nothing else on that screen knows an API path.
 *
 * The shape is the database's (migration 0032): one row per workspace, `palette` and `footer`
 * JSON objects (never null, never an array), `logo_url` and `font_stack` nullable text. The keys
 * inside `palette` and `footer` are this lane's guess (see `brand-kit-model.ts`) and every other
 * key is carried through a save untouched, because a kit written by a newer version must survive
 * a round trip through this screen.
 *
 * `get` answers with an empty kit for a workspace that has no row — the server's own answer, not
 * a 404 mapped here — so the screen never has to tell "no kit yet" from "an empty one".
 *
 * The PUT is last-write-wins: there is no etag, so two operators saving at once is the later one.
 */

import { api } from "../api/client";

export type BrandKit = {
  logo_url: string | null;
  // `unknown` values: a newer server may enrich a key this screen knows, or add its own.
  palette: Record<string, unknown>;
  font_stack: string | null;
  footer: Record<string, unknown>;
};

/** The `{ data, error }` an `openapi-fetch` call resolves to, as the screen reads it. */
export type Result = { data?: BrandKit; error?: unknown };

export type BrandKitSource = {
  get: (workspace: string) => Promise<Result>;
  put: (workspace: string, kit: BrandKit) => Promise<Result>;
};

/**
 * The generated type has `logo_url` and `font_stack` optional (they are `Option<String>` in Rust
 * and `utoipa` writes that as an absent key), and the screen's model wants both present. The
 * two open maps are passed through as they arrive — stripping an unknown key here is exactly the
 * round-trip loss the kit is built to avoid.
 */
const kit = (data: {
  logo_url?: string | null;
  palette: Record<string, unknown>;
  font_stack?: string | null;
  footer: Record<string, unknown>;
}): BrandKit => ({
  logo_url: data.logo_url ?? null,
  palette: data.palette,
  font_stack: data.font_stack ?? null,
  footer: data.footer,
});

export const brandKitSource: BrandKitSource = {
  get: async (workspace) => {
    const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/brand-kit", {
      params: { path: { workspace_id: workspace } },
    });
    return data ? { data: kit(data) } : { error };
  },
  put: async (workspace, body) => {
    const { data, error } = await api.PUT("/api/v1/workspaces/{workspace_id}/brand-kit", {
      params: { path: { workspace_id: workspace } },
      body,
    });
    return data ? { data: kit(data) } : { error };
  },
};
