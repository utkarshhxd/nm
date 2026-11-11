// SPDX-License-Identifier: AGPL-3.0-only

import type { ComponentProps } from "react";

/** Shared with `Select`: the two controls must look like one family. Width is the caller's. */
export const CONTROL =
  "min-h-(--control-h) rounded-sm border border-border-strong bg-surface px-3 text-fg " +
  "placeholder:text-fg-subtle disabled:cursor-not-allowed disabled:opacity-60 " +
  "aria-[invalid=true]:border-danger";

/** A native input. Give it a visible <label> (see `Field`); a placeholder is never a label. */
export function Input({ className, ...rest }: ComponentProps<"input">) {
  return <input className={`w-full ${CONTROL}${className ? ` ${className}` : ""}`} {...rest} />;
}
