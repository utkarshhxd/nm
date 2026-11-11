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
 * The text is the full status word, never an abbreviation.
 */
export function Badge({
  variant = "neutral",
  glyph,
  className,
  children,
  ...rest
}: ComponentProps<"span"> & { variant?: BadgeVariant; glyph?: BadgeGlyph }) {
  return (
    <span
      className={
        "inline-flex items-center gap-1 whitespace-nowrap rounded-full px-2 text-xs font-medium " +
        `${VARIANT[variant]}${className ? ` ${className}` : ""}`
      }
      {...rest}
    >
      {glyph ? <span aria-hidden="true">{GLYPH[glyph]}</span> : null}
      {children}
    </span>
  );
}
