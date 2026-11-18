// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { type FormEvent, useState } from "react";

import { api, problemText } from "../api/client";
import { Button } from "../components/button";
import { EmptyState } from "../components/empty-state";
import { Alert } from "../components/field";
import { CONTROL } from "../components/input";
import { type Uploaded, uploadKey } from "../lib/import-upload";
import { atLeast } from "../lib/roles";
import { CARD } from "../lib/ui";
import { useWorkspace } from "./workspace-layout";

const KNOWN = {
  "insufficient-role": "Your role cannot import contacts. It takes editor or above.",
  // These three say what to do next and name no figure, so none of them can go stale against a
  // limit that moves. The server's own `detail` carries the number for anyone who needs it.
  "import-too-large":
    "That file is over the size limit. Split it into smaller files and upload them one at a time.",
  "empty-file":
    "That file has no rows. Check you picked the right file, and that it still has its contacts in it — a file with only a header row counts as empty.",
  "too-many-columns":
    "That file has more columns than an import can take. Delete the columns you do not need to import and save it again; columns you keep but do not map are still counted.",
  // 1,000,000 mirrors the server's MAX_IMPORT_ROWS; its own detail names the cap only as a bare number.
  "too-many-rows":
    "That file has more than 1,000,000 rows, the most an import takes. Split it into smaller files and upload them one at a time.",
  "rate-limited":
    "Too many uploads from this workspace in the last minute. Nothing is wrong with the file: wait a minute, then upload it again.",
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
      let result;
      try {
        result = await api.POST("/api/v1/workspaces/{workspace_id}/imports", {
          params: { path: { workspace_id: ws } },
          // The generated type says `file: string`; the serializer sends the real file.
          body: { file: "" },
          bodySerializer: () => form,
        });
      } catch {
        // `fetch` REJECTS rather than answering when the connection goes, and the browser's own
        // message for it is "Failed to fetch", which tells an operator nothing they can act on.
        // The upload is the long request on this screen, so this is the likely one to be cut.
        throw new Error(
          "The upload did not finish — the connection to the server was lost. Nothing was imported. Check your connection and upload the file again.",
        );
      }
      const { data, error } = result;
      if (!data) throw new Error(problemText(error, KNOWN, "The file could not be uploaded."));
      return { upload: data, filename: chosen.name } satisfies Uploaded;
    },
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    setMissing(file === null);
    if (!file || send.isPending) return;
    // ON THE MUTATE CALL, NOT THE HOOK. A hook-level `onSuccess` runs even if this screen is
    // gone, so an operator who navigated away during a long upload was yanked to the mapping
    // screen minutes later. React Query skips a mutate-level callback once the caller unmounts.
    send.mutate(file, {
      onSuccess: async (uploaded) => {
        qc.setQueryData(uploadKey(ws, uploaded.upload.id), uploaded);
        await navigate({
          to: "/w/$slug/imports/$importId",
          params: { slug, importId: uploaded.upload.id },
        });
      },
    });
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
      {/* A large file takes a while and `aria-busy` alone is silent on screen, so the wait says
          so in words, as the run does on the next screen. */}
      <p role="status" className="mt-4 text-fg-muted">
        {send.isPending ? "Uploading the file. Keep this tab open until it finishes." : ""}
      </p>
      <div className="mt-6 flex justify-end">
        <Button type="submit" variant="solid" busy={send.isPending}>
          Upload
        </Button>
      </div>
    </form>
  );
}
