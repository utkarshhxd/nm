// SPDX-License-Identifier: AGPL-3.0-only

import { type FormEvent, useEffect, useState } from "react";

import { api, problemDetail, problemType } from "../api/client";
import { signInWithPasskey } from "../lib/passkeys";
import { BUTTON, Field, SECONDARY_BUTTON } from "../lib/ui";

/**
 * Where to go once signed in. `next` comes from the URL, so only a path on this origin is
 * accepted: an absolute URL, `//elsewhere` or `/\elsewhere` (which browsers also read as
 * protocol-relative) would make this an open redirect.
 */
function nextPath(): string {
  const next = new URLSearchParams(window.location.search).get("next") ?? "";
  const local = next.startsWith("/") && !next.startsWith("//") && !next.startsWith("/\\");
  return local ? next : "/";
}

/** A full load, not a router navigation: it is what re-reads `/me` and the new session cookie. */
function enterApp(): void {
  window.location.assign(nextPath());
}

function tooManyAttempts(retryAfter: string | null): string {
  const seconds = Number(retryAfter);
  const minutes = Number.isFinite(seconds) && seconds > 0 ? Math.ceil(seconds / 60) : 15;
  return `Too many attempts — try again in ${minutes} minute${minutes === 1 ? "" : "s"}`;
}

export default function Login() {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [secondFactor, setSecondFactor] = useState(false);
  const [recovery, setRecovery] = useState(false);
  const [code, setCode] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  // Conditional UI: the browser may offer a passkey straight from the email field's autofill.
  // It stays pending until someone picks one, and rejects outright where conditional UI is not
  // available — neither is an error worth showing, so nothing is rendered for it.
  useEffect(() => {
    void signInWithPasskey(true).then(enterApp, () => {});
  }, []);

  async function submitCredentials(event: FormEvent) {
    event.preventDefault();
    setError("");
    setBusy(true);
    const { data, error: problem, response } = await api.POST("/api/v1/auth/login", {
      body: { email, password },
    });
    setBusy(false);
    if (response.status === 429) {
      setError(tooManyAttempts(response.headers.get("retry-after")));
    } else if (problem || !data) {
      setError(problemDetail(problem, "Could not sign in"));
    } else if (data.status === "mfa_required") {
      setSecondFactor(true);
      setPassword("");
    } else {
      enterApp();
    }
  }

  async function submitSecondFactor(event: FormEvent) {
    event.preventDefault();
    setError("");
    setBusy(true);
    const { data, error: problem, response } = await api.POST("/api/v1/auth/login/totp", {
      body: recovery ? { recovery_code: code } : { code },
    });
    setBusy(false);
    if (response.status === 429) {
      setError(tooManyAttempts(response.headers.get("retry-after")));
    } else if (problemType(problem) === "urn:nm:problem:mfa-restart") {
      // The pending session is gone; only the password can mint another one.
      setSecondFactor(false);
      setCode("");
      setError("Too many wrong codes. Sign in again.");
    } else if (problem || !data) {
      setError(problemDetail(problem, "Could not sign in"));
    } else {
      enterApp();
    }
  }

  async function usePasskey() {
    setError("");
    try {
      await signInWithPasskey(false);
      enterApp();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : "That passkey could not be used");
    }
  }

  return (
    <main className="mx-auto max-w-sm p-8">
      <h1 className="text-2xl font-semibold">Sign in</h1>

      {secondFactor ? (
        <form onSubmit={submitSecondFactor} noValidate>
          {recovery ? (
            <Field
              id="recovery-code"
              label="Recovery code"
              autoComplete="one-time-code"
              autoFocus
              required
              error={error}
              value={code}
              onChange={(e) => setCode(e.target.value)}
            />
          ) : (
            <Field
              id="code"
              label="Six-digit code"
              inputMode="numeric"
              autoComplete="one-time-code"
              autoFocus
              required
              error={error}
              value={code}
              onChange={(e) => setCode(e.target.value)}
            />
          )}
          <button className={BUTTON} type="submit" disabled={busy}>
            Verify
          </button>
          <button
            className={SECONDARY_BUTTON}
            type="button"
            onClick={() => {
              setRecovery(!recovery);
              setCode("");
              setError("");
            }}
          >
            {recovery ? "Use an authenticator code instead" : "Use a recovery code instead"}
          </button>
        </form>
      ) : (
        <form onSubmit={submitCredentials} noValidate>
          <Field
            id="email"
            label="Email"
            type="email"
            // `webauthn` is what lets the browser offer a passkey in this field's autofill list.
            autoComplete="username webauthn"
            autoFocus
            required
            value={email}
            onChange={(e) => setEmail(e.target.value)}
          />
          <Field
            id="password"
            label="Password"
            type="password"
            autoComplete="current-password"
            required
            error={error}
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
          <button className={BUTTON} type="submit" disabled={busy}>
            Sign in
          </button>
          <button className={SECONDARY_BUTTON} type="button" onClick={usePasskey}>
            Sign in with a passkey
          </button>
        </form>
      )}
    </main>
  );
}
