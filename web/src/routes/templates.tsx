// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";

import { api, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Button } from "../components/button";
import { ConfirmDialog } from "../components/confirm-dialog";
import { Dialog } from "../components/dialog";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Alert, Field } from "../components/field";
import { Select } from "../components/select";
import { Skeleton } from "../components/skeleton";
import { TableScroller } from "../components/table-scroller";
import { atLeast } from "../lib/roles";
import { useWorkspace } from "./workspace-layout";

export type Template = components["schemas"]["TemplateView"];
export type Kind = components["schemas"]["Kind"];

const day = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });
const link = "text-accent-text underline";
const th = "py-2 pe-3 text-start font-medium";

/** What each kind is, in the operator's words rather than the column's. */
export const KINDS: [Kind, string][] = [
  ["blocks", "Blocks — built in the editor"],
  ["mjml", "MJML — written by hand"],
  ["html", "HTML — written by hand"],
];
export const kindName = (k: Kind) => KINDS.find(([v]) => v === k)?.[1] ?? k;

export const TEMPLATE_PROBLEMS = {
  "insufficient-role": "Your role cannot change templates. It takes editor or above.",
  "not-found": "This template no longer exists. It may have been deleted in another window.",
};

export default function Templates() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return (
    <AllTemplates
      ws={workspace.id}
      slug={workspace.slug}
      canEdit={atLeast(workspace.role, "editor")}
    />
  );
}

/**
 * shortcut: the first 200 templates, newest first, with no cursor. The route pages; add a "Load
 * more" the day a workspace keeps more than that.
 */
const PAGE = 200;

function AllTemplates({ ws, slug, canEdit }: { ws: string; slug: string; canEdit: boolean }) {
  const qc = useQueryClient();
  const [creating, setCreating] = useState(false);
  const [deleting, setDeleting] = useState<Template | null>(null);

  const templates = useQuery({
    queryKey: ["templates", ws],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/templates", {
        params: { path: { workspace_id: ws }, query: { limit: PAGE } },
      });
      if (!data) {
        throw new Error(
          problemText(error, TEMPLATE_PROBLEMS, "The templates could not be loaded."),
        );
      }
      return data.data;
    },
    retry: false,
  });

  const remove = useMutation({
    mutationFn: async (template: Template) => {
      const { response, error } = await api.DELETE(
        "/api/v1/workspaces/{workspace_id}/templates/{id}",
        { params: { path: { workspace_id: ws, id: template.id } } },
      );
      if (!response.ok) {
        throw new Error(
          problemText(error, TEMPLATE_PROBLEMS, "The template could not be deleted."),
        );
      }
    },
    onSuccess: async () => {
      setDeleting(null);
      await qc.invalidateQueries({ queryKey: ["templates", ws] });
    },
  });

  return (
    <section className="mt-8">
      <div className="flex flex-wrap items-baseline justify-between gap-3">
        <h2 className="text-xl font-semibold">Templates</h2>
        {canEdit ? (
          <Button variant="solid" onClick={() => setCreating(true)}>
            New template
          </Button>
        ) : null}
      </div>
      <div className="mt-4">
        {templates.isPending ? (
          <Skeleton label="Loading templates" rows={5} />
        ) : templates.isError ? (
          <ErrorState
            title="The templates could not be loaded."
            onRetry={() => void templates.refetch()}
          >
            {templates.error.message}
          </ErrorState>
        ) : templates.data.length === 0 ? (
          <EmptyState title="No templates yet">
            A template is the email itself: its blocks, or the MJML or HTML you wrote. The brand
            kit decides how it looks.
          </EmptyState>
        ) : (
          <TableScroller label="Templates table">
            <table className="w-full">
              <caption className="sr-only">Templates</caption>
              <thead>
                <tr className="border-b border-border">
                  <th scope="col" className={th}>
                    Name
                  </th>
                  <th scope="col" className={th}>
                    Kind
                  </th>
                  <th scope="col" className={th}>
                    Changed
                  </th>
                  {canEdit ? (
                    <th scope="col" className={th}>
                      <span className="sr-only">Actions</span>
                    </th>
                  ) : null}
                </tr>
              </thead>
              <tbody>
                {templates.data.map((t) => (
                  <tr key={t.id} className="border-b border-border">
                    <td className="py-2 pe-3 wrap-anywhere">
                      <Link
                        to="/w/$slug/templates/$templateId"
                        params={{ slug, templateId: t.id }}
                        className={link}
                      >
                        {t.name}
                      </Link>
                    </td>
                    <td className="py-2 pe-3">{kindName(t.kind)}</td>
                    <td className="py-2 pe-3 tabular-nums text-fg-muted">
                      {day.format(new Date(t.updated_at))}
                    </td>
                    {canEdit ? (
                      <td className="py-2">
                        <Button
                          aria-label={`Delete ${t.name}`}
                          onClick={() => {
                            remove.reset();
                            setDeleting(t);
                          }}
                        >
                          Delete
                        </Button>
                      </td>
                    ) : null}
                  </tr>
                ))}
              </tbody>
            </table>
          </TableScroller>
        )}
      </div>

      <Dialog open={creating} onClose={() => setCreating(false)} title="New template">
        <NewTemplateForm ws={ws} slug={slug} onCancel={() => setCreating(false)} />
      </Dialog>

      <ConfirmDialog
        open={deleting !== null}
        onClose={() => setDeleting(null)}
        title={`Delete ${deleting?.name ?? ""}?`}
        confirmLabel="Delete template"
        busy={remove.isPending}
        error={remove.error?.message}
        onConfirm={() => deleting && remove.mutate(deleting)}
      >
        <p>
          The template and both of its compiled renditions are removed. Emails already sent are
          not affected.
        </p>
      </ConfirmDialog>
    </section>
  );
}

