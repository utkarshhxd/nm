// SPDX-License-Identifier: AGPL-3.0-only

// The design system is src/styles/tokens.css and src/components/. What is left here is the
// compatibility layer the seven route files still import: the old class-string constants, now
// spelled with the primitives and the tokens, and the two composites (`ConfirmButton`,
// `CopyButton`) that are not primitives. The route-file tasks delete the constants as they move
// each screen onto the components.

import { useState } from "react";

import { Alert } from "../components/field";
import { buttonClass } from "../components/button";
import { CONTROL } from "../components/input";

export { Alert, Field } from "../components/field";

export const BUTTON = `mt-4 ${buttonClass("solid")}`;

export const SECONDARY_BUTTON = `mt-4 ml-3 ${buttonClass("outline")}`;

export const CARD = "mt-8 rounded-md border border-border bg-surface p-6";

/** A button sized for a table row or a list item: no `mt`/`ml`, which fight a flex row. */
export const ROW_BUTTON = buttonClass("outline");

export const SELECT = CONTROL;

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
        <span role="status" className="text-success-text">
          Copied
        </span>
      ) : null}
      {state === "failed" ? (
        <Alert>Copying failed — select the text and copy it yourself.</Alert>
      ) : null}
    </>
  );
}
