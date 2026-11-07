// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useState } from "react";

import { api, problemMessage } from "../api/client";
import type { components } from "../api/schema";
import { Alert, CARD, ROW_BUTTON } from "../lib/ui";
import { useWorkspace } from "./workspace-layout";

type Entry = components["schemas"]["AuditEntry"];

/**
 * Who did it, in one cell. The server resolves the actor ids to an address or a key prefix; an
 * id that resolved to nothing is shown raw rather than hidden, because the actor having been
 * deleted since is itself worth seeing.
 */
function actorOf(entry: Entry): string {
  if (entry.actor_email) return entry.actor_email;
  if (entry.actor_api_key_prefix) return `API key nm_${entry.actor_api_key_prefix}`;
  if (entry.actor_user_id) return entry.actor_user_id;
  if (entry.actor_api_key_id) return `API key ${entry.actor_api_key_id}`;
  return "system";
}

function targetOf(entry: Entry): string {
  if (!entry.target_type) return "";
  return entry.target_id ? `${entry.target_type} ${entry.target_id}` : entry.target_type;
}

export default function AuditLog() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return <AuditLogPage id={workspace.id} />;
}

function AuditLogPage({ id }: { id: string }) {
  const [entries, setEntries] = useState<Entry[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [busy, setBusy] = useState(true);
  const [error, setError] = useState("");

  /**
   * One page, newest first. `cursor` is the previous page's `next_cursor`, which is the id of
   * its last row; the server returns strictly smaller ids, so pages cannot overlap, cannot skip
   * a row at the boundary, and must run out — `next_cursor` is null on the last page.
   *
   * Deliberately not `useInfiniteQuery`: this is the only accumulating list in the app, and the
   * entry chunk is 45 KB from the 150 KB budget. Reach for it if a second screen needs one.
   */
  async function page(after: string | null, live: () => boolean): Promise<void> {
    setBusy(true);
    const { data, error: problem } = await api.GET("/api/v1/workspaces/{id}/audit-log", {
      params: { path: { id }, query: after ? { cursor: after } : {} },
    });
    if (!live()) return;
    setBusy(false);
    if (problem || !data) {
      setError(problemMessage(problem, "The audit log could not be loaded."));
      return;
    }
    setError("");
    setEntries((shown) => (after ? [...shown, ...data.data] : data.data));
    setCursor(data.next_cursor ?? null);
  }

  useEffect(() => {
    // `live` discards the result of the run StrictMode throws away, so the first page cannot be
    // appended twice — and a slow answer for the workspace we have just navigated away from
    // cannot land in the list of the one we are on now.
    let live = true;
    setEntries([]);
    setCursor(null);
    void page(null, () => live);
    return () => {
      live = false;
    };
    // `page` is redefined every render; `id` is the only thing it closes over that matters.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id]);

  return (
    <section className={CARD}>
      <h2 className="text-xl font-semibold">Audit log</h2>
      {error ? <Alert>{error}</Alert> : null}
      <table className="mt-4 w-full text-left">
        <thead>
          <tr className="border-b border-gray-300 dark:border-gray-700">
            <th scope="col">Time</th>
            <th scope="col">Action</th>
            <th scope="col">Actor</th>
            <th scope="col">Target</th>
            <th scope="col">IP</th>
          </tr>
        </thead>
        <tbody>
          {entries.map((entry) => (
            <tr className="border-b border-gray-300 dark:border-gray-700" key={entry.id}>
              <td className="py-2">{new Date(entry.created_at).toLocaleString()}</td>
              <td className="py-2 font-mono">{entry.action}</td>
              <td className="py-2 break-all">{actorOf(entry)}</td>
              <td className="py-2 break-all">{targetOf(entry)}</td>
              <td className="py-2">{entry.ip ?? ""}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {busy ? <p className="mt-4">Loading…</p> : null}
      {!busy && entries.length === 0 ? <p className="mt-4">Nothing recorded yet.</p> : null}
      {cursor ? (
        <button
          className={`mt-4 ${ROW_BUTTON}`}
          type="button"
          disabled={busy}
          onClick={() => void page(cursor, () => true)}
        >
          Load more
        </button>
      ) : null}
    </section>
  );
}
