// SPDX-License-Identifier: AGPL-3.0-only

import { type ComponentProps, useEffect, useId, useRef } from "react";

type DialogProps = Omit<ComponentProps<"dialog">, "open" | "title"> & {
  open: boolean;
  /** Called when the browser closes the dialog (Escape) and when `open` goes false. Idempotent. */
  onClose: () => void;
  /** The <h2>, and the dialog's accessible name. Name the action and the object, not "Are you sure?". */
  title: string;
  /** `drawer` is the same element anchored to the inline-end edge; it is modal too. */
  variant?: "dialog" | "drawer";
};

// Tailwind's preflight zeroes every margin, which also removes the UA's `margin: auto` that
// centres a modal dialog, so `m-auto` is restored here.
const SHAPE = {
  dialog:
    "m-auto w-[min(32rem,calc(100%-2rem))] rounded-lg " +
    // Under 40rem a centred dialog with the keyboard open shows about two fields: a bottom sheet.
    "max-sm:mb-0 max-sm:mt-auto max-sm:max-h-[90dvh] max-sm:w-full max-sm:max-w-full max-sm:rounded-b-none",
  drawer: "my-0 me-0 ms-auto h-dvh max-h-none w-[min(36rem,100%)] max-w-full rounded-none",
};

/**
 * A modal on the native <dialog> and `showModal()`, which already provides the focus trap, an
 * inert page behind it, Escape, the top layer and `::backdrop`. Native `close()` also returns
 * focus to whatever had it when the dialog opened, so there is no focus code here for that.
 *
 * The one thing the browser does not do is pick a *good* first focus: the first field, or the
 * heading when the dialog is informational, never a destructive button.
 *
 * Children are mounted only while open, so a form inside always starts empty.
 *
 * Set `open` to false before unmounting a Dialog. Unmounting one that is still open skips
 * `close()`, so the browser never returns focus to the trigger and it falls to <body>.
 */
export function Dialog({
  open,
  onClose,
  title,
  variant = "dialog",
  className,
  children,
  ...rest
}: DialogProps) {
  const ref = useRef<HTMLDialogElement>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  const titleId = useId();

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    if (open && !el.open) {
      el.showModal();
      (el.querySelector<HTMLElement>("input, select, textarea") ?? heading.current)?.focus();
    } else if (!open && el.open) {
      el.close();
    }
  }, [open]);

  return (
    <dialog
      ref={ref}
      aria-labelledby={titleId}
      onClose={onClose}
      className={
        "border border-border bg-surface p-6 text-fg shadow-dialog backdrop:bg-backdrop " +
        `${SHAPE[variant]}${className ? ` ${className}` : ""}`
      }
      {...rest}
    >
      <h2 id={titleId} ref={heading} tabIndex={-1} className="text-lg font-semibold outline-none">
        {title}
      </h2>
      {open ? children : null}
    </dialog>
  );
}
