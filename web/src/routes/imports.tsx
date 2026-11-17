// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";

import { api, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Button } from "../components/button";
import { EmptyState } from "../components/empty-state";
import { Alert } from "../components/field";
import { CONTROL } from "../components/input";
import { atLeast } from "../lib/roles";
import { CARD } from "../lib/ui";
import { useWorkspace } from "./workspace-layout";

type Upload = components["schemas"]["Upload"];

/** What the mapping screen is given: the server's reading of the file, and the name the browser had for it. */
export type Uploaded = { upload: Upload; filename: string };

/**
 * Where an upload waits for the mapping screen. There is no `GET /imports/{id}` yet, so the upload
 * response is the only copy of what the server read: it travels in the query cache, and a reload
 * of the mapping screen loses it.
 */
export const uploadKey = (ws: string, id: string) => ["import-upload", ws, id] as const;

const KNOWN = {
  "insufficient-role": "Your role cannot import contacts. It takes editor or above.",
  "import-too-large": "That file is over the size limit.",
  "empty-file": "That file has no rows.",
  "too-many-columns": "That file has more columns than an import can take.",
};

export default function Imports() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return (
    <section className="mt-8">
      <h2 className="text-xl font-semibold">Import contacts</h2>
      {atLeast(workspace.role, "editor") ? (
        <UploadForm ws={workspace.id} slug={workspace.slug} />
      ) : (
        <EmptyState title="Importing takes editor">
          Your role cannot import contacts. Ask an admin of this workspace to raise it.
        </EmptyState>
      )}
    </section>
  );
}

function UploadForm({ ws, slug }: { ws: string; slug: string }) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [file, setFile] = useState<File | null>(null);
  const [missing, setMissing] = useState(false);

  const send = useMutation({
    mutationFn: async (chosen: File) => {
      const form = new FormData();
      form.append("file", chosen);
      const { data, error } = await api.POST("/api/v1/workspaces/{workspace_id}/imports", {
        params: { path: { workspace_id: ws } },
        // The generated type says `file: string`; the serializer sends the real file.
        body: { file: "" },
        bodySerializer: () => form,
      });
      if (!data) throw new Error(problemText(error, KNOWN, "The file could not be uploaded."));
      return { upload: data, filename: chosen.name } satisfies Uploaded;
    },
    onSuccess: async (uploaded) => {
      qc.setQueryData(uploadKey(ws, uploaded.upload.id), uploaded);
      await navigate({
        to: "/w/$slug/imports/$importId",
        params: { slug, importId: uploaded.upload.id },
      });
    },
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    setMissing(file === null);
    if (file) send.mutate(file);
  }

  return (
    <form onSubmit={submit} noValidate className={CARD}>
      <label className="block font-medium" htmlFor="import-file">
        CSV file
      </label>
      <input
        id="import-file"
        type="file"
        accept=".csv,.tsv,.txt,text/csv,text/plain"
        aria-describedby="import-file-hint"
        aria-invalid={missing || undefined}
        className={`${CONTROL} mt-1 block w-full py-2`}
        onChange={(e) => {
          setFile(e.target.files?.[0] ?? null);
          setMissing(false);
        }}
      />
      <span id="import-file-hint" className="mt-1 block text-sm text-fg-muted">
        Separated by commas, semicolons, tabs or pipes. The next screen shows how each column was
        read, and nothing is imported until you confirm it.
      </span>
      {missing ? <Alert>Choose a CSV file.</Alert> : null}
      {send.error ? <Alert>{send.error.message}</Alert> : null}
      <div className="mt-6 flex justify-end">
        <Button type="submit" variant="solid" busy={send.isPending}>
          Upload
        </Button>
      </div>
    </form>
  );
}
