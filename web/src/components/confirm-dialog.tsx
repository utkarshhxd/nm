// SPDX-License-Identifier: AGPL-3.0-only

import type { ReactNode } from "react";

import { Button } from "./button";
import { Dialog } from "./dialog";
import { Alert } from "./field";

/**
 * A confirmation that says what will happen in its body and names the action on its button
 * ("Archive field", never "OK"). The caller owns the request: `onConfirm` starts it, `busy` is
 * its pending state and `error` the sentence for a refusal, which stays on screen so the person
 * sees why nothing changed. Focus starts on the heading, never on the destructive button.
 */
export function ConfirmDialog({
  open,
  onClose,
  title,
  confirmLabel,
  onConfirm,
  busy,
  error,
  children,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  confirmLabel: string;
  onConfirm: () => void;
  busy?: boolean;
  error?: string | undefined;
  children: ReactNode;
}) {
  return (
    <Dialog open={open} onClose={onClose} title={title}>
      <div className="mt-3 space-y-3">{children}</div>
      {error ? <Alert>{error}</Alert> : null}
      <div className="mt-6 flex flex-wrap justify-end gap-3">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="danger-solid" busy={busy ?? false} onClick={onConfirm}>
          {confirmLabel}
        </Button>
      </div>
    </Dialog>
  );
}
