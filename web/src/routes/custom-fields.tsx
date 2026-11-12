// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";

import { api, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Badge } from "../components/badge";
import { Button } from "../components/button";
import { ConfirmDialog } from "../components/confirm-dialog";
import { Dialog } from "../components/dialog";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Alert, Field } from "../components/field";
import { CONTROL } from "../components/input";
import { Select } from "../components/select";
import { Skeleton } from "../components/skeleton";
import { atLeast } from "../lib/roles";
import { useWorkspace } from "./workspace-layout";

type Definition = components["schemas"]["Definition"];
type FieldKind = components["schemas"]["FieldKind"];

/** In the order the picker offers them. */
const KINDS: [FieldKind, string][] = [
  ["text", "Text"],
  ["number", "Number"],
  ["date", "Date"],
  ["datetime", "Date and time"],
  ["boolean", "Yes or no"],
  ["select", "Single choice"],
  ["multi_select", "Multiple choice"],
];
const kindLabel = (k: FieldKind) => KINDS.find(([v]) => v === k)?.[1] ?? k;
const hasOptions = (k: FieldKind) => k === "select" || k === "multi_select";

const th = "py-2 pe-3 text-start font-medium";
const KNOWN = {
  "insufficient-role": "Your role cannot change custom fields. It takes admin.",
  "key-taken": "A field with that key already exists, even an archived one. Keys are never reused.",
  "field-not-archived": "This field is not archived, so its values cannot be purged. Archive it first.",
};

export default function CustomFields() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return <Fields ws={workspace.id} admin={atLeast(workspace.role, "admin")} />;
}

