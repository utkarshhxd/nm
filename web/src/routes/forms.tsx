// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";

import { api, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Button } from "../components/button";
import { Checkbox } from "../components/checkbox";
import { Dialog } from "../components/dialog";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Alert, Field } from "../components/field";
import { Skeleton } from "../components/skeleton";
import { TableScroller } from "../components/table-scroller";
import { atLeast } from "../lib/roles";
import { useWorkspace } from "./workspace-layout";

export type Form = components["schemas"]["Form"];
type FormField = components["schemas"]["FormField"];
type FormTarget = components["schemas"]["FormTarget"];

const day = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });
const link = "text-accent-text underline";
const th = "py-2 pe-3 text-start font-medium";
const KNOWN = {
  "form-slug-taken": "A form with that slug already exists in this workspace.",
  "insufficient-role": "Your role cannot change forms. It takes editor or above.",
};

export default function Forms() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return (
    <AllForms
      ws={workspace.id}
      slug={workspace.slug}
      canEdit={atLeast(workspace.role, "editor")}
    />
  );
}

function AllForms({ ws, slug, canEdit }: { ws: string; slug: string; canEdit: boolean }) {
  const [creating, setCreating] = useState(false);
  const forms = useQuery({
    queryKey: ["forms", ws, "all"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/forms", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      if (!data) throw new Error(problemText(error, KNOWN, "The forms could not be loaded."));
      // shortcut: the first 200 forms only; add "Load more" when a workspace has more.
      return data.data;
    },
    retry: false,
  });

  return (
    <section className="mt-8">
      <div className="flex flex-wrap items-baseline justify-between gap-3">
        <h2 className="text-xl font-semibold">Forms</h2>
        {canEdit ? (
          <Button variant="solid" onClick={() => setCreating(true)}>
            New form
          </Button>
        ) : null}
      </div>
      <div className="mt-4">
        {forms.isPending ? (
          <Skeleton label="Loading forms" rows={5} />
        ) : forms.isError ? (
          <ErrorState title="The forms could not be loaded." onRetry={() => void forms.refetch()}>
            {forms.error.message}
          </ErrorState>
        ) : forms.data.length === 0 ? (
          <EmptyState title="No forms yet">
            A form is a hosted signup page, and an HTML snippet you can paste into your own site.
          </EmptyState>
        ) : (
          <TableScroller label="Forms table"><table className="w-full">
            <caption className="sr-only">Forms</caption>
            <thead>
              <tr className="border-b border-border">
                <th scope="col" className={th}>
                  Name
                </th>
                <th scope="col" className={th}>
                  Address
                </th>
                <th scope="col" className={th}>
                  Lists
                </th>
                <th scope="col" className={th}>
                  Created
                </th>
              </tr>
            </thead>
            <tbody>
              {forms.data.map((f) => (
                <tr key={f.id} className="border-b border-border">
                  <td className="py-2 pe-3 wrap-anywhere">
                    <Link to="/w/$slug/forms/$formId" params={{ slug, formId: f.id }} className={link}>
                      {f.name}
                    </Link>
                  </td>
                  <td className="py-2 pe-3 wrap-anywhere">{publicPath(slug, f.slug)}</td>
                  <td className="py-2 pe-3 tabular-nums">{f.list_ids.length}</td>
                  <td className="py-2 tabular-nums text-fg-muted">
                    {day.format(new Date(f.created_at))}
                  </td>
                </tr>
              ))}
            </tbody>
          </table></TableScroller>
        )}
      </div>

      <Dialog open={creating} onClose={() => setCreating(false)} title="New form">
        <FormEditor ws={ws} onCancel={() => setCreating(false)} onDone={() => setCreating(false)} />
      </Dialog>
    </section>
  );
}

/**
 * Where a form answers, as a path.
 *
 * **Both halves come from the workspace the operator is looking at**, never from a configured
 * name or from "the first workspace this account has": `slug` is `useWorkspace()`'s, which is the
 * same record whose `id` fetched the form. The two cannot disagree. A path naming the wrong
 * workspace would send a visitor's address to another tenant's form, and nothing on the
 * operator's own page would show it.
 */
export function publicPath(workspaceSlug: string, formSlug: string): string {
  return `/f/${workspaceSlug}/${formSlug}`;
}

/** The optional built-in targets, in the order they are offered and rendered. */
const BUILT_IN: { key: string; target: FormTarget; label: string }[] = [
  { key: "first_name", target: { kind: "first_name" }, label: "First name" },
  { key: "last_name", target: { kind: "last_name" }, label: "Last name" },
  { key: "timezone", target: { kind: "timezone" }, label: "Time zone" },
];

type Collected = { on: boolean; label: string; required: boolean };

