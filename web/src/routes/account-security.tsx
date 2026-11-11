// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useEffect, useState } from "react";

import { api, problemMessage, problemType } from "../api/client";
import { registerPasskey } from "../lib/passkeys";
import { useMe } from "../lib/session";
import { Alert, BUTTON, CARD, CopyButton, Field, SECONDARY_BUTTON } from "../lib/ui";

export default function AccountSecurity() {
  const me = useMe();

  return (
    <main className="mx-auto max-w-xl p-8">
      <h1 className="text-2xl font-semibold">Security</h1>
      {me.isPending || !me.data ? (
        <p className="mt-4">Loading…</p>
      ) : (
        <>
          <ChangePassword />
          <TwoFactor enabled={me.data.totp_enabled} />
          <Passkeys />
        </>
      )}
    </main>
  );
}

function ChangePassword() {
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  // One message per field, not one shared one: a rejected *current* password announced as a
  // defect of the *new* password field sends a screen-reader user to fix the input they got
  // right. The server tells the two apart — a wrong current password is 401
  // `invalid-credentials`, a new password outside 12-256 characters is 422 `validation`.
  const [currentError, setCurrentError] = useState("");
  const [nextError, setNextError] = useState("");
  const [formError, setFormError] = useState("");
  const [done, setDone] = useState(false);
  const [busy, setBusy] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    setCurrentError("");
    setNextError("");
    setFormError("");
    setDone(false);
    setBusy(true);
    let problem: unknown;
    try {
      ({ error: problem } = await api.POST("/api/v1/me/password", {
        body: { current_password: current, new_password: next },
      }));
    } catch {
      // The request never completed — offline, DNS, a dropped connection.
      setBusy(false);
      setFormError("The password could not be changed. Check your connection and try again.");
      return;
    }
    setBusy(false);
    if (problem) {
      const message = problemMessage(problem, "The password could not be changed.");
      // Only the two problems that are *about* a field go on that field. A 429, a 5xx or
      // anything else says nothing about what was typed, and announcing it on an input tells
      // the person to fix something that is not wrong.
      switch (problemType(problem)) {
        case "urn:nm:problem:validation":
          setNextError(message);
          break;
        case "urn:nm:problem:invalid-credentials":
          setCurrentError(message);
          break;
        default:
          setFormError(message);
      }
      return;
    }
    setCurrent("");
    setNext("");
    setDone(true);
  }

  return (
    <section className={CARD}>
      <h2 className="text-xl font-semibold">Password</h2>
      <form onSubmit={submit} noValidate>
        <Field
          id="current-password"
          label="Current password"
          type="password"
          autoComplete="current-password"
          required
          error={currentError}
          value={current}
          onChange={(e) => setCurrent(e.target.value)}
        />
        <Field
          id="new-password"
          label="New password"
          type="password"
          autoComplete="new-password"
          minLength={12}
          required
          hint="At least 12 characters."
          error={nextError}
          value={next}
          onChange={(e) => setNext(e.target.value)}
        />
        {formError ? <Alert>{formError}</Alert> : null}
        {done ? <p className="mt-2 text-green-800 dark:text-green-300">Password changed.</p> : null}
        <button className={BUTTON} type="submit" disabled={busy}>
          Change password
        </button>
      </form>
    </section>
  );
}

type Setup = { secret: string; otpauth_url: string };

