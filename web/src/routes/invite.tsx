// SPDX-License-Identifier: AGPL-3.0-only

import { useQuery } from "@tanstack/react-query";
import { useParams } from "@tanstack/react-router";
import { type FormEvent, type ReactNode, useState } from "react";

import { api, problemMessage } from "../api/client";
import { fetchMeOrNull } from "../lib/session";
import { Alert, BUTTON, Field } from "../lib/ui";

/** Claims the invite token the API put in `invite_url`. */
export default function Invite() {
  const { token } = useParams({ from: "/invite/$token" });
  const [displayName, setDisplayName] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  const invite = useQuery({
    queryKey: ["invite", token],
    queryFn: async () => {
      const { data, error: problem } = await api.GET("/api/v1/invites/{token}", {
        params: { path: { token } },
      });
      // Unknown, expired, used and revoked are one indistinguishable 404 by design.
      if (problem || !data) throw new Error("This invitation is no longer valid.");
      return data;
    },
    retry: false,
  });

  // Not `useMe`: a newcomer has no account, and being bounced to `/login` from the one page
  // meant for them would be a dead end. See `fetchMeOrNull`.
  const viewer = useQuery({ queryKey: ["me", "optional"], queryFn: fetchMeOrNull, retry: false });

  async function accept(event: FormEvent) {
    event.preventDefault();
    setError("");
    setBusy(true);
    const newcomer = invite.data?.account_exists === false;
    const { data, error: problem } = await api.POST("/api/v1/invites/{token}/accept", {
      params: { path: { token } },
      body: newcomer ? { display_name: displayName, password } : {},
    });
    setBusy(false);
    if (problem || !data) {
      setError(problemMessage(problem, "This invitation could not be accepted."));
      return;
    }
    window.location.assign("/");
  }

  if (invite.isPending || viewer.isPending) {
    return <Shell>Loading…</Shell>;
  }
  if (invite.error || !invite.data) {
    return (
      <Shell>
        <Alert>This invitation is no longer valid.</Alert>
      </Shell>
    );
  }

  const { workspace_name, role, email, account_exists } = invite.data;
  const signedIn = viewer.data !== null && viewer.data !== undefined;

  return (
    <Shell>
      <p className="mt-4">
        You have been invited to join <strong>{workspace_name}</strong> as {role}, at {email}.
      </p>

      {account_exists && !signedIn ? (
        <p className="mt-6">
          <a
            className="font-medium text-blue-800 underline dark:text-blue-200"
            href={`/login?next=${encodeURIComponent(`/invite/${token}`)}`}
          >
            Sign in to accept
          </a>
        </p>
      ) : (
        <form onSubmit={accept} noValidate>
          {account_exists ? null : (
            <>
              <Field
                id="display-name"
                label="Your name"
                autoComplete="name"
                required
                value={displayName}
                onChange={(e) => setDisplayName(e.target.value)}
              />
              <Field
                id="password"
                label="Password"
                type="password"
                autoComplete="new-password"
                minLength={12}
                required
                hint="At least 12 characters."
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
            </>
          )}
          {error ? <Alert>{error}</Alert> : null}
          <button className={BUTTON} type="submit" disabled={busy}>
            Accept
          </button>
        </form>
      )}
    </Shell>
  );
}

function Shell({ children }: { children: ReactNode }) {
  return (
    <main className="mx-auto max-w-sm p-8">
      <h1 className="text-2xl font-semibold">Invitation</h1>
      {children}
    </main>
  );
}
