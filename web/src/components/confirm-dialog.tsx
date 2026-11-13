// SPDX-License-Identifier: AGPL-3.0-only

import type { ReactNode } from "react";

import { Button, type ButtonVariant } from "./button";
import { Dialog } from "./dialog";
import { Alert } from "./field";

/**
 * A confirmation that says what will happen in its body and names the action on its button
 * ("Archive field", never "OK"). The caller owns the request: `onConfirm` starts it, `busy` is
 * its pending state and `error` the sentence for a refusal, which stays on screen so the person
 * sees why nothing changed. Focus starts on the heading, never on the destructive button.
 *
 * `variant` is red by default because most confirmations here guard something irreversible. An
 * action that is merely weighty passes `solid`: red on something "Remove tag" undoes completely,
 * two controls from a bulk delete, is how red stops meaning anything.
 */
export function ConfirmDialog({
  open,
  onClose,
  title,
  confirmLabel,
  onConfirm,
  busy,
  error,
  variant = "danger-solid",
  children,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  confirmLabel: string;
  onConfirm: () => void;
  busy?: boolean;
  error?: string | undefined;
  variant?: ButtonVariant;
  children: ReactNode;
}) {
  return (
    <Dialog open={open} onClose={onClose} title={title}>
      <div className="mt-3 space-y-3">{children}</div>
      {error ? <Alert>{error}</Alert> : null}
      <div className="mt-6 flex flex-wrap justify-end gap-3">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant={variant} busy={busy ?? false} onClick={onConfirm}>
          {confirmLabel}
        </Button>
      </div>
    </Dialog>
  );
}