/** What the editor holds for one offerable field, keyed by `key` above or a custom field's key. */
const collected = (fields: FormField[], target: FormTarget, fallback: string): Collected => {
  const found = fields.find((f) => sameTarget(f.target, target));
  return {
    on: found !== undefined,
    label: found?.label ?? fallback,
    required: found?.required ?? false,
  };
};

const sameTarget = (a: FormTarget, b: FormTarget) =>
  a.kind === b.kind && (a.kind !== "custom" || b.kind !== "custom" || a.value === b.value);

const EMAIL: FormTarget = { kind: "email" };

/**
 * Create a form, or edit one. Shared by the list screen's New form dialog and the detail
 * screen's Edit form dialog, so there is one editor and one set of names.
 *
 * The address is not an option: a submission with no address subscribes nobody, and
 * `forms::validate_fields` refuses a form that does not collect one exactly once. It is shown as
 * a labelled row with no checkbox rather than a ticked one that cannot be unticked.
 *
 * shortcut: only custom fields of kind `text` are offered. `src/public/forms.rs::input` renders
 * every custom field as a text box — a `select` or a `date` offered here would be typed into as
 * free text and refused by `contacts::create` with "check the details". Both shortcuts lift
 * together: give the public page the workspace's definitions, then widen this filter.
 */