/**
 * The least MJML and HTML that compiles. The API requires `source` for those two kinds and
 * refuses an empty one, so a new hand-written template has to start as something.
 */
const STARTER_MJML =
  "<mjml><mj-body><mj-section><mj-column><mj-text>Hello</mj-text></mj-column></mj-section></mj-body></mjml>";
const STARTER_HTML = "<p>Hello</p>";

/**
 * The create form. `kind` is chosen once and never again: it decides which column holds the
 * document, so the API refuses it in a patch — changing it is a different template, not an edit.
 */
function NewTemplateForm({
  ws,
  slug,
  onCancel,
}: {
  ws: string;
  slug: string;
  onCancel: () => void;
}) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [kind, setKind] = useState<Kind>("blocks");

  const create = useMutation({
    mutationFn: async () => {
      const { data, error } = await api.POST("/api/v1/workspaces/{workspace_id}/templates", {
        params: { path: { workspace_id: ws } },
        body:
          kind === "blocks"
            ? { name, kind, tree: { blocks: [] } }
            : { name, kind, source: kind === "mjml" ? STARTER_MJML : STARTER_HTML },
      });
      if (!data) {
        throw new Error(
          problemText(error, TEMPLATE_PROBLEMS, "The template could not be created."),
        );
      }
      return data;
    },
    onSuccess: async (template) => {
      await qc.invalidateQueries({ queryKey: ["templates", ws] });
      await navigate({
        to: "/w/$slug/templates/$templateId",
        params: { slug, templateId: template.id },
      });
    },
  });

  return (
    <form
      className="mt-3"
      onSubmit={(e: FormEvent) => {
        e.preventDefault();
        // `busy` does not disable the button, so a second press must be ignored here.
        if (!create.isPending) create.mutate();
      }}
    >
      <Field
        id="template-name"
        label="Name"
        type="text"
        autoComplete="off"
        value={name}
        onChange={(e) => setName(e.target.value)}
      />
      <div className="mt-4">
        <label className="block font-medium" htmlFor="template-kind">
          Kind
        </label>
        <Select
          id="template-kind"
          className="mt-1"
          value={kind}
          onChange={(e) => setKind(e.target.value as Kind)}
          aria-describedby="template-kind-hint"
        >
          {KINDS.map(([value, label]) => (
            <option key={value} value={value}>
              {label}
            </option>
          ))}
        </Select>
        <span id="template-kind-hint" className="mt-1 block text-sm text-fg-muted">
          This cannot be changed later.
        </span>
      </div>
      {create.error ? <Alert>{create.error.message}</Alert> : null}
      <div className="mt-6 flex flex-wrap justify-end gap-3">
        <Button onClick={onCancel}>Cancel</Button>
        <Button type="submit" variant="solid" busy={create.isPending}>
          Create template
        </Button>
      </div>
    </form>
  );
}