function TwoFactor({ enabled }: { enabled: boolean }) {
  const client = useQueryClient();
  const [setup, setSetup] = useState<Setup | null>(null);
  const [codes, setCodes] = useState<string[] | null>(null);
  const [code, setCode] = useState("");
  const [password, setPassword] = useState("");
  const [turningOff, setTurningOff] = useState(false);
  // One message per failing action, each tied to the field that caused it. Setting up has no
  // field at all, so its failure is a form-level alert rather than `aria-describedby` of an
  // input the person never filled in.
  const [setupError, setSetupError] = useState("");
  const [codeError, setCodeError] = useState("");
  const [passwordError, setPasswordError] = useState("");
  const [busy, setBusy] = useState(false);

  async function start() {
    setSetupError("");
    setBusy(true);
    let result;
    try {
      result = await api.POST("/api/v1/me/totp/setup");
    } catch {
      // The request never completed — offline, DNS, a dropped connection.
      setBusy(false);
      setSetupError("Two-factor setup could not be started. Check your connection and try again.");
      return;
    }
    const { data, error: problem } = result;
    setBusy(false);
    if (problem || !data) {
      setSetupError(problemMessage(problem, "Two-factor setup could not be started."));
      return;
    }
    setSetup(data);
  }

  async function confirm(event: FormEvent) {
    event.preventDefault();
    setCodeError("");
    setBusy(true);
    let result;
    try {
      result = await api.POST("/api/v1/me/totp/confirm", { body: { code } });
    } catch {
      // The request never completed — offline, DNS, a dropped connection.
      setBusy(false);
      setCodeError("That code could not be submitted. Check your connection and try again.");
      return;
    }
    const { data, error: problem } = result;
    setBusy(false);
    if (problem || !data) {
      setCodeError(problemMessage(problem, "That code was not accepted."));
      return;
    }
    setSetup(null);
    setCode("");
    setCodes(data.recovery_codes);
    await client.invalidateQueries({ queryKey: ["me"] });
  }

  async function disable(event: FormEvent) {
    event.preventDefault();
    setPasswordError("");
    setBusy(true);
    let problem: unknown;
    try {
      ({ error: problem } = await api.DELETE("/api/v1/me/totp", { body: { password } }));
    } catch {
      // The request never completed — offline, DNS, a dropped connection.
      setBusy(false);
      setPasswordError(
        "Two-factor sign-in could not be turned off. Check your connection and try again.",
      );
      return;
    }
    setBusy(false);
    if (problem) {
      setPasswordError(problemMessage(problem, "Two-factor sign-in could not be turned off."));
      return;
    }
    setPassword("");
    setTurningOff(false);
    await client.invalidateQueries({ queryKey: ["me"] });
  }

  return (
    <section className={CARD}>
      <h2 className="text-xl font-semibold">Two-factor sign-in</h2>

      {codes ? <RecoveryCodes codes={codes} onSaved={() => setCodes(null)} /> : null}

      {setup ? (
        <form onSubmit={confirm} noValidate>
          <p className="mt-4">
            Scan this with your authenticator app, or type the secret into it, then enter the code
            it shows.
          </p>
          <QrCode value={setup.otpauth_url} />
          <p className="mt-2">
            Secret:{" "}
            <code className="break-all font-mono" data-testid="totp-secret">
              {setup.secret}
            </code>
          </p>
          <Field
            id="totp-code"
            label="Six-digit code"
            inputMode="numeric"
            autoComplete="one-time-code"
            required
            error={codeError}
            value={code}
            onChange={(e) => setCode(e.target.value)}
          />
          <button className={BUTTON} type="submit" disabled={busy}>
            Confirm
          </button>
        </form>
      ) : null}

      {!setup && !enabled ? (
        <>
          <p className="mt-4">A code from an authenticator app is asked for at every sign-in.</p>
          {setupError ? <Alert>{setupError}</Alert> : null}
          <button className={BUTTON} type="button" onClick={start} disabled={busy}>
            Set up
          </button>
        </>
      ) : null}

      {!setup && enabled ? (
        <>
          <p className="mt-4">Two-factor sign-in is on.</p>
          {turningOff ? (
            <form onSubmit={disable} noValidate>
              <Field
                id="disable-password"
                label="Password"
                type="password"
                autoComplete="current-password"
                required
                error={passwordError}
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
              <button className={BUTTON} type="submit" disabled={busy}>
                Disable two-factor sign-in
              </button>
            </form>
          ) : (
            <button className={BUTTON} type="button" onClick={() => setTurningOff(true)}>
              Turn off
            </button>
          )}
        </>
      ) : null}
    </section>
  );
}

/** `qrcode` is loaded here and nowhere else, so it stays out of the entry bundle. */
function QrCode({ value }: { value: string }) {
  const [src, setSrc] = useState("");

  useEffect(() => {
    let live = true;
    void import("qrcode")
      .then((qr) => qr.toDataURL(value))
      .then((url) => {
        if (live) setSrc(url);
      })
      .catch(() => {
        // The secret below is the fallback: it is all an authenticator app actually needs.
      });
    return () => {
      live = false;
    };
  }, [value]);

  return src ? (
    <img className="mt-4 bg-white p-2" src={src} alt="QR code for your authenticator app" />
  ) : null;
}

