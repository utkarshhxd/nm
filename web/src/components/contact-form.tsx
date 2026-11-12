// SPDX-License-Identifier: AGPL-3.0-only

import { useQuery } from "@tanstack/react-query";
import { type FormEvent, type ReactNode, useMemo, useState } from "react";

import { api, problemText, problemType } from "../api/client";
import type { components } from "../api/schema";
import { Button } from "./button";
import { Checkbox } from "./checkbox";
import { Alert, Field } from "./field";
import { Select } from "./select";
import { Skeleton } from "./skeleton";

type Contact = components["schemas"]["Contact"];
type Definition = components["schemas"]["Definition"];

const KNOWN = {
  "email-taken": "A contact with that address already exists in this workspace.",
  "address-erased":
    "That address was erased on request. It can never be added to this workspace again.",
  "insufficient-role": "Your role cannot change contacts. It takes editor or above.",
};

/** A field's value while it is being edited: text for every kind, a list for `multi_select`. */
type Draft = string | string[];

const blank = (v: Draft) => (Array.isArray(v) ? v.length === 0 : v.trim() === "");

/** `2026-03-09T14:30:00Z` as the local wall time `<input type="datetime-local">` wants. */
function localInput(iso: string): string {
  const t = new Date(iso);
  if (Number.isNaN(t.getTime())) return "";
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${t.getFullYear()}-${pad(t.getMonth() + 1)}-${pad(t.getDate())}T${pad(t.getHours())}:${pad(t.getMinutes())}`;
}

function toDraft(def: Definition, value: unknown): Draft {
  if (value == null) return def.kind === "multi_select" ? [] : "";
  switch (def.kind) {
    case "boolean":
      return value === true ? "yes" : "no";
    case "multi_select":
      return Array.isArray(value) ? value.map(String) : [];
    case "datetime":
      return typeof value === "string" ? localInput(value) : "";
    default:
      return String(value);
  }
}

/**
 * The draft as JSON for the API. A value the browser cannot type (a number box holding text, an
 * unparsable date) is sent as it stands rather than silently dropped, so the server answers with
 * the field error that says what is wrong with it.
 */
function toJson(def: Definition, value: Draft): unknown {
  if (Array.isArray(value)) return value;
  switch (def.kind) {
    case "number": {
      const n = Number(value);
      return Number.isFinite(n) ? n : value;
    }
    case "boolean":
      return value === "yes";
    case "datetime": {
      const t = new Date(value);
      return Number.isNaN(t.getTime()) ? value : t.toISOString();
    }
    default:
      return value;
  }
}

/** Messages by field name, where `""` is the form as a whole. */
type Failures = Record<string, string>;

function failuresOf(problem: unknown, fallback: string): Failures {
  const text = problemText(problem, KNOWN, fallback);
  const code = problemType(problem).replace("urn:nm:problem:", "");
  // The two conflicts are about the address, and belong next to it, not in a form-wide alert.
  if (code === "email-taken" || code === "address-erased") return { email: text };
  const list = (problem as { errors?: { field?: string; message?: string }[] } | null | undefined)
    ?.errors;
  if (!list?.length) return { "": text };
  const out: Failures = {};
  for (const e of list) {
    if (e.field && e.message) out[e.field] = e.message;
    else out[""] = text;
  }
  return out;
}

/**
 * The one form behind both "New contact" and "Edit contact". It renders the workspace's custom
 * field definitions as well as the built-in fields, because the server validates them on both
 * routes and a form that cannot set them cannot create the contacts a workspace actually needs.
 *
 * Editing sends a merge patch (RFC 7386): only what changed, and an emptied optional field goes
 * as `null`, never `""`. `""` is a value — it would store an empty string instead of clearing.
 * The address is not patchable, so it is shown as text when editing.
 *
 * Archived definitions are left out: the server refuses new values for them. Whatever a contact
 * already holds for one stays readable on its page.
 */
export function ContactForm({
  ws,
  existing,
  onCancel,
  onDone,
}: {
  ws: string;
  existing?: Contact;
  onCancel: () => void;
  onDone: () => Promise<void> | void;
}) {
  const defs = useQuery({
    queryKey: ["custom-fields", ws],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/custom-fields", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      if (!data) throw new Error(problemText(error, KNOWN, "The custom fields could not be loaded."));
      // shortcut: the first 200 definitions only (the server maximum), as the contact page does.
      return data.data;
    },
    retry: false,
  });
  // Oldest first, as the contact page lists them.
  const fields = useMemo(
    () => [...(defs.data ?? [])].reverse().filter((d) => d.archived_at == null),
    [defs.data],
  );
  const zones = useMemo(() => Intl.supportedValuesOf("timeZone"), []);

  const values = (existing?.attributes ?? {}) as Record<string, unknown>;
  const [email, setEmail] = useState("");
  const [first, setFirst] = useState(existing?.first_name ?? "");
  const [last, setLast] = useState(existing?.last_name ?? "");
  const [timezone, setTimezone] = useState(existing?.timezone ?? "");
  // Keyed by field key; a key with no entry yet has never been touched on this form.
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  const draft = (d: Definition): Draft => drafts[d.key] ?? toDraft(d, values[d.key]);
  const set = (key: string, value: Draft) => setDrafts((prev) => ({ ...prev, [key]: value }));

  const [failures, setFailures] = useState<Failures>({});
  const [busy, setBusy] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setFailures({});
    const attributes: Record<string, unknown> = {};
    for (const d of fields) {
      const now = draft(d);
      if (existing) {
        // Only what changed: an untouched key must keep whatever the contact holds.
        if (JSON.stringify(now) === JSON.stringify(toDraft(d, values[d.key]))) continue;
        attributes[d.key] = blank(now) ? null : toJson(d, now);
      } else if (!blank(now)) {
        attributes[d.key] = toJson(d, now);
      }
    }
    const { data, error } = existing
      ? await api.PATCH("/api/v1/workspaces/{workspace_id}/contacts/{id}", {
          params: { path: { workspace_id: ws, id: existing.id } },
          body: {
            ...(first.trim() === existing.first_name ? {} : { first_name: first.trim() || null }),
            ...(last.trim() === existing.last_name ? {} : { last_name: last.trim() || null }),
            ...(timezone === (existing.timezone ?? "") ? {} : { timezone: timezone || null }),
            ...(Object.keys(attributes).length > 0 ? { attributes } : {}),
          },
        })
      : await api.POST("/api/v1/workspaces/{workspace_id}/contacts", {
          params: { path: { workspace_id: ws } },
          body: {
            email: email.trim(),
            ...(first.trim() ? { first_name: first.trim() } : {}),
            ...(last.trim() ? { last_name: last.trim() } : {}),
            ...(timezone ? { timezone } : {}),
            ...(Object.keys(attributes).length > 0 ? { attributes } : {}),
          },
        });
    setBusy(false);
    if (!data) {
      setFailures(failuresOf(error, "The contact could not be saved."));
      return;
    }
    await onDone();
  }

  return (
    <form onSubmit={submit} noValidate>
      {existing ? (
        <p className="mt-3">
          <span className="font-medium">Email</span>{" "}
          <span className="wrap-anywhere">{existing.email}</span>. It cannot be changed. Every
          list membership here is consent this address gave, and the suppressions that block it
          are held against it too; moving the contact to another address would hand one person&rsquo;s
          consent to another, and take them out from under their own blocks. Delete this contact
          and add the new address, so that address gives its own consent — or erase this one if
          the person asked to be forgotten.
        </p>
      ) : (
        <Field
          id="contact-email"
          label="Email"
          type="email"
          autoComplete="off"
          required
          value={email}
          error={failures["email"]}
          onChange={(e) => setEmail(e.target.value)}
        />
      )}
      <Field
        id="contact-first"
        label="First name"
        autoComplete="off"
        maxLength={100}
        value={first}
        error={failures["first_name"]}
        onChange={(e) => setFirst(e.target.value)}
      />
      <Field
        id="contact-last"
        label="Last name"
        autoComplete="off"
        maxLength={100}
        value={last}
        error={failures["last_name"]}
        onChange={(e) => setLast(e.target.value)}
      />
      <Labelled id="contact-timezone" label="Time zone" error={failures["timezone"]}>
        <Select
          id="contact-timezone"
          className="mt-1 w-full"
          aria-describedby={failures["timezone"] ? "contact-timezone-error" : undefined}
          value={timezone}
          onChange={(e) => setTimezone(e.target.value)}
        >
          <option value="">No time zone</option>
          {zones.map((z) => (
            <option key={z} value={z}>
              {z}
            </option>
          ))}
        </Select>
      </Labelled>

      {defs.isPending ? (
        <div className="mt-4">
          <Skeleton label="Loading custom fields" />
        </div>
      ) : defs.isError ? (
        <Alert>{defs.error.message}</Alert>
      ) : (
        fields.map((d) => (
          <CustomField key={d.id} def={d} value={draft(d)} error={failures[`attributes.${d.key}`]} onChange={(v) => set(d.key, v)} />
        ))
      )}

      {failures[""] ? <Alert>{failures[""]}</Alert> : null}
      <div className="mt-6 flex flex-wrap justify-end gap-3">
        <Button onClick={onCancel}>Cancel</Button>
        <Button type="submit" variant="solid" busy={busy}>
          {existing ? "Save contact" : "Create contact"}
        </Button>
      </div>
    </form>
  );
}

/** A label, a control the caller supplies and its error, for the kinds `Field` does not cover. */
function Labelled({
  id,
  label,
  error,
  children,
}: {
  id: string;
  label: string;
  error?: string;
  children: ReactNode;
}) {
  return (
    <div className="mt-4">
      <label className="block font-medium wrap-anywhere" htmlFor={id}>
        {label}
      </label>
      {children}
      {error ? <Alert id={`${id}-error`}>{error}</Alert> : null}
    </div>
  );
}

const TYPE: Record<string, string> = {
  text: "text",
  number: "number",
  date: "date",
  datetime: "datetime-local",
};

function CustomField({
  def,
  value,
  error,
  onChange,
}: {
  def: Definition;
  value: Draft;
  error?: string;
  onChange: (value: Draft) => void;
}) {
  const id = `contact-cf-${def.key}`;
  const described = error ? `${id}-error` : undefined;

  if (def.kind === "multi_select") {
    const picked = Array.isArray(value) ? value : [];
    return (
      // A group, not a <select multiple>: a fieldset of checkboxes is the one form of multiple
      // choice that works with a keyboard, a screen reader and a phone alike.
      <fieldset className="mt-4" aria-describedby={described}>
        <legend className="font-medium wrap-anywhere">{def.label}</legend>
        <div className="mt-1 flex flex-wrap gap-x-4 gap-y-2">
          {def.options.map((o) => (
            <label key={o} className="flex items-center gap-2 wrap-anywhere">
              <Checkbox
                checked={picked.includes(o)}
                onChange={(e) =>
                  onChange(e.target.checked ? [...picked, o] : picked.filter((p) => p !== o))
                }
              />
              {o}
            </label>
          ))}
        </div>
        {error ? <Alert id={`${id}-error`}>{error}</Alert> : null}
      </fieldset>
    );
  }

  if (def.kind === "boolean" || def.kind === "select") {
    // A checkbox cannot say "not set", which is what an unanswered yes-or-no field is.
    const options: [string, string][] =
      def.kind === "boolean"
        ? [
            ["yes", "Yes"],
            ["no", "No"],
          ]
        : def.options.map((o) => [o, o]);
    return (
      <Labelled id={id} label={def.label} error={error}>
        <Select
          id={id}
          className="mt-1 w-full"
          aria-describedby={described}
          value={String(value)}
          onChange={(e) => onChange(e.target.value)}
        >
          <option value="">Not set</option>
          {options.map(([v, text]) => (
            <option key={v} value={v}>
              {text}
            </option>
          ))}
        </Select>
      </Labelled>
    );
  }

  return (
    <Field
      id={id}
      label={def.label}
      type={TYPE[def.kind] ?? "text"}
      autoComplete="off"
      value={String(value)}
      error={error}
      onChange={(e) => onChange(e.target.value)}
    />
  );
}
