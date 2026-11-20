// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "@tanstack/react-router";
import { useState } from "react";

import { api, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Button } from "../components/button";
import { ConfirmDialog } from "../components/confirm-dialog";
import { Dialog } from "../components/dialog";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { CONTROL } from "../components/input";
import { Skeleton } from "../components/skeleton";
import { atLeast } from "../lib/roles";
import { CopyButton } from "../lib/ui";
import { type Form, FormEditor, publicPath } from "./forms";
import { useWorkspace } from "./workspace-layout";

type FormTarget = components["schemas"]["FormTarget"];

const link = "text-accent-text underline";
const KNOWN = {
  "insufficient-role": "Your role cannot change forms. It takes editor or above.",
};

export default function FormDetail() {
  const { workspace } = useWorkspace();
  const { formId } = useParams({ from: "/w/$slug/forms/$formId" });
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return (
    <Detail
      ws={workspace.id}
      slug={workspace.slug}
      formId={formId}
      canEdit={atLeast(workspace.role, "editor")}
    />
  );
}

function Detail({
  ws,
  slug,
  formId,
  canEdit,
}: {
  ws: string;
  slug: string;
  formId: string;
  canEdit: boolean;
}) {
  const form = useQuery({
    queryKey: ["form", ws, formId],
    queryFn: async () => {
      const { data, response } = await api.GET("/api/v1/workspaces/{workspace_id}/forms/{id}", {
        params: { path: { workspace_id: ws, id: formId } },
      });
      if (response.status === 404) return null;
      if (!data) throw new Error("The form could not be loaded.");
      return data;
    },
    retry: false,
  });
  return (
    <section className="mt-8">
      <Link to="/w/$slug/forms" params={{ slug }} className={link}>
        All forms
      </Link>
      {form.isPending ? (
        <div className="mt-4">
          <Skeleton label="Loading form" rows={3} />
        </div>
      ) : form.isError ? (
        <div className="mt-4">
          <ErrorState title="The form could not be loaded." onRetry={() => void form.refetch()}>
            {form.error.message}
          </ErrorState>
        </div>
      ) : form.data === null ? (
        <EmptyState title="Form not found">
          It may have been deleted, or it belongs to another workspace.
        </EmptyState>
      ) : (
        <>
          <h2 className="mt-4 text-xl font-semibold wrap-anywhere">{form.data.name}</h2>
          {canEdit ? <Actions ws={ws} slug={slug} form={form.data} /> : null}
          <Settings ws={ws} slug={slug} form={form.data} />
          <Embed slug={slug} form={form.data} />
        </>
      )}
    </section>
  );
}

function Actions({ ws, slug, form }: { ws: string; slug: string; form: Form }) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);

  const remove = useMutation({
    mutationFn: async () => {
      const { response, error } = await api.DELETE(
        "/api/v1/workspaces/{workspace_id}/forms/{id}",
        { params: { path: { workspace_id: ws, id: form.id } } },
      );
      if (!response.ok) throw new Error(problemText(error, KNOWN, "The form could not be deleted."));
    },
    onSuccess: async () => {
      // Leaving first: this page is still mounted, and invalidating its query here would refetch
      // a form from a URL that now answers 404.
      await navigate({ to: "/w/$slug/forms", params: { slug } });
      qc.removeQueries({ queryKey: ["form", ws, form.id] });
      await qc.invalidateQueries({ queryKey: ["forms", ws] });
    },
  });

  return (
    <div className="mt-4 flex flex-wrap gap-2">
      <Button onClick={() => setEditing(true)}>Edit form</Button>
      <Button variant="danger-ghost" onClick={() => setDeleting(true)}>
        Delete form
      </Button>

      <Dialog open={editing} onClose={() => setEditing(false)} title="Edit form">
        <FormEditor
          ws={ws}
          existing={form}
          onCancel={() => setEditing(false)}
          onDone={() => setEditing(false)}
        />
      </Dialog>

      <ConfirmDialog
        open={deleting}
        onClose={() => {
          setDeleting(false);
          remove.reset();
        }}
        title="Delete this form"
        confirmLabel="Delete form"
        busy={remove.isPending}
        error={remove.error?.message}
        onConfirm={() => remove.mutate()}
      >
        <p>
          <strong className="wrap-anywhere">{form.name}</strong> stops answering, and any snippet
          already pasted on another site stops working. Its lists, their members and everyone who
          has already signed up are untouched.
        </p>
        <p>
          The consent rows this form wrote stay, and keep pointing at it by id. They still say
          somebody signed up; what the form looked like at the time is in the audit log.
        </p>
        <p>This cannot be undone.</p>
      </ConfirmDialog>
    </div>
  );
}

