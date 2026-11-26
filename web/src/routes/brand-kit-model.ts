// SPDX-License-Identifier: AGPL-3.0-only

import { hrefAllowed } from "../components/block-editor/runs";
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

/** A logo is an image source: `http` and `https` only, no user name or password (contract section 11). `mailto:` and `tel:` are links, not images. */
export const logoAllowed = (url: string) =>
  hrefAllowed(url, ["http:", "https:"]) && !new URL(url).username && !new URL(url).password;
/**
 * The one sentence for this rule, said by the inline flag and by the server's refusal alike
 * (contract section 11: two different explanations of one rule is how an operator learns to
 * ignore both). W5 chose its own wording while the server was a mock; this is now `BrandError::Logo`
 * in `src/template/mjml.rs`, word for word, because that is the sentence the operator gets back
 * from a save and only one of the two can move. `e2e/brand-kit.spec.ts` pins that they agree.
 */
export const LOGO_MESSAGE = "logo must be an http or https URL with no user name or password";

/** `BrandError::Colour`'s sentence for `field`, word for word (same rule as the logo: one rule, one sentence). */
export const colourMessage = (field: string) => `${field} must be a hex colour such as #fff or #1a2b3c`;

/** The server may put anything under a key this screen knows (a newer version's richer value): only a string is editable here. */
const str = (v: unknown) => (typeof v === "string" ? v : "");

export const toDraft = (k: BrandKit): Draft => ({
  logo: k.logo_url ?? "",
  font: k.font_stack ?? "",
  footerText: str(k.footer["text"]),
  footerAddress: str(k.footer["address"]),
  background: str(k.palette["background"]),
  text: str(k.palette["text"]),
  accent: str(k.palette["accent"]),
  accent_text: str(k.palette["accent_text"]),
});

/** The form after `kit` arrives where `seen` was: replaced, unless the operator has typed in it. */
export const resync = (draft: Draft, seen: BrandKit, kit: BrandKit): Draft =>
  JSON.stringify(draft) === JSON.stringify(toDraft(seen)) ? toDraft(kit) : draft;

/** `base` with the screen's fields applied. A blank field is absent (or null), never `""`; keys the screen does not edit are kept. */
export function toKit(base: BrandKit, d: Draft): BrandKit {
  const put = (into: Record<string, unknown>, key: string, value: string) => {
    const v = value.trim();
    if (v !== "") into[key] = v;
    // A blank field clears a string. A value that is not one was written by something newer than
    // this screen, which showed it as blank: leave it be.
    else if (typeof into[key] === "string") delete into[key];
  };
  const palette = { ...base.palette };
  for (const [key] of PALETTE) put(palette, key, d[key]);
  const footer = { ...base.footer };
  put(footer, "text", d.footerText);
  put(footer, "address", d.footerAddress);
  return { logo_url: d.logo.trim() || null, palette, font_stack: d.font.trim() || null, footer };
}

/** WCAG relative luminance of a valid `#rgb` or `#rrggbb`. */
function luminance(hex: string): number {
  const h = hex.slice(1);
  const full = h.length === 3 ? [...h].map((c) => c + c).join("") : h;
  const [r, g, b] = [0, 2, 4].map((i) => {
    const c = parseInt(full.slice(i, i + 2), 16) / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  }) as [number, number, number];
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

/** WCAG contrast ratio, 1 to 21, of two valid colours in either order. */
export function contrast(a: string, b: string): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x) as [number, number];
  return (hi + 0.05) / (lo + 0.05);
}

/** Two places, rounded DOWN: a ratio under the line must never be printed as the line. */
export const ratioText = (r: number) => (Math.floor(r * 100) / 100).toFixed(2);

/** The pairs a template renders together: only these are worth a warning. */
export const PAIRS = [
  ["Text and background colours", "text", "background"],
  ["Accent and accent text colours", "accent_text", "accent"],
] as const;

/** AA for body text. Advice, not a rule: nothing refuses a pair below it. */
export const MIN_CONTRAST = 4.5;
