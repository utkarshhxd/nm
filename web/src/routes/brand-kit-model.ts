// SPDX-License-Identifier: AGPL-3.0-only

import type { BrandKit } from "./brand-kit-source";

/**
 * The palette keys this screen edits, in the order it shows them. The database only says
 * `palette` is an object; these names are this lane's choice until the renderer's are known, and
 * a key the screen does not know is carried through a save untouched (see `toKit`).
 */
export const PALETTE = [
  ["background", "Background colour"],
  ["text", "Text colour"],
  ["accent", "Accent colour"],
  ["accent_text", "Text on the accent colour"],
] as const;

export type Draft = {
  logo: string;
  font: string;
  footerText: string;
  footerAddress: string;
} & Record<(typeof PALETTE)[number][0], string>;

/** `#rgb` or `#rrggbb`. Only used to flag a colour as it is typed; the server decides what it takes. */
export const isColour = (s: string) => /^#(?:[0-9a-f]{3}|[0-9a-f]{6})$/i.test(s);

export const toDraft = (k: BrandKit): Draft => ({
  logo: k.logo_url ?? "",
  font: k.font_stack ?? "",
  footerText: k.footer["text"] ?? "",
  footerAddress: k.footer["address"] ?? "",
  background: k.palette["background"] ?? "",
  text: k.palette["text"] ?? "",
  accent: k.palette["accent"] ?? "",
  accent_text: k.palette["accent_text"] ?? "",
});

/** `base` with the screen's fields applied. A blank field is absent (or null), never `""`; keys the screen does not edit are kept. */
export function toKit(base: BrandKit, d: Draft): BrandKit {
  const put = (into: Record<string, string>, key: string, value: string) => {
    const v = value.trim();
    if (v === "") delete into[key];
    else into[key] = v;
  };
  const palette = { ...base.palette };
  for (const [key] of PALETTE) put(palette, key, d[key]);
  const footer = { ...base.footer };
  put(footer, "text", d.footerText);
  put(footer, "address", d.footerAddress);
  return { logo_url: d.logo.trim() || null, palette, font_stack: d.font.trim() || null, footer };
}