function Settings({ ws, slug, form }: { ws: string; slug: string; form: Form }) {
  const lists = useQuery({
    queryKey: ["lists", ws],
    queryFn: async () => {
      const { data } = await api.GET("/api/v1/workspaces/{workspace_id}/lists", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      return data?.data ?? [];
    },
  });
  const named = form.list_ids.map(
    (id) => lists.data?.find((l) => l.id === id)?.name ?? "A list this workspace no longer has",
  );
  return (
    <section aria-labelledby="form-settings" className="mt-6">
      <h3 id="form-settings" className="text-lg font-semibold">
        Settings
      </h3>
      <dl className="mt-3 grid grid-cols-[max-content_1fr] gap-x-6 gap-y-1">
        <dt className="text-fg-muted">Address</dt>
        <dd className="wrap-anywhere">
          <a className={link} href={publicPath(slug, form.slug)}>
            {publicPath(slug, form.slug)}
          </a>
        </dd>
        <dt className="text-fg-muted">Headline</dt>
        <dd className="wrap-anywhere">{form.headline}</dd>
        <dt className="text-fg-muted">Button</dt>
        <dd className="wrap-anywhere">{form.button_label}</dd>
        <dt className="text-fg-muted">Redirect</dt>
        <dd className="wrap-anywhere">{form.redirect_url || "—"}</dd>
        <dt className="text-fg-muted">Lists</dt>
        <dd className="wrap-anywhere">{named.join(", ") || "—"}</dd>
        <dt className="text-fg-muted">Collects</dt>
        <dd className="wrap-anywhere">
          {form.fields.map((f) => `${f.label}${f.required ? " (required)" : ""}`).join(", ")}
        </dd>
      </dl>
    </section>
  );
}

/**
 * The embed snippet, read-only, with a copy button.
 *
 * **The `action` is built from the workspace on screen and the form on screen**, so a snippet for
 * the wrong tenant cannot be produced here: `slug` is `useWorkspace()`'s, the same record whose
 * `id` fetched this form, and `form.slug` came back with it. A form slug is unique per workspace
 * and not globally, so a snippet missing the workspace — or carrying someone else's — would post
 * a visitor's address into another tenant's list, and nothing on the operator's own page would
 * show it.
 *
 * `window.location.origin` rather than a configured base: the operator reached this screen on the
 * address their subscribers will reach too, and a second place to spell it is a second place for
 * it to be wrong.
 */
function Embed({ slug, form }: { slug: string; form: Form }) {
  const html = snippet(window.location.origin + publicPath(slug, form.slug), form);
  return (
    <section aria-labelledby="form-embed" className="mt-6">
      <h3 id="form-embed" className="text-lg font-semibold">
        Embed on your own site
      </h3>
      <p className="mt-1 text-fg-muted">
        Plain HTML: it needs no JavaScript, and it posts straight to the address above. Editing the
        form here changes this snippet, so copy it again after a change.
      </p>
      <label className="mt-3 block font-medium" htmlFor="form-snippet">
        Embed snippet
      </label>
      <textarea
        id="form-snippet"
        readOnly
        rows={Math.min(20, html.split("\n").length)}
        spellCheck={false}
        className={`${CONTROL} mt-1 w-full py-2 font-mono text-sm`}
        value={html}
      />
      <div className="mt-2 flex flex-wrap items-center gap-3">
        <CopyButton value={html} />
      </div>
    </section>
  );
}

/** `& < > " '` escaped: every value below reaches element text or a quoted attribute value. */
function esc(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/**
 * The input `name` and `type` for one target. **This mirrors `input` in `src/public/forms.rs`**,
 * which is the contract `POST /f/{workspace}/{slug}` reads: a name spelt differently here is a
 * box on somebody's site that collects nothing. `web/e2e/forms.spec.ts` submits a generated
 * snippet to the real route, which is what holds the two together.
 */
function nameOf(target: FormTarget): [name: string, type: string, autocomplete: string] {
  switch (target.kind) {
    case "email":
      return ["email", "email", "email"];
    case "first_name":
      return ["first_name", "text", "given-name"];
    case "last_name":
      return ["last_name", "text", "family-name"];
    case "timezone":
      return ["timezone", "text", "off"];
    default:
      return [`custom.${target.value}`, "text", "off"];
  }
}

/**
 * The snippet itself.
 *
 * It carries the honeypot the hosted page carries, because the route checks it either way: a
 * submission with `website` filled in is discarded, and an embedder who leaves it out gets no
 * spam protection at all. Its label says what to do with it, for the sites whose stylesheet does
 * not hide it — a visible "Website" box that silently discards the submission is the one failure
 * this control must not produce.
 */
function snippet(action: string, form: Form): string {
  const lines = [`<form method="post" action="${esc(action)}">`];
  for (const field of form.fields) {
    const [name, type, autocomplete] = nameOf(field.target);
    const id = `nm-${name}`;
    lines.push(
      "  <p>",
      `    <label for="${esc(id)}">${esc(field.label)}</label><br>`,
      `    <input id="${esc(id)}" name="${esc(name)}" type="${type}" ` +
        `autocomplete="${autocomplete}"${field.required ? " required" : ""}>`,
      "  </p>",
    );
  }
  lines.push(
    '  <div aria-hidden="true" style="position:absolute;left:-10000px">',
    '    <label for="nm-website">Leave this field empty</label>',
    '    <input id="nm-website" name="website" type="text" tabindex="-1" autocomplete="off">',
    "  </div>",
    `  <button type="submit">${esc(form.button_label)}</button>`,
    "</form>",
  );
  return lines.join("\n");
}
