// SPDX-License-Identifier: AGPL-3.0-only

import type { ComponentProps, ReactNode } from "react";

import { Input } from "./input";

/** An error message screen readers announce; `id` ties it to the field that caused it. */
export function Alert({ id, children }: { id?: string; children: ReactNode }) {
  return (
    <span id={id} role="alert" className="mt-1 block text-danger-text">
      {children}
    </span>
  );
}

type FieldProps = { id: string; label: string; error?: string; hint?: ReactNode } & ComponentProps<
  "input"
>;

/** A labelled input. Its hint and error are referenced by `aria-describedby`, in that order. */
export function Field({ id, label, error, hint, ...input }: FieldProps) {
  const described = [hint ? `${id}-hint` : "", error ? `${id}-error` : ""].filter(Boolean).join(" ");
  return (
    <div className="mt-4">
      {/* A label is not always ours: a custom field's is up to 100 characters of whatever its
          workspace typed, and one unbroken word would otherwise scroll the dialog sideways. */}
      <label className="block font-medium wrap-anywhere" htmlFor={id}>
        {label}
      </label>
      <Input
        id={id}
        className="mt-1"
        aria-invalid={error ? true : undefined}
        aria-describedby={described || undefined}
        {...input}
      />
      {hint ? (
        <span id={`${id}-hint`} className="mt-1 block text-sm text-fg-muted">
          {hint}
        </span>
      ) : null}
      {error ? <Alert id={`${id}-error`}>{error}</Alert> : null}
    </div>
  );
}