function RecoveryCodes({ codes, onSaved }: { codes: string[]; onSaved: () => void }) {
  const text = `${codes.join("\n")}\n`;

  function download() {
    const url = URL.createObjectURL(new Blob([text], { type: "text/plain" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = "nm-recovery-codes.txt";
    link.click();
    // Not in this tick, and not merely in the next macrotask: Firefox and Safari cancel a
    // download whose object URL is revoked before the download has actually started, and they
    // start it asynchronously. "I've saved these" then clears the only copy of these codes that
    // exists anywhere, so a cancelled download loses them for good. 40s is the margin
    // FileSaver.js settled on for the same reason.
    setTimeout(() => URL.revokeObjectURL(url), 40_000);
  }

  return (
    <div className="mt-4">
      <p className="font-medium">
        Save these recovery codes now. They are shown once, and each one works once.
      </p>
      <ul className="mt-2 font-mono">
        {codes.map((recoveryCode) => (
          <li data-testid="recovery-code" key={recoveryCode}>
            {recoveryCode}
          </li>
        ))}
      </ul>
      <CopyButton value={text} className={BUTTON} />
      <button className={SECONDARY_BUTTON} type="button" onClick={download}>
        Download
      </button>
      <button className={SECONDARY_BUTTON} type="button" onClick={onSaved}>
        I&apos;ve saved these
      </button>
    </div>
  );
}

function Passkeys() {
  const client = useQueryClient();
  const [name, setName] = useState("");
  // Adding fails because of the name field; removing has no field at all, so its failure is a
  // form-level alert rather than `aria-describedby` of an input that had nothing to do with it.
  const [addError, setAddError] = useState("");
  const [removeError, setRemoveError] = useState("");

  const passkeys = useQuery({
    queryKey: ["passkeys"],
    queryFn: async () => {
      const { data, error: problem } = await api.GET("/api/v1/me/passkeys");
      if (problem || !data) throw new Error("could not load your passkeys");
      // One page is the whole list in practice: these are the caller's own passkeys, and the
      // default limit is 50. `next_cursor` is ignored rather than paged through.
      return data.data;
    },
    retry: false,
  });

  const refresh = () => client.invalidateQueries({ queryKey: ["passkeys"] });

  const add = useMutation({
    mutationFn: async () => {
      await registerPasskey(name);
    },
    onSuccess: async () => {
      setName("");
      setAddError("");
      await refresh();
    },
    onError: (failure: Error) => setAddError(failure.message),
  });

  const remove = useMutation({
    mutationFn: async (id: string) => {
      const { error: problem } = await api.DELETE("/api/v1/me/passkeys/{id}", {
        params: { path: { id } },
      });
      if (problem) throw new Error(problemMessage(problem, "That passkey could not be removed."));
    },
    onSuccess: async () => {
      setRemoveError("");
      await refresh();
    },
    onError: (failure: Error) => setRemoveError(failure.message),
  });

  return (
    <section className={CARD}>
      <h2 className="text-xl font-semibold">Passkeys</h2>
      {passkeys.data && passkeys.data.length > 0 ? (
        <ul className="mt-4">
          {passkeys.data.map((passkey) => (
            <li className="flex items-center justify-between border-b border-gray-300 py-2 dark:border-gray-700" key={passkey.id}>
              <span>{passkey.name}</span>
              <button
                className="rounded border border-gray-500 px-3 py-1 text-blue-800 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-blue-700 dark:border-gray-400 dark:text-blue-200"
                type="button"
                aria-label={`Remove ${passkey.name}`}
                onClick={() => remove.mutate(passkey.id)}
              >
                Remove
              </button>
            </li>
          ))}
        </ul>
      ) : (
        <p className="mt-4">No passkeys yet.</p>
      )}
      {removeError ? <Alert>{removeError}</Alert> : null}
      <form
        onSubmit={(event) => {
          event.preventDefault();
          add.mutate();
        }}
        noValidate
      >
        <Field
          id="passkey-name"
          label="Passkey name"
          required
          error={addError}
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
        <button className={BUTTON} type="submit" disabled={add.isPending}>
          Add passkey
        </button>
      </form>
    </section>
  );
}
