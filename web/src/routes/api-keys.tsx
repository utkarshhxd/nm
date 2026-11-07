// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useEffect, useRef, useState } from "react";

import { api, problemMessage, problemType } from "../api/client";
import { ROLES, type Role, canAssign } from "../lib/roles";
import { Alert, BUTTON, CARD, ConfirmButton, CopyButton, Field, ROW_BUTTON, SELECT } from "../lib/ui";
import { useWorkspace } from "./workspace-layout";

function day(iso: string): string {
  return new Date(iso).toLocaleDateString();
}

export default function ApiKeys() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return <ApiKeyList id={workspace.id} myRole={workspace.role} />;
}

function ApiKeyList({ id, myRole }: { id: string; myRole: Role }) {
  const client = useQueryClient();
  // `owner` is never a key's role, so it is not offered whatever the caller's own role is.
  const offerable = ROLES.filter((role) => role !== "owner" && canAssign(myRole, null, role));
  const [name, setName] = useState("");
  const [role, setRole] = useState<Role>(offerable[0] ?? "viewer");
  const [nameError, setNameError] = useState("");
  const [formError, setFormError] = useState("");
  const [busy, setBusy] = useState(false);
  /**
   * The new key, held in component state for exactly as long as the dialog is open. It is never
   * logged, never put in the URL and never stored: this is the only copy that will ever exist.
   */
  const [created, setCreated] = useState<{ name: string; key: string } | null>(null);

  const keys = useQuery({
    queryKey: ["api-keys", id],
    queryFn: async () => {
      const { data, error: problem } = await api.GET("/api/v1/workspaces/{id}/api-keys", {
        params: { path: { id } },
      });
      if (problem || !data) throw new Error("could not load the API keys");
      return data.data;
    },
    retry: false,
  });

  const revoke = useMutation({
    mutationFn: async (keyId: string) => {
      const { error: problem } = await api.DELETE("/api/v1/workspaces/{id}/api-keys/{key_id}", {
        params: { path: { id, key_id: keyId } },
      });
      if (problem) throw new Error(problemMessage(problem, "That key could not be revoked."));
    },
    onSuccess: async () => {
      setFormError("");
      await client.invalidateQueries({ queryKey: ["api-keys", id] });
    },
    onError: (refused: Error) => setFormError(refused.message),
  });

  async function create(event: FormEvent) {
    event.preventDefault();
    setNameError("");
    setFormError("");
    setBusy(true);
    const { data, error: problem } = await api.POST("/api/v1/workspaces/{id}/api-keys", {
      params: { path: { id } },
      body: { name, role },
    });
    setBusy(false);
    if (problem || !data) {
      const message = problemMessage(problem, "That key could not be created.");
      if (problemType(problem) === "urn:nm:problem:validation") setNameError(message);
      else setFormError(message);
      return;
    }
    setName("");
    setCreated({ name: data.name, key: data.key });
    await client.invalidateQueries({ queryKey: ["api-keys", id] });
  }

  return (
    <section className={CARD}>
      <h2 className="text-xl font-semibold">API keys</h2>

      {created ? <NewKey created={created} onStored={() => setCreated(null)} /> : null}

      {formError ? <Alert>{formError}</Alert> : null}
      {keys.isPending ? <p className="mt-4">Loading…</p> : null}
      {keys.isError ? (
        <Alert>The API keys could not be loaded. Reload the page to try again.</Alert>
      ) : null}
      {keys.data && keys.data.length > 0 ? (
        <table className="mt-4 w-full text-left">
          <thead>
            <tr className="border-b border-gray-300 dark:border-gray-700">
              <th scope="col">Name</th>
              <th scope="col">Role</th>
              <th scope="col">Prefix</th>
              <th scope="col">Created</th>
              <th scope="col">Last used</th>
              <th scope="col">
                <span className="sr-only">Actions</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {keys.data.map((key) => (
              <tr className="border-b border-gray-300 dark:border-gray-700" key={key.id}>
                <td className="py-2">{key.name}</td>
                <td className="py-2">{key.role}</td>
                <td className="py-2 font-mono">nm_{key.prefix}</td>
                <td className="py-2">{day(key.created_at)}</td>
                <td className="py-2">{key.last_used_at ? day(key.last_used_at) : "never"}</td>
                <td className="py-2">
                  <ConfirmButton
                    action="Revoke"
                    subject={key.name}
                    onConfirm={() => revoke.mutate(key.id)}
                  />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}
      {keys.data && keys.data.length === 0 ? <p className="mt-4">No API keys yet.</p> : null}

      <form onSubmit={create} noValidate>
        <Field
          id="key-name"
          label="Name"
          autoComplete="off"
          required
          error={nameError}
          value={name}
          onChange={(event) => setName(event.target.value)}
        />
        <div className="mt-4">
          <label className="block font-medium" htmlFor="key-role">
            Role
          </label>
          <select
            id="key-role"
            className={`mt-1 ${SELECT}`}
            value={role}
            onChange={(event) => setRole(event.target.value as Role)}
          >
            {offerable.map((offered) => (
              <option key={offered} value={offered}>
                {offered}
              </option>
            ))}
          </select>
        </div>
        <button className={BUTTON} type="submit" disabled={busy || offerable.length === 0}>
          Create key
        </button>
      </form>
    </section>
  );
}

/**
 * The one and only sight of a new key.
 *
 * shortcut: focus moves into the dialog but is not trapped inside it, so `aria-modal` is not
 * claimed; nothing behind it is destructive and the key stays on screen until "I've stored it"
 * is pressed. Give it a real trap the day a second modal needs one.
 */
function NewKey({
  created,
  onStored,
}: {
  created: { name: string; key: string };
  onStored: () => void;
}) {
  const panel = useRef<HTMLDivElement>(null);

  useEffect(() => {
    panel.current?.focus();
  }, []);

  return (
    <div
      ref={panel}
      role="dialog"
      aria-labelledby="new-key-title"
      tabIndex={-1}
      className="mt-4 rounded border-2 border-blue-700 p-4 dark:border-blue-300"
    >
      <h3 className="font-semibold" id="new-key-title">
        Copy {created.name} now
      </h3>
      <p className="mt-2">This is the only time this key is shown. It cannot be read again.</p>
      <code className="mt-2 block break-all font-mono" data-testid="api-key-value">
        {created.key}
      </code>
      <p className="mt-4 flex flex-wrap gap-3">
        <CopyButton value={created.key} />
        <button className={ROW_BUTTON} type="button" onClick={onStored}>
          I&apos;ve stored it
        </button>
      </p>
    </div>
  );
}
