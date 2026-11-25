// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "@tanstack/react-router";
import { useMemo, useState } from "react";

import { api, problemText } from "../api/client";
import { BlockEditor } from "../components/block-editor/editor";
import type { Doc } from "../components/block-editor/model";
import { templatePreview } from "../components/block-editor/preview-source";
import { Button } from "../components/button";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Alert, Field } from "../components/field";
import { CONTROL } from "../components/input";
import { Skeleton } from "../components/skeleton";
import { atLeast } from "../lib/roles";
import { type Template, TEMPLATE_PROBLEMS, kindName } from "./templates";
import { useWorkspace } from "./workspace-layout";

const link = "text-accent-text underline";

/**
 * The refusals a save can get, on top of the two the list screen names.
 *
 * `template-stale` and `validation` keep the server's own `detail`: it names which rendition went
 * stale, or which merge tag will not compile and the field it names, and this screen could not
 * say either. Every stale variant's sentence already ends in the one cure — open it and save it
 * again — which is what the operator is doing when they read it.
 */
const SAVE_PROBLEMS = {
  ...TEMPLATE_PROBLEMS,
  "template-uncheckable": "This template could not be checked, so nothing has been checked.",
};

export default function TemplateDetail() {
  const { workspace } = useWorkspace();
  const { templateId } = useParams({ from: "/w/$slug/templates/$templateId" });
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return (
    <Detail
      ws={workspace.id}
      slug={workspace.slug}
      templateId={templateId}
      canEdit={atLeast(workspace.role, "editor")}
    />
  );
}

function Detail({
  ws,
  slug,
  templateId,
  canEdit,
}: {
  ws: string;
  slug: string;
  templateId: string;
  canEdit: boolean;
}) {
  const template = useQuery({
    queryKey: ["template", ws, templateId],
    queryFn: async () => {
      const { data, error, response } = await api.GET(
        "/api/v1/workspaces/{workspace_id}/templates/{id}",
        { params: { path: { workspace_id: ws, id: templateId } } },
      );
      // Another workspace's template is a 404 and never the data, so there is one absent state
      // here and it says nothing about whether the id exists somewhere else.
      if (response.status === 404) return null;
      if (!data) {
        throw new Error(problemText(error, TEMPLATE_PROBLEMS, "The template could not be loaded."));
      }
      return data;
    },
    retry: false,
  });

  return (
    <section className="mt-8">
      <Link to="/w/$slug/templates" params={{ slug }} className={link}>
        All templates
      </Link>
      {template.isPending ? (
        <div className="mt-4">
          <Skeleton label="Loading template" rows={4} />
        </div>
      ) : template.isError ? (
        <div className="mt-4">
          <ErrorState
            title="The template could not be loaded."
            onRetry={() => void template.refetch()}
          >
            {template.error.message}
          </ErrorState>
        </div>
      ) : template.data === null ? (
        <div className="mt-4">
          <EmptyState title="Template not found">
            It may have been deleted, or it belongs to another workspace.
          </EmptyState>
        </div>
      ) : (
        // Keyed by the template, so opening another one starts a fresh editor rather than
        // keeping the first one's unsaved document under the second one's name.
        <Edit key={template.data.id} ws={ws} template={template.data} canEdit={canEdit} />
      )}
    </section>
  );
}

function Edit({ ws, template, canEdit }: { ws: string; template: Template; canEdit: boolean }) {
  const qc = useQueryClient();
  const [name, setName] = useState(template.name);
  /**
   * The document being edited. **It starts from the server's copy, never from the URL**: a
   * `?doc=` affordance exists in the e2e editor harness for fixtures and is confined to it. A
   * document from the address bar is a document anyone can put in a link.
   */
  const [doc, setDoc] = useState<Doc>(() => template.tree ?? { blocks: [] });
  const [source, setSource] = useState(template.source ?? "");
  // Bound once per template: `Preview` re-asks the server whenever this identity changes, so a
  // fresh closure on every keystroke would be a request on every keystroke.
  const preview = useMemo(() => templatePreview(ws, template.id), [ws, template.id]);

  const save = useMutation({
    mutationFn: async () => {
      // `kind` is not patchable, and the API refuses the column this template does not use.
      const { data, error } = await api.PATCH(
        "/api/v1/workspaces/{workspace_id}/templates/{id}",
        {
          params: { path: { workspace_id: ws, id: template.id } },
          body: template.kind === "blocks" ? { name, tree: doc } : { name, source },
        },
      );
      if (!data) {
        throw new Error(problemText(error, SAVE_PROBLEMS, "The template could not be saved."));
      }
      return data;
    },
    onSuccess: async (saved) => {
      qc.setQueryData(["template", ws, saved.id], saved);
      await qc.invalidateQueries({ queryKey: ["templates", ws] });
    },
  });

  return (
    <div className="mt-4">
      <h2 className="text-xl font-semibold wrap-anywhere">{template.name}</h2>
      <p className="mt-1 text-fg-muted">{kindName(template.kind)}</p>

      {!canEdit ? (
        // shortcut: a viewer reads the name and the kind and no more. The editor has no
        // read-only mode, and showing its controls to somebody whose save the server will
        // refuse is a trap. Give this screen the stored preview when a viewer needs the email
        // itself — `POST …/preview` with an empty body is that, and takes viewer.
        <p className="mt-4 text-fg-muted">
          Your role can read templates but not change them. It takes editor or above.
        </p>
      ) : (
        <>
          <div className="mt-4 max-w-xl">
            <Field
              id="template-name"
              label="Name"
              type="text"
              autoComplete="off"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </div>

          {template.kind === "blocks" ? (
            <div className="mt-6">
              <BlockEditor initial={doc} source={preview} onChange={setDoc} />
            </div>
          ) : (
            <div className="mt-6">
              <label className="block font-medium" htmlFor="template-source">
                {template.kind === "mjml" ? "MJML" : "HTML"}
              </label>
              <textarea
                id="template-source"
                rows={20}
                spellCheck={false}
                className={`${CONTROL} mt-1 w-full py-2 font-mono text-sm`}
                value={source}
                onChange={(e) => setSource(e.target.value)}
              />
            </div>
          )}

          {save.error ? <Alert>{save.error.message}</Alert> : null}
          {save.isSuccess ? (
            <p role="status" className="mt-4">
              Template saved.
            </p>
          ) : (
            // A live region that is present before it speaks, so the next message is announced.
            <p role="status" className="sr-only" />
          )}
          <div className="mt-6">
            <Button
              variant="solid"
              busy={save.isPending}
              onClick={() => {
                // `busy` does not disable the button, so a second press must be ignored here.
                if (!save.isPending) save.mutate();
              }}
            >
              Save template
            </Button>
          </div>
        </>
      )}
    </div>
  );
}
