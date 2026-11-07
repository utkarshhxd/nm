// SPDX-License-Identifier: AGPL-3.0-only

import { Link, Navigate } from "@tanstack/react-router";
import { type FormEvent, type ReactNode, useMemo, useState } from "react";

import { api, problemMessage } from "../api/client";
import { signOut, useMe } from "../lib/session";
import { Alert, BUTTON, Field, ROW_BUTTON, SELECT } from "../lib/ui";

/**
 * The entry point: straight into the first workspace when there is one. A person with none sees
 * either the form to make one, or who to ask — this is also the screen a brand-new instance's
 * admin lands on after the first sign-in.
 */
export default function Home() {
  const { data, isPending } = useMe();

  if (isPending || !data) return <Shell>Loading…</Shell>;

  const first = data.workspaces[0];
  if (first) {
    return <Navigate to="/w/$slug/members" params={{ slug: first.slug }} replace />;
  }

  return (
    <Shell>
      {data.is_instance_admin ? (
        <CreateWorkspaceForm />
      ) : (
        <p className="mt-4">You&apos;re not in any workspace yet — ask an admin for an invite.</p>
      )}
    </Shell>
  );
}

/**
 * `/workspaces/new`: the same form, reachable once the admin already has a workspace — `/` then
 * redirects into it, so this is the only way back to the form.
 */
export function NewWorkspace() {
  const { data, isPending } = useMe();

  if (isPending || !data) return <Shell>Loading…</Shell>;
  // Cosmetic only: `POST /workspaces` is `instance-admin-required` whatever this renders.
  if (!data.is_instance_admin) {
    return (
      <Shell>
        <p className="mt-4">Only an instance admin can create a workspace.</p>
      </Shell>
    );
  }
  return (
    <Shell>
      <p className="mt-4">
        <Link className="font-medium text-blue-800 underline dark:text-blue-200" to="/">
          Back
        </Link>
      </p>
      <CreateWorkspaceForm />
    </Shell>
  );
}

function CreateWorkspaceForm() {
  const [slug, setSlug] = useState("");
  const [name, setName] = useState("");
  // `Intl` knows the zones the runtime actually has, so the list cannot drift from the one the
  // server validates against (both are IANA names). V8 includes "UTC" in that list and the spec
  // does not require it to, so it is dropped here and offered first on its own either way.
  const zones = useMemo(() => Intl.supportedValuesOf("timeZone").filter((z) => z !== "UTC"), []);
  const [timezone, setTimezone] = useState("UTC");
  const [slugError, setSlugError] = useState("");
  const [nameError, setNameError] = useState("");
  const [formError, setFormError] = useState("");
  const [busy, setBusy] = useState(false);

  async function create(event: FormEvent) {
    event.preventDefault();
    setSlugError("");
    setNameError("");
    setFormError("");
    setBusy(true);
    const { data, error: problem } = await api.POST("/api/v1/workspaces", {
      body: { slug, name, timezone },
    });
    setBusy(false);
    if (problem || !data) {
      const message = problemMessage(problem, "That workspace could not be created.");
      // A taken slug and an invalid one are both about the slug field. A 422 can also be about
      // the name or the zone, and `problemMessage` gives the first of them, so a name that is
      // genuinely wrong would be announced on the slug — hence the field-by-field read below.
      const fields = (problem as { errors?: { field?: string; message?: string }[] } | undefined)
        ?.errors;
      const forField = (field: string) => fields?.find((e) => e.field === field)?.message ?? "";
      if (fields) {
        setSlugError(forField("slug"));
        setNameError(forField("name"));
        if (!forField("slug") && !forField("name")) setFormError(message);
      } else {
        setSlugError(message);
      }
      return;
    }
    // A full load, not a router navigation: `/me.workspaces` is what every workspace screen
    // reads, and it has just changed.
    window.location.assign(`/w/${data.slug}/members`);
  }

  return (
    <form className="mt-4 max-w-sm" onSubmit={create} noValidate>
      <h2 className="text-xl font-semibold">Create workspace</h2>
      <Field
        id="workspace-slug"
        label="Slug"
        autoComplete="off"
        required
        hint="Lowercase letters, digits and hyphens. It is in every link and cannot be changed."
        error={slugError}
        value={slug}
        onChange={(event) => setSlug(event.target.value)}
      />
      <Field
        id="workspace-name"
        label="Workspace name"
        autoComplete="off"
        required
        error={nameError}
        value={name}
        onChange={(event) => setName(event.target.value)}
      />
      <div className="mt-4">
        <label className="block font-medium" htmlFor="workspace-timezone">
          Time zone
        </label>
        <select
          id="workspace-timezone"
          className={`mt-1 w-full ${SELECT}`}
          value={timezone}
          onChange={(event) => setTimezone(event.target.value)}
        >
          <option value="UTC">UTC</option>
          {zones.map((zone) => (
            <option key={zone} value={zone}>
              {zone}
            </option>
          ))}
        </select>
      </div>
      {formError ? <Alert>{formError}</Alert> : null}
      <button className={BUTTON} type="submit" disabled={busy}>
        Create workspace
      </button>
    </form>
  );
}

function Shell({ children }: { children: ReactNode }) {
  const { data } = useMe();

  return (
    <main className="mx-auto max-w-xl p-8">
      <div className="flex flex-wrap items-baseline justify-between gap-3">
        <h1 className="text-2xl font-semibold">NM</h1>
        {data ? (
          <p className="flex items-center gap-3">
            <span>Signed in as {data.email}</span>
            <button className={ROW_BUTTON} type="button" onClick={() => void signOut()}>
              Sign out
            </button>
          </p>
        ) : null}
      </div>
      {data ? (
        <p className="mt-4">
          <Link
            className="font-medium text-blue-800 underline dark:text-blue-200"
            to="/account/security"
          >
            Account security
          </Link>
        </p>
      ) : null}
      {children}
    </main>
  );
}
