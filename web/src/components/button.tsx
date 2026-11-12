// SPDX-License-Identifier: AGPL-3.0-only

import type { ComponentProps } from "react";

export type ButtonVariant = "solid" | "outline" | "ghost" | "danger-solid" | "danger-ghost";

const BASE =
  "inline-flex min-h-(--control-h) min-w-(--tap-min) items-center justify-center gap-2 " +
  "rounded-sm px-3 font-medium disabled:cursor-not-allowed disabled:opacity-60";

const VARIANT: Record<ButtonVariant, string> = {
  // One per view.
  solid: "bg-accent text-on-accent hover:bg-accent-hover",
  outline: "border border-border-strong bg-surface text-fg hover:bg-sunken",
  ghost: "text-fg hover:bg-sunken",
  "danger-solid": "bg-danger text-on-danger hover:bg-danger/90",
  "danger-ghost": "text-danger-text hover:bg-danger-subtle",
};

/** The classes of a button, for the one place that must style a link or a legacy constant as one. */
export function buttonClass(variant: ButtonVariant = "outline"): string {
  return `${BASE} ${VARIANT[variant]}`;
}

type ButtonProps = ComponentProps<"button"> & {
  variant?: ButtonVariant;
  /**
   * Keeps the label, so the width does not change, and says so to assistive technology. It does
   * not disable the button: a form submit is never disabled, it is validated on submit.
   */
  busy?: boolean;
};

export function Button({ variant = "outline", busy, className, type = "button", ...rest }: ButtonProps) {
  return (
    <button
      type={type}
      className={`${buttonClass(variant)}${className ? ` ${className}` : ""}`}
      aria-busy={busy || undefined}
      {...rest}
    />
  );
}
