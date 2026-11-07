// SPDX-License-Identifier: AGPL-3.0-only

// The visual design is provisional (global-constraints.md), so these are plain Tailwind
// utilities, not a design system. They exist so the accessibility wiring — label, focus ring,
// `aria-invalid` and `aria-describedby` — is written once instead of at each of the dozen
// fields across the sign-in, invite and security screens.

import type { ComponentProps, ReactNode } from "react";

const FOCUS =
  "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-blue-700 " +
  "dark:focus-visible:outline-blue-300";

export const BUTTON =
  `mt-4 rounded bg-blue-700 px-4 py-2 font-medium text-white disabled:opacity-60 ${FOCUS}`;

export const SECONDARY_BUTTON =
  "mt-4 ml-3 rounded border border-gray-500 px-4 py-2 font-medium text-blue-800 " +
  `dark:border-gray-400 dark:text-blue-200 ${FOCUS}`;

export const CARD =
  "mt-8 rounded border border-gray-300 p-6 dark:border-gray-700";

/** An error message screen readers announce; `id` ties it to the field that caused it. */
export function Alert({ id, children }: { id?: string; children: ReactNode }) {
  return (
    <span id={id} role="alert" className="mt-1 block text-red-800 dark:text-red-300">
      {children}
    </span>
  );
}

type FieldProps = { id: string; label: string; error?: string; hint?: ReactNode } & ComponentProps<
  "input"
>;

/** A labelled input. Its error is announced and referenced by `aria-describedby`. */
export function Field({ id, label, error, hint, ...input }: FieldProps) {
  const described = [hint ? `${id}-hint` : "", error ? `${id}-error` : ""].filter(Boolean).join(" ");
  return (
    <div className="mt-4">
      <label className="block font-medium" htmlFor={id}>
        {label}
      </label>
      <input
        id={id}
        className={
          "mt-1 w-full rounded border border-gray-500 bg-white px-3 py-2 text-gray-900 " +
          `dark:border-gray-400 dark:bg-gray-800 dark:text-gray-100 ${FOCUS}`
        }
        aria-invalid={error ? true : undefined}
        aria-describedby={described || undefined}
        {...input}
      />
      {hint ? (
        <span id={`${id}-hint`} className="mt-1 block text-sm text-gray-700 dark:text-gray-300">
          {hint}
        </span>
      ) : null}
      {error ? <Alert id={`${id}-error`}>{error}</Alert> : null}
    </div>
  );
}