function Fields({ ws, admin }: { ws: string; admin: boolean }) {
  const qc = useQueryClient();
  // The same key and shape as the contact page's query, so the two share one cache entry.
  const fields = useQuery({
    queryKey: ["custom-fields", ws],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/custom-fields", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      if (!data) throw new Error(problemText(error, KNOWN, "The custom fields could not be loaded."));
      // shortcut: the first 200 fields only; add "Load more" when a workspace has more.
      return data.data;
    },
    retry: false,
  });
  const refresh = () => qc.invalidateQueries({ queryKey: ["custom-fields", ws] });

  // `null` is closed; `"new"` is the create form; a field is the edit form for it.
  const [form, setForm] = useState<Definition | "new" | null>(null);
  const [archiving, setArchiving] = useState<Definition | null>(null);
  const [purging, setPurging] = useState<Definition | null>(null);
  const [purged, setPurged] = useState("");

  const archive = useMutation({
    mutationFn: async (field: Definition) => {
      const { response, error } = await api.DELETE(
        "/api/v1/workspaces/{workspace_id}/custom-fields/{id}",
        { params: { path: { workspace_id: ws, id: field.id } } },
      );
      if (!response.ok) throw new Error(problemText(error, KNOWN, "The field could not be archived."));
    },
    onSuccess: async () => {
      setArchiving(null);
      await refresh();
    },
  });

  const purge = useMutation({
    mutationFn: async (field: Definition) => {
      const { data, error } = await api.POST(
        "/api/v1/workspaces/{workspace_id}/custom-fields/{id}/purge",
        { params: { path: { workspace_id: ws, id: field.id } } },
      );
      if (!data) throw new Error(problemText(error, KNOWN, "The values could not be purged."));
      return { field, affected: data.affected };
    },
    onSuccess: async ({ field, affected }) => {
      setPurging(null);
      setPurged(
        `Removed the ${field.label} value from ${affected.toLocaleString()} contact${affected === 1 ? "" : "s"}.`,
      );
      await refresh();
    },
  });

  return (
    <section className="mt-8">
      <div className="flex flex-wrap items-baseline justify-between gap-3">
        <h2 className="text-xl font-semibold">Custom fields</h2>
        {admin ? (
          <Button variant="solid" onClick={() => setForm("new")}>
            New field
          </Button>
        ) : null}
      </div>
      <p className="mt-2 text-fg-muted">
        Archiving a field never removes what contacts already have: it only stops the field being
        offered and refuses new values. Purging the values is a separate step, and only for an
        archived field.
        {admin ? "" : " Changing fields takes an admin."}
      </p>
      {purged ? (
        <p role="status" className="mt-3 font-medium">
          {purged}
        </p>
      ) : null}

      <div className="mt-4">
        {fields.isPending ? (
          <Skeleton label="Loading custom fields" rows={5} />
        ) : fields.isError ? (
          <ErrorState title="The custom fields could not be loaded." onRetry={() => void fields.refetch()}>
            {fields.error.message}
          </ErrorState>
        ) : fields.data.length === 0 ? (
          <EmptyState title="No custom fields yet">Fields you create will appear here.</EmptyState>
        ) : (
          <table className="w-full">
            <caption className="sr-only">Custom fields</caption>
            <thead>
              <tr className="border-b border-border">
                <th scope="col" className={th}>
                  Label
                </th>
                <th scope="col" className={th}>
                  Key
                </th>
                <th scope="col" className={th}>
                  Type
                </th>
                <th scope="col" className={th}>
                  Options
                </th>
                <th scope="col" className={th}>
                  Status
                </th>
                {admin ? (
                  <th scope="col" className={th}>
                    <span className="sr-only">Actions</span>
                  </th>
                ) : null}
              </tr>
            </thead>
            <tbody>
              {fields.data.map((f) => (
                <tr key={f.id} className="border-b border-border">
                  <td className="py-2 pe-3">{f.label}</td>
                  <td className="py-2 pe-3">
                    <code>{f.key}</code>
                  </td>
                  <td className="py-2 pe-3">{kindLabel(f.kind)}</td>
                  <td className="py-2 pe-3">{f.options.length ? f.options.join(", ") : "—"}</td>
                  <td className="py-2 pe-3">
                    {f.archived_at ? (
                      <Badge variant="warning" glyph="ring">
                        Archived
                      </Badge>
                    ) : (
                      <Badge variant="success" glyph="dot">
                        Active
                      </Badge>
                    )}
                  </td>
                  {admin ? (
                    <td className="py-2 text-end">
                      {f.archived_at ? (
                        <Button
                          variant="danger-ghost"
                          aria-label={`Purge values of ${f.label}`}
                          onClick={() => setPurging(f)}
                        >
                          Purge values
                        </Button>
                      ) : (
                        <>
                          <Button
                            variant="ghost"
                            aria-label={`Edit ${f.label}`}
                            onClick={() => setForm(f)}
                          >
                            Edit
                          </Button>
                          <Button
                            variant="danger-ghost"
                            aria-label={`Archive ${f.label}`}
                            onClick={() => setArchiving(f)}
                          >
                            Archive
                          </Button>
                        </>
                      )}
                    </td>
                  ) : null}
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      <Dialog
        open={form !== null}
        onClose={() => setForm(null)}
        title={form === "new" || form === null ? "New custom field" : `Edit the field ${form.label}`}
      >
        {form !== null ? (
          <FieldForm
            ws={ws}
            existing={form === "new" ? undefined : form}
            onCancel={() => setForm(null)}
            onDone={async () => {
              setForm(null);
              await refresh();
            }}
          />
        ) : null}
      </Dialog>

      <ConfirmDialog
        open={archiving !== null}
        onClose={() => {
          setArchiving(null);
          archive.reset();
        }}
        title={`Archive the field ${archiving?.label ?? ""}`}
        confirmLabel="Archive field"
        busy={archive.isPending}
        error={archive.error?.message}
        onConfirm={() => archiving && archive.mutate(archiving)}
      >
        <p>
          Archiving stops this field appearing in forms and refuses new values for it. Nothing is
          removed.
        </p>
        <p>
          <strong>Every contact keeps the value it already has</strong>, and those values are still
          readable. New values are refused.
        </p>
        <p>
          Removing the stored values is a separate action, Purge values, offered only once a field
          is archived.
        </p>
      </ConfirmDialog>

      <ConfirmDialog
        open={purging !== null}
        onClose={() => {
          setPurging(null);
          purge.reset();
        }}
        title={`Purge the values of ${purging?.label ?? ""}`}
        confirmLabel="Purge values permanently"
        busy={purge.isPending}
        error={purge.error?.message}
        onConfirm={() => purging && purge.mutate(purging)}
      >
        <p>
          This removes the {purging?.label} value from every contact that has one.{" "}
          <strong>It cannot be undone.</strong>
        </p>
        <p>
          The field stays archived and no contact is deleted; only this field&rsquo;s stored values
          are lost.
        </p>
      </ConfirmDialog>
    </section>
  );
}

function FieldForm({
  ws,
  existing,
  onCancel,
  onDone,
}: {
  ws: string;
  existing: Definition | undefined;
  onCancel: () => void;
  onDone: () => Promise<void>;
}) {
  const [label, setLabel] = useState(existing?.label ?? "");
  const [key, setKey] = useState("");
  const [kind, setKind] = useState<FieldKind>("text");
  const [options, setOptions] = useState(existing?.options.join("\n") ?? "");
  const shownKind = existing?.kind ?? kind;
  const list = options.split("\n").map((o) => o.trim()).filter(Boolean);

  const save = useMutation({
    mutationFn: async () => {
      const body = { label: label.trim(), options: hasOptions(shownKind) ? list : undefined };
      const { data, error } = existing
        ? await api.PATCH("/api/v1/workspaces/{workspace_id}/custom-fields/{id}", {
            params: { path: { workspace_id: ws, id: existing.id } },
            body,
          })
        : await api.POST("/api/v1/workspaces/{workspace_id}/custom-fields", {
            params: { path: { workspace_id: ws } },
            body: { ...body, key, kind },
          });
      if (!data) throw new Error(problemText(error, KNOWN, "The field could not be saved."));
    },
    onSuccess: onDone,
  });
  function submit(event: FormEvent) {
    event.preventDefault();
    save.mutate();
  }

  return (
    <form onSubmit={submit}>
      <Field id="field-label" label="Label" value={label} onChange={(e) => setLabel(e.target.value)} />
      {existing ? (
        <p className="mt-4">
          <span className="font-medium">Key</span> <code>{existing.key}</code>, type{" "}
          {kindLabel(existing.kind)}. Neither can be changed: converting every contact&rsquo;s
          stored value is not supported yet.
        </p>
      ) : (
        <>
          <Field
            id="field-key"
            label="Key"
            value={key}
            onChange={(e) => setKey(e.target.value)}
            hint="Lowercase letters, digits and underscores, starting with a letter. It cannot change afterwards."
          />
          <div className="mt-4">
            <label className="block font-medium" htmlFor="field-kind">
              Type
            </label>
            <Select
              id="field-kind"
              className="mt-1 w-full"
              value={kind}
              onChange={(e) => setKind(e.target.value as FieldKind)}
            >
              {KINDS.map(([value, name]) => (
                <option key={value} value={value}>
                  {name}
                </option>
              ))}
            </Select>
          </div>
        </>
      )}
      {hasOptions(shownKind) ? (
        <div className="mt-4">
          <label className="block font-medium" htmlFor="field-options">
            Options
          </label>
          <textarea
            id="field-options"
            rows={4}
            className={`${CONTROL} mt-1 w-full py-2`}
            value={options}
            onChange={(e) => setOptions(e.target.value)}
            aria-describedby="field-options-hint"
          />
          <span id="field-options-hint" className="mt-1 block text-sm text-fg-muted">
            One per line. At least one, and no repeats.
          </span>
        </div>
      ) : null}
      {save.error ? <Alert>{save.error.message}</Alert> : null}
      <div className="mt-6 flex justify-end gap-3">
        <Button onClick={onCancel}>Cancel</Button>
        <Button type="submit" variant="solid" busy={save.isPending}>
          {existing ? "Save" : "Create field"}
        </Button>
      </div>
    </form>
  );
}
