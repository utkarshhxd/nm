// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { type FormEvent, useState } from "react";

import { api, problemMessage, problemType } from "../api/client";
import { ROLES, type Role, canAssign } from "../lib/roles";
import { useMe } from "../lib/session";
import { Alert, BUTTON, CARD, ConfirmButton, Field, ROW_BUTTON, SELECT } from "../lib/ui";
import { useWorkspace } from "./workspace-layout";

/** The brief's wording for `409 last-owner`, which `humanise` cannot produce from the code. */
const LAST_OWNER = "A workspace needs at least one owner. Make someone else an owner first.";

/** The message for a failed action: the last-owner conflict gets the sentence above. */
function failure(problem: unknown, fallback: string): string {
  if (problemType(problem) === "urn:nm:problem:last-owner") return LAST_OWNER;
  return problemMessage(problem, fallback);
}

function day(iso: string): string {
  return new Date(iso).toLocaleDateString();
}

export default function Members() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return (
    <>
      <MemberList id={workspace.id} myRole={workspace.role} />
      <Invites id={workspace.id} myRole={workspace.role} />
    </>
  );
}

function MemberList({ id, myRole }: { id: string; myRole: Role }) {
  const client = useQueryClient();
  const me = useMe();
  // One form-level alert, not an `aria-describedby` of a select: a refused removal and a refused
  // role change are about the row, not about the value in any one control.
  const [error, setError] = useState("");

  const members = useQuery({
    queryKey: ["members", id],
    queryFn: async () => {
      const { data, error: problem } = await api.GET("/api/v1/workspaces/{id}/members", {
        params: { path: { id } },
      });
      if (problem || !data) throw new Error("could not load the members");
      // shortcut: the first page only (50 members). Add "Load more" the way the audit log has
      // it the day a workspace outgrows that.
      return data.data;
    },
    retry: false,
  });

  const refresh = () => client.invalidateQueries({ queryKey: ["members", id] });

  const changeRole = useMutation({
    mutationFn: async (change: { userId: string; role: Role }) => {
      const { error: problem } = await api.PATCH("/api/v1/workspaces/{id}/members/{user_id}", {
        params: { path: { id, user_id: change.userId } },
        body: { role: change.role },
      });
      if (problem) throw new Error(failure(problem, "That role could not be changed."));
    },
    onSuccess: async () => {
      setError("");
      await refresh();
    },
    onError: (refused: Error) => setError(refused.message),
  });

  const remove = useMutation({
    mutationFn: async (userId: string) => {
      const { error: problem } = await api.DELETE("/api/v1/workspaces/{id}/members/{user_id}", {
        params: { path: { id, user_id: userId } },
      });
      if (problem) throw new Error(failure(problem, "That member could not be removed."));
      return userId;
    },
    onSuccess: async (userId) => {
      setError("");
      // Leaving takes away the workspace this screen is about, and `/me` is what every screen
      // reads it from, so start again from the top rather than render a workspace we just left.
      if (userId === me.data?.id) {
        window.location.assign("/");
        return;
      }
      await refresh();
    },
    onError: (refused: Error) => setError(refused.message),
  });

  return (
    <section className={CARD}>
      <h2 className="text-xl font-semibold">Members</h2>
      {error ? <Alert>{error}</Alert> : null}
      {members.isPending ? <p className="mt-4">Loading…</p> : null}
      {members.data ? (
        <table className="mt-4 w-full text-left">
          <thead>
            <tr className="border-b border-gray-300 dark:border-gray-700">
              <th scope="col">Email</th>
              <th scope="col">Name</th>
              <th scope="col">Role</th>
              <th scope="col">Joined</th>
              <th scope="col">
                <span className="sr-only">Actions</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {members.data.map((member) => (
              <tr className="border-b border-gray-300 dark:border-gray-700" key={member.user_id}>
                <td className="py-2">{member.email}</td>
                <td className="py-2">{member.display_name}</td>
                <td className="py-2">
                  <select
                    className={SELECT}
                    aria-label={`Role for ${member.email}`}
                    value={member.role}
                    disabled={!ROLES.some((role) => canAssign(myRole, member.role, role))}
                    onChange={(event) =>
                      changeRole.mutate({
                        userId: member.user_id,
                        role: event.target.value as Role,
                      })
                    }
                  >
                    {ROLES.map((role) => (
                      <option
                        key={role}
                        value={role}
                        disabled={role !== member.role && !canAssign(myRole, member.role, role)}
                      >
                        {role}
                      </option>
                    ))}
                  </select>
                </td>
                <td className="py-2">{day(member.joined_at)}</td>
                <td className="py-2">
                  <ConfirmButton
                    action={member.user_id === me.data?.id ? "Leave" : "Remove"}
                    subject={member.email}
                    onConfirm={() => remove.mutate(member.user_id)}
                  />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : null}
    </section>
  );
}

function Invites({ id, myRole }: { id: string; myRole: Role }) {
  const client = useQueryClient();
  const offerable = ROLES.filter((role) => canAssign(myRole, null, role));
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<Role>(offerable[0] ?? "viewer");
  const [emailError, setEmailError] = useState("");
  const [formError, setFormError] = useState("");
  const [link, setLink] = useState("");
  const [busy, setBusy] = useState(false);

  const invites = useQuery({
    queryKey: ["invites", id],
    queryFn: async () => {
      const { data, error: problem } = await api.GET("/api/v1/workspaces/{id}/invites", {
        params: { path: { id } },
      });
      if (problem || !data) throw new Error("could not load the open invitations");
      return data.data;
    },
    retry: false,
  });

  const revoke = useMutation({
    mutationFn: async (inviteId: string) => {
      const { error: problem } = await api.DELETE(
        "/api/v1/workspaces/{id}/invites/{invite_id}",
        { params: { path: { id, invite_id: inviteId } } },
      );
      if (problem) throw new Error(failure(problem, "That invitation could not be revoked."));
    },
    onSuccess: async () => {
      setFormError("");
      await client.invalidateQueries({ queryKey: ["invites", id] });
    },
    onError: (refused: Error) => setFormError(refused.message),
  });

  async function invite(event: FormEvent) {
    event.preventDefault();
    setEmailError("");
    setFormError("");
    setLink("");
    setBusy(true);
    const { data, error: problem } = await api.POST("/api/v1/workspaces/{id}/invites", {
      params: { path: { id } },
      body: { email, role },
    });
    setBusy(false);
    if (problem || !data) {
      const message = failure(problem, "That invitation could not be sent.");
      // Only the two problems that are about the address go on the address field.
      const kind = problemType(problem);
      if (kind === "urn:nm:problem:validation" || kind === "urn:nm:problem:already-member") {
        setEmailError(message);
      } else {
        setFormError(message);
      }
      return;
    }
    setEmail("");
    setLink(data.invite_url);
    await client.invalidateQueries({ queryKey: ["invites", id] });
  }

  return (
    <section className={CARD}>
      <h2 className="text-xl font-semibold">Invite someone</h2>
      <p className="mt-2">
        Nothing is emailed: send them the link this makes. Anyone holding it can join.
      </p>
      <form onSubmit={invite} noValidate>
        <Field
          id="invite-email"
          label="Email"
          type="email"
          autoComplete="off"
          required
          error={emailError}
          value={email}
          onChange={(event) => setEmail(event.target.value)}
        />
        <div className="mt-4">
          <label className="block font-medium" htmlFor="invite-role">
            Role
          </label>
          <select
            id="invite-role"
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
        {formError ? <Alert>{formError}</Alert> : null}
        <button className={BUTTON} type="submit" disabled={busy || offerable.length === 0}>
          Send invite
        </button>
      </form>

      {link ? (
        <div className="mt-4">
          <p className="font-medium">Send this link to them:</p>
          <p className="mt-1 flex flex-wrap items-center gap-3">
            <a className="break-all font-mono underline" data-testid="invite-url" href={link}>
              {link}
            </a>
            <button
              className={ROW_BUTTON}
              type="button"
              onClick={() => void navigator.clipboard.writeText(link).catch(() => {})}
            >
              Copy
            </button>
          </p>
        </div>
      ) : null}

      <h3 className="mt-6 font-semibold">Open invitations</h3>
      {invites.data && invites.data.length > 0 ? (
        <ul className="mt-2">
          {invites.data.map((open) => (
            <li
              className="flex items-center justify-between gap-3 border-b border-gray-300 py-2 dark:border-gray-700"
              key={open.id}
            >
              <span>
                {open.email} — {open.role}, expires {day(open.expires_at)}
              </span>
              <ConfirmButton
                action="Revoke"
                subject={`invitation for ${open.email}`}
                onConfirm={() => revoke.mutate(open.id)}
              />
            </li>
          ))}
        </ul>
      ) : (
        <p className="mt-2">No open invitations.</p>
      )}
    </section>
  );
}