export function FormEditor({
  ws,
  existing,
  onCancel,
  onDone,
}: {
  ws: string;
  existing?: Form;
  onCancel: () => void;
  onDone: (form: Form) => void;
}) {
  const qc = useQueryClient();
  const fields = existing?.fields ?? [];
  const [name, setName] = useState(existing?.name ?? "");
  const [slug, setSlug] = useState(existing?.slug ?? "");
  const [headline, setHeadline] = useState(existing?.headline ?? "");
  const [button, setButton] = useState(existing?.button_label ?? "Subscribe");
  const [redirect, setRedirect] = useState(existing?.redirect_url ?? "");
  const [emailLabel, setEmailLabel] = useState(
    fields.find((f) => f.target.kind === "email")?.label ?? "Email address",
  );
  const [lists, setLists] = useState<string[]>(existing?.list_ids ?? []);
  const [picked, setPicked] = useState<Record<string, Collected>>(() =>
    Object.fromEntries(BUILT_IN.map((b) => [b.key, collected(fields, b.target, b.label)])),
  );

  // The same key and request as the contacts grid's filter and the contact page's picker.
  const allLists = useQuery({
    queryKey: ["lists", ws],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/lists", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      return data?.data ?? [];
    },
  });
  const customFields = useQuery({
    queryKey: ["custom-fields", ws],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/custom-fields", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      return data?.data ?? [];
    },
  });
  // Oldest first, as the contact page shows them. An archived definition is not offered: a form
  // collecting into one collects into nowhere.
  const custom = [...(customFields.data ?? [])]
    .reverse()
    .filter((d) => d.kind === "text" && d.archived_at == null);

  const state = (key: string, fallback: string): Collected =>
    picked[key] ?? collected(fields, { kind: "custom", value: key }, fallback);
  const set = (key: string, fallback: string, change: Partial<Collected>) =>
    setPicked((was) => ({ ...was, [key]: { ...state(key, fallback), ...change } }));

  const body = () => {
    const chosen: FormField[] = [
      { target: EMAIL, required: true, label: emailLabel.trim() },
      ...BUILT_IN.filter((b) => state(b.key, b.label).on).map((b) => ({
        target: b.target,
        required: state(b.key, b.label).required,
        label: state(b.key, b.label).label.trim(),
      })),
      ...custom
        .filter((d) => state(d.key, d.label).on)
        .map((d) => ({
          target: { kind: "custom" as const, value: d.key },
          required: state(d.key, d.label).required,
          label: state(d.key, d.label).label.trim(),
        })),
    ];
    return {
      slug: slug.trim(),
      name: name.trim(),
      list_ids: lists,
      fields: chosen,
      // An empty box means "no redirect", which the API spells `null`.
      redirect_url: redirect.trim() === "" ? null : redirect.trim(),
      headline: headline.trim(),
      button_label: button.trim(),
    };
  };

  const save = useMutation({
    mutationFn: async () => {
      // The whole definition either way: unlike a list, every field here is part of what the
      // public page renders, so there is no "only what changed" to send.
      const { data, error } = existing
        ? await api.PATCH("/api/v1/workspaces/{workspace_id}/forms/{id}", {
            params: { path: { workspace_id: ws, id: existing.id } },
            body: body(),
          })
        : await api.POST("/api/v1/workspaces/{workspace_id}/forms", {
            params: { path: { workspace_id: ws } },
            body: body(),
          });
      if (!data) throw new Error(problemText(error, KNOWN, "The form could not be saved."));
      return data;
    },
    onSuccess: async (form) => {
      onDone(form);
      await Promise.all([
        qc.invalidateQueries({ queryKey: ["forms", ws] }),
        existing ? qc.invalidateQueries({ queryKey: ["form", ws, existing.id] }) : null,
      ]);
    },
  });
  function submit(event: FormEvent) {
    event.preventDefault();
    save.mutate();
  }

  const row = (key: string, label: string) => {
    const it = state(key, label);
    return (
      <li key={key} className="mt-3">
        <label className="flex w-fit items-center gap-2 font-medium">
          <Checkbox
            checked={it.on}
            aria-label={`Collect ${label}`}
            onChange={(e) => set(key, label, { on: e.target.checked })}
          />
          {label}
        </label>
        {it.on ? (
          <div className="ms-7 mt-1 flex flex-wrap items-center gap-3">
            <input
              className="min-h-(--control-h) rounded-sm border border-border-strong bg-surface px-3 text-fg"
              aria-label={`${label} label`}
              maxLength={100}
              value={it.label}
              onChange={(e) => set(key, label, { label: e.target.value })}
            />
            <label className="flex items-center gap-2">
              <Checkbox
                checked={it.required}
                aria-label={`Require ${label}`}
                onChange={(e) => set(key, label, { required: e.target.checked })}
              />
              Required
            </label>
          </div>
        ) : null}
      </li>
    );
  };

  return (
    <form onSubmit={submit} noValidate>
      <Field
        id="form-name"
        label="Name"
        autoComplete="off"
        required
        maxLength={100}
        hint="Yours, for this screen. Nobody filling the form in sees it."
        value={name}
        onChange={(e) => setName(e.target.value)}
      />
      <Field
        id="form-slug"
        label="URL slug"
        autoComplete="off"
        required
        maxLength={63}
        hint="Lowercase letters, digits and hyphens. It names the form in its address."
        value={slug}
        onChange={(e) => setSlug(e.target.value)}
      />
      <Field
        id="form-headline"
        label="Headline"
        autoComplete="off"
        required
        maxLength={200}
        value={headline}
        onChange={(e) => setHeadline(e.target.value)}
      />
      <Field
        id="form-button"
        label="Button label"
        autoComplete="off"
        required
        maxLength={100}
        value={button}
        onChange={(e) => setButton(e.target.value)}
      />
      <Field
        id="form-redirect"
        label="Redirect after signing up"
        type="url"
        autoComplete="off"
        maxLength={2000}
        placeholder="https://example.com/thanks"
        hint="Optional, and an absolute http or https address. Left empty, the hosted page says thank you itself."
        value={redirect}
        onChange={(e) => setRedirect(e.target.value)}
      />

      <fieldset className="mt-6">
        <legend className="font-medium">Lists</legend>
        <p className="text-sm text-fg-muted">
          One submission subscribes to every list ticked here. Whether a membership starts pending
          follows each list&rsquo;s own double opt-in setting, not the form&rsquo;s.
        </p>
        {(allLists.data ?? []).length === 0 ? (
          <p className="mt-2 text-fg-muted">This workspace has no lists yet.</p>
        ) : (
          <ul>
            {(allLists.data ?? []).map((l) => (
              <li key={l.id} className="mt-2">
                <label className="flex w-fit items-center gap-2">
                  <Checkbox
                    checked={lists.includes(l.id)}
                    onChange={(e) =>
                      setLists((was) =>
                        e.target.checked ? [...was, l.id] : was.filter((id) => id !== l.id),
                      )
                    }
                  />
                  <span className="wrap-anywhere">{l.name}</span>
                </label>
              </li>
            ))}
          </ul>
        )}
      </fieldset>

      <fieldset className="mt-6">
        <legend className="font-medium">Fields</legend>
        <ul>
          <li>
            <span className="font-medium">Email address</span>
            <div className="ms-0 mt-1 flex flex-wrap items-center gap-3">
              <input
                className="min-h-(--control-h) rounded-sm border border-border-strong bg-surface px-3 text-fg"
                aria-label="Email address label"
                maxLength={100}
                value={emailLabel}
                onChange={(e) => setEmailLabel(e.target.value)}
              />
              <span className="text-sm text-fg-muted">Always collected, always required.</span>
            </div>
          </li>
          {BUILT_IN.map((b) => row(b.key, b.label))}
          {custom.map((d) => row(d.key, d.label))}
        </ul>
      </fieldset>

      {save.error ? <Alert>{save.error.message}</Alert> : null}
      <div className="mt-6 flex flex-wrap justify-end gap-3">
        <Button onClick={onCancel}>Cancel</Button>
        <Button type="submit" variant="solid" busy={save.isPending}>
          {existing ? "Save form" : "Create form"}
        </Button>
      </div>
    </form>
  );
}
