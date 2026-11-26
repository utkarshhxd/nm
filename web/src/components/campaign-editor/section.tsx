// SPDX-License-Identifier: AGPL-3.0-only

import { Link } from "@tanstack/react-router";
import { type ComponentProps, type ReactNode, useId } from "react";

import { problemText, problemType } from "../../api/client";
import { Button } from "../button";
import { Alert } from "../field";
import { CONTROL } from "../input";

/** One editor section: a named region, its fields disabled together when the role or state forbids editing. */
export function Section({
  title,
  disabled = false,
  children,
}: {
  title: string;
  disabled?: boolean;
  children: ReactNode;
}) {
  const id = useId();
  return (
    <section aria-labelledby={id} className="mt-8 rounded-md border border-border bg-surface p-6">
      <h3 id={id} className="text-lg font-semibold">
        {title}
      </h3>
      <fieldset disabled={disabled} className="m-0 min-w-0 border-0 p-0">
        {children}
      </fieldset>
    </section>
  );
}

/** A labelled native select with its error beside it, wired as `Field` wires an input. */
export function SelectField({
  id,
  label,
  error,
  children,
  ...select
}: { id: string; label: string; error?: string | undefined } & ComponentProps<"select">) {
  return (
    <div className="mt-4">
      <label className="block font-medium wrap-anywhere" htmlFor={id}>
        {label}
      </label>
      <select
        id={id}
        className={`${CONTROL} mt-1 max-w-full min-w-0`}
        aria-invalid={error ? true : undefined}
        aria-describedby={error ? `${id}-error` : undefined}
        {...select}
      >
        {children}
      </select>
      {error ? <Alert id={`${id}-error`}>{error}</Alert> : null}
    </div>
  );
}

/** A save button, the live region that says it worked, and the sentence for a refusal that names no field. */
export function SaveRow({
  label,
  busy,
  saved,
  onSave,
  children,
}: {
  label: string;
  busy: boolean;
  saved: boolean;
  onSave: () => void;
  children?: ReactNode;
}) {
  return (
    <div className="mt-6 flex flex-wrap items-center gap-3">
      <Button
        variant="solid"
        busy={busy}
        // `busy` does not disable: a second press while saving is ignored here.
        onClick={() => {
          if (!busy) onSave();
        }}
      >
        {label}
      </Button>
      {/* Always mounted: a live region that arrives with its text already in it is not announced. */}
      <span role="status" className="text-fg-muted">
        {saved ? "Saved." : ""}
      </span>
      {children}
    </div>
  );
}

const link = "text-accent-text underline";

/**
 * A refusal in the server's own sentence (`problemText`, never reworded), and for the three the
 * contract names, the screen where it is fixed. Nothing here is shown for a 422 that names a field:
 * that sentence is on the field.
 */
export function Problem({
  error,
  slug,
  templates,
}: {
  error: unknown;
  slug: string;
  /** The templates this campaign uses, for a `template-stale` link to each. */
  templates: { id: string; name: string }[];
}) {
  const code = problemType(error).replace("urn:nm:problem:", "");
  return (
    <>
      <Alert>{problemText(error, {}, "That did not work. Try again.")}</Alert>
      {code === "postal-address-required" ? (
        <p className="mt-1">
          <Link to="/w/$slug/brand-kit" params={{ slug }} className={link}>
            Open the brand kit
          </Link>
        </p>
      ) : null}
      {code === "sending-domain-unverified" ? (
        <p className="mt-1">
          <Link to="/w/$slug/sending-domains" params={{ slug }} className={link}>
            Open sending domains
          </Link>
        </p>
      ) : null}
      {code === "template-stale"
        ? templates.map((t) => (
            <p key={t.id} className="mt-1">
              <Link to="/w/$slug/templates/$templateId" params={{ slug, templateId: t.id }} className={link}>
                Open template {t.name}
              </Link>
            </p>
          ))
        : null}
    </>
  );
}
