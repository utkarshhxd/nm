// SPDX-License-Identifier: AGPL-3.0-only

import type { ComponentProps } from "react";

export type BadgeVariant = "neutral" | "accent" | "success" | "warning" | "danger" | "info";

/** A shape as well as a colour: status is never carried by colour alone (WCAG 1.4.1). */
export type BadgeGlyph = "dot" | "ring" | "slash" | "cross";

const VARIANT: Record<BadgeVariant, string> = {
  neutral: "bg-sunken text-fg-muted",
  accent: "bg-accent-subtle text-accent-text",
  success: "bg-success-subtle text-success-text",
  warning: "bg-warning-subtle text-warning-text",
  danger: "bg-danger-subtle text-danger-text",
  info: "bg-info-subtle text-info-text",
};

const GLYPH: Record<BadgeGlyph, string> = { dot: "●", ring: "○", slash: "⊘", cross: "✕" };

/**
 * A status or a tag. Non-interactive; a removable tag is a Badge with a real <button> inside it.
 * The text is the full status word, never an abbreviation. `wrap` is for text a user typed, which
 * may be any length: it breaks anywhere rather than running out of its container.
 */
export function Badge({
  variant = "neutral",
  glyph,
  wrap,
  className,
  children,
  ...rest
}: ComponentProps<"span"> & { variant?: BadgeVariant; glyph?: BadgeGlyph; wrap?: boolean }) {
  return (
    <span
      className={
        "inline-flex items-center gap-1 rounded-full px-2 text-xs font-medium " +
        (wrap ? "max-w-full wrap-anywhere " : "whitespace-nowrap ") +
        `${VARIANT[variant]}${className ? ` ${className}` : ""}`
      }
      {...rest}
    >
      {glyph ? <span aria-hidden="true">{GLYPH[glyph]}</span> : null}
      {children}
    </span>
  );
}
