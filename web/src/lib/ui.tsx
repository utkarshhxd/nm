// SPDX-License-Identifier: AGPL-3.0-only

// The visual design is provisional (global-constraints.md), so these are plain Tailwind
// utilities, not a design system. They exist so the accessibility wiring — label, focus ring,
// `aria-invalid` and `aria-describedby` — is written once instead of at each of the dozen
// fields across the sign-in, invite and security screens.

import { type ComponentProps, type ReactNode, useState } from "react";

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

/** A button sized for a table row or a list item: no `mt`/`ml`, which fight a flex row. */
export const ROW_BUTTON =
  "rounded border border-gray-500 px-3 py-1 font-medium text-blue-800 " +
  `dark:border-gray-400 dark:text-blue-200 ${FOCUS}`;

export const SELECT =
  "rounded border border-gray-500 bg-white px-2 py-1 text-gray-900 disabled:opacity-60 " +
  `dark:border-gray-400 dark:bg-gray-800 dark:text-gray-100 ${FOCUS}`;

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

/**
 * A destructive action behind one confirmation step, in place: removing a member, leaving a
 * workspace, revoking an invitation or a key. `window.confirm` would do the same job, but it
 * blocks the thread and cannot be styled or read by the same test selectors as everything else.
 *
 * `subject` is what is being acted on. It is only in the accessible name, so several rows can
 * each carry a "Remove" button without the names colliding; the visible text stays inside the
 * accessible name, which is what `label-content-name-mismatch` asks for.
 */
export function ConfirmButton({
  action,
  subject,
  onConfirm,
  disabled,
}: {
  action: string;
  subject: string;
  onConfirm: () => void;
  disabled?: boolean;
}) {
  const [armed, setArmed] = useState(false);
  const verb = action.toLowerCase();

  if (!armed) {
    return (
      <button
        className={ROW_BUTTON}
        type="button"
        aria-label={`${action} ${subject}`}
        disabled={disabled}
        onClick={() => setArmed(true)}
      >
        {action}
      </button>
    );
  }
  return (
    <span className="inline-flex gap-2">
      <button
        className={ROW_BUTTON}
        type="button"
        aria-label={`Confirm ${verb} ${subject}`}
        // Arming unmounts the button that was just pressed, so without this a keyboard user's
        // focus falls to <body> in the middle of a confirmation. Nothing automated catches it.
        autoFocus
        onClick={() => {
          setArmed(false);
          onConfirm();
        }}
      >
        Confirm
      </button>
      <button
        className={ROW_BUTTON}
        type="button"
        aria-label={`Cancel ${verb} ${subject}`}
        onClick={() => setArmed(false)}
      >
        Cancel
      </button>
    </span>
  );
}

/**
 * Puts `value` on the clipboard, and says so when it cannot.
 *
 * `navigator.clipboard` is **undefined** outside a secure context, so `navigator.clipboard
 * .writeText(…)` throws a synchronous TypeError that a trailing `.catch` never sees. Self-hosting
 * this over plain HTTP on a LAN address is a deployment we support, and the three things copied
 * in this app — an invite link, a new API key, a set of recovery codes — are each shown once or
 * are the whole point of the screen. Failing silently there loses them.
 */
export function CopyButton({ value, className }: { value: string; className?: string }) {
  const [state, setState] = useState<"" | "copied" | "failed">("");

  async function copy(): Promise<void> {
    try {
      const clipboard = navigator.clipboard as Clipboard | undefined;
      if (!clipboard) throw new Error("no clipboard on this origin");
      await clipboard.writeText(value);
      setState("copied");
    } catch {
      setState("failed");
    }
  }

  return (
    <>
      <button className={className ?? ROW_BUTTON} type="button" onClick={() => void copy()}>
        Copy
      </button>
      {state === "copied" ? (
        <span role="status" className="text-green-800 dark:text-green-300">
          Copied
        </span>
      ) : null}
      {state === "failed" ? (
        <Alert>Copying failed — select the text and copy it yourself.</Alert>
      ) : null}
    </>
  );
}
