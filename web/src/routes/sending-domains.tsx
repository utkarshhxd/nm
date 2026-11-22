// SPDX-License-Identifier: AGPL-3.0-only

import { useInfiniteQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { useRef, useState } from "react";

import { api, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Badge } from "../components/badge";
import { Button } from "../components/button";
import { Checkbox } from "../components/checkbox";
import { ConfirmDialog } from "../components/confirm-dialog";
import { Dialog } from "../components/dialog";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Alert, Field } from "../components/field";
import { CONTROL } from "../components/input";
import { Select } from "../components/select";
import { Skeleton } from "../components/skeleton";
import { TableScroller } from "../components/table-scroller";
import { atLeast } from "../lib/roles";
import { useWorkspace } from "./workspace-layout";

type Domain = components["schemas"]["SendingDomainView"];
type Signing = components["schemas"]["Signing"];
type Check = components["schemas"]["CheckResult"];
type UpdateDomain = components["schemas"]["UpdateSendingDomain"];

const SIGNING: Record<Signing, string> = {
  relay: "The relay signs; NM holds no key",
  app: "NM signs; you paste a private key",
};
const CHECKS = [
  ["spf", "SPF"],
  ["dkim", "DKIM"],
  ["dmarc", "DMARC"],
  ["return_path", "Return-Path"],
] as const;
// OpenSSL 1.x writes `BEGIN RSA PRIVATE KEY` and 3.x `BEGIN PRIVATE KEY`; the API takes either.
const OPENSSL = "openssl genrsa -out dkim-private.pem 2048";
const allPass = (d: Domain) => CHECKS.every(([key]) => d.checks[key].status === "pass");
const when = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });
const PAGE = 100;
const th = "py-2 pe-3 text-start font-medium";
const KNOWN = {
  "sending-domain-taken": "That domain is already set up in this workspace.",
  "sending-domain-changed": "The domain was edited while it was being checked. Check it again.",
  "insufficient-role": "Your role cannot change sending domains. It takes admin.",
  "rate-limited": "Too many checks just now. Wait a minute and check again.",
};

type KeyMode = "keep" | "replace" | "clear";

export default function SendingDomains() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return <Page ws={workspace.id} canEdit={atLeast(workspace.role, "admin")} />;
}

/** One check, as a status word and its failure reason when it has one. */
function CheckLine({ label, result }: { label: string; result: Check }) {
  return (
    <li className="py-0.5">
      <span className="inline-block min-w-24 font-medium">{label}</span>{" "}
      {result.status === "pass" ? (
        <Badge variant="success" glyph="dot">Pass</Badge>
      ) : result.status === "fail" ? (
        <>
          <Badge variant="danger" glyph="cross">Fail</Badge>
          <span className="mt-1 block text-sm wrap-anywhere">{result.reason}</span>
        </>
      ) : (
        <Badge glyph="ring">Not checked</Badge>
      )}
    </li>
  );
}

function Page({ ws, canEdit }: { ws: string; canEdit: boolean }) {
  const qc = useQueryClient();
  const list = useInfiniteQuery({
    queryKey: ["sending-domains", ws],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/sending-domains", {
        params: { path: { workspace_id: ws }, query: { limit: PAGE, cursor: pageParam } },
      });
      if (!data) throw new Error(problemText(error, KNOWN, "The sending domains could not be loaded."));
      return data;
    },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    retry: false,
  });
  const rows = list.data?.pages.flatMap((p) => p.data) ?? [];
  const refresh = () => qc.invalidateQueries({ queryKey: ["sending-domains", ws] });

  const recheck = useMutation({
    mutationFn: async (d: Domain) => {
      const { data, error } = await api.POST(
        "/api/v1/workspaces/{workspace_id}/sending-domains/{id}/check",
        { params: { path: { workspace_id: ws, id: d.id } } },
      );
      if (!data) {
        throw new Error(`${d.domain}: ${problemText(error, KNOWN, "The check could not be run.")}`);
      }
    },
    onSuccess: refresh,
  });

  // `null` is closed, "new" is the create form, a domain is the edit form for it.
  const [editing, setEditing] = useState<Domain | "new" | null>(null);
  const [deleting, setDeleting] = useState<Domain | null>(null);
  const remove = useMutation({
    mutationFn: async (d: Domain) => {
      const { response, error } = await api.DELETE(
        "/api/v1/workspaces/{workspace_id}/sending-domains/{id}",
        { params: { path: { workspace_id: ws, id: d.id } } },
      );
      if (!response.ok) throw new Error(problemText(error, KNOWN, "The domain could not be deleted."));
    },
    onSuccess: async () => {
      setDeleting(null);
      await refresh();
    },
  });

  const add = (
    <Button variant="solid" onClick={() => setEditing("new")}>
      Add sending domain
    </Button>
  );

  return (
    <section className="mt-8">
      <h2 className="text-xl font-semibold">Sending domains</h2>
      <p className="mt-2 text-fg-muted">
        The domains this workspace may send as. Each needs SPF, DKIM, DMARC and a return-path
        record in DNS; mail from a domain that is missing them is likely to land in spam or be
        rejected outright. Re-check after you change a record: results are read from DNS and can
        lag behind your DNS provider.
      </p>
      {canEdit && rows.length > 0 ? <div className="mt-4">{add}</div> : null}
      {recheck.error ? <Alert>{recheck.error.message}</Alert> : null}

      <div className="mt-4">
        {list.isPending ? (
          <Skeleton label="Loading sending domains" rows={3} />
        ) : list.isError && rows.length === 0 ? (
          <ErrorState title="The sending domains could not be loaded." onRetry={() => void list.refetch()}>
            {list.error.message}
          </ErrorState>
        ) : rows.length === 0 ? (
          <EmptyState title="No sending domains" actions={canEdit ? add : undefined}>
            A campaign can only be sent from a domain listed here.
            {canEdit ? "" : " An admin can add one."}
          </EmptyState>
        ) : (
          <>
            <TableScroller label="Sending domains table">
              <table className="w-full">
                <caption className="sr-only">Sending domains</caption>
                <thead>
                  <tr className="border-b border-border">
                    <th scope="col" className={th}>Domain</th>
                    <th scope="col" className={th}>Signing</th>
                    <th scope="col" className={th}>DNS checks</th>
                    {canEdit ? (
                      <th scope="col" className={th}>
                        <span className="sr-only">Actions</span>
                      </th>
                    ) : null}
                  </tr>
                </thead>
                <tbody>
                  {rows.map((d) => (
                    <tr key={d.id} className="border-b border-border align-top">
                      <td className="py-2 pe-3 wrap-anywhere">
                        {d.domain}
                        {d.from_name ? <div className="text-sm text-fg-muted">{d.from_name}</div> : null}
                        {d.return_path_domain ? (
                          <div className="text-sm text-fg-muted">Return-Path: {d.return_path_domain}</div>
                        ) : null}
                      </td>
                      <td className="py-2 pe-3">
                        {d.signing === "app" ? "NM signs" : "The relay signs"}
                        <div className="text-sm text-fg-muted">
                          Selector {d.dkim_selector}
                        </div>
                        <div className="mt-1">
                          {d.signing === "relay" ? (
                            <Badge>No key used</Badge>
                          ) : d.has_dkim_key ? (
                            <Badge variant="success" glyph="dot">Key set</Badge>
                          ) : (
                            <Badge variant="warning" glyph="cross">Key not set</Badge>
                          )}
                        </div>
                        {d.signing === "app" && d.has_dkim_key ? (
                          d.dkim_public_txt ? (
                            <>
                              <p className="mt-2 text-sm">
                                Publish this TXT record at{" "}
                                <code className="wrap-anywhere">
                                  {d.dkim_selector}._domainkey.{d.domain}
                                </code>
                                :
                              </p>
                              <pre className="mt-1 max-w-md rounded-sm bg-sunken p-2 text-sm break-all whitespace-pre-wrap">
                                <code>{d.dkim_public_txt}</code>
                              </pre>
                            </>
                          ) : (
                            <p className="mt-2 text-sm text-danger-text">
                              The stored key could not be read, so there is no record to show.
                              Replace the key.
                            </p>
                          )
                        ) : null}
                      </td>
                      <td className="py-2 pe-3">
                        <ul>
                          {CHECKS.map(([key, label]) => (
                            <CheckLine key={key} label={label} result={d.checks[key]} />
                          ))}
                        </ul>
                        <p className="mt-1 text-sm text-fg-muted">
                          {d.checked_at ? `Checked ${when.format(new Date(d.checked_at))}` : "Never checked"}
                        </p>
                        {allPass(d) ? null : d.send_unverified_acknowledged_at ? (
                          <p className="mt-2 text-sm">
                            <Badge variant="warning" glyph="dot">Sending unverified allowed</Badge>{" "}
                            since {when.format(new Date(d.send_unverified_acknowledged_at))}
                          </p>
                        ) : (
                          <p className="mt-2 text-sm">
                            <Badge variant="danger" glyph="slash">Not allowed to send</Badge> Not every
                            check passes and no admin has accepted that. An admin can allow it by editing the domain.
                          </p>
                        )}
                      </td>
                      {canEdit ? (
                        <td className="py-2">
                          <Button
                            aria-label={`Re-check ${d.domain}`}
                            busy={recheck.isPending && recheck.variables?.id === d.id}
                            onClick={() => recheck.mutate(d)}
                          >
                            Re-check
                          </Button>
                          <Button aria-label={`Edit ${d.domain}`} onClick={() => setEditing(d)}>
                            Edit
                          </Button>
                          <Button
                            variant="danger-ghost"
                            aria-label={`Delete ${d.domain}`}
                            onClick={() => setDeleting(d)}
                          >
                            Delete
                          </Button>
                        </td>
                      ) : null}
                    </tr>
                  ))}
                </tbody>
              </table>
            </TableScroller>
            {list.isFetchNextPageError ? (
              <p role="alert" className="mt-3">More sending domains could not be loaded.</p>
            ) : null}
            {list.hasNextPage ? (
              <Button className="mt-3" busy={list.isFetchingNextPage} onClick={() => void list.fetchNextPage()}>
                Load more sending domains
              </Button>
            ) : null}
          </>
        )}
      </div>

      <DomainDialog ws={ws} editing={editing} onClose={() => setEditing(null)} />

      <ConfirmDialog
        open={deleting !== null}
        onClose={() => {
          setDeleting(null);
          remove.reset();
        }}
        title={`Delete the sending domain ${deleting?.domain ?? ""}`}
        confirmLabel="Delete domain"
        busy={remove.isPending}
        error={remove.error?.message}
        onConfirm={() => deleting && remove.mutate(deleting)}
      >
        <p>
          <strong>{deleting?.domain}</strong> will no longer be somewhere a campaign can be sent
          from, and its stored DKIM key is destroyed. Its DNS records are not touched.
        </p>
        <p>This cannot be undone.</p>
      </ConfirmDialog>
    </section>
  );
}

/**
 * The create and edit form. The DKIM private key is read from its own uncontrolled textarea when
 * the form is sent, so it lives only there: not in component state, not in the mutation's
 * `variables`, and not in a URL. The dialog unmounts its children on close, which drops it. The
 * API never returns a key; `has_dkim_key` is all this screen ever learns about it.
 */
function DomainDialog({
  ws,
  editing,
  onClose,
}: {
  ws: string;
  editing: Domain | "new" | null;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const form = useRef<HTMLFormElement>(null);
  const domain = editing !== null && editing !== "new" ? editing : null;
  const [signing, setSigning] = useState<Signing>("relay");
  const [keyMode, setKeyMode] = useState<KeyMode>("keep");
  const [ack, setAck] = useState(false);
  const [, rerender] = useState(0);
  // A key that is not stored cannot be kept.
  const mode: KeyMode = domain?.has_dkim_key ? keyMode : "replace";

  /**
   * What the form changes on `domain`, and only that: the API resets the DNS verdict and the
   * acknowledgement for any of the four settings it is sent, even with the value it already has.
   * A key chosen for replacement but not yet typed changes nothing.
   */
  const diff = (f: FormData, d: Domain): UpdateDomain => {
    const str = (k: string) => String(f.get(k) ?? "").trim();
    const key = String(f.get("dkim_private_key") ?? "");
    const body: UpdateDomain = {};
    if (str("from_name") !== (d.from_name ?? "")) body.from_name = str("from_name") || null;
    if (signing !== d.signing) body.signing = signing;
    if (str("dkim_selector") !== d.dkim_selector) body.dkim_selector = str("dkim_selector");
    if (str("return_path_domain") !== (d.return_path_domain ?? "")) {
      body.return_path_domain = str("return_path_domain") || null;
    }
    if (signing === "app") {
      if (mode === "clear") body.dkim_private_key = null;
      if (mode === "replace" && key.trim() !== "") body.dkim_private_key = key;
    }
    return body;
  };
  const resetsAck = (b: UpdateDomain) =>
    "signing" in b || "dkim_selector" in b || "return_path_domain" in b || "dkim_private_key" in b;
  // Read at render, and `rerender` on every edit, so the box tells the truth about the save.
  const withdraws = domain && form.current ? resetsAck(diff(new FormData(form.current), domain)) : false;
  const was = domain?.send_unverified_acknowledged_at != null;

  const save = useMutation({
    mutationFn: async () => {
      // Reached late (a request held while offline) after the form was closed: not a save.
      if (!form.current) throw new Error("The form was closed before it was sent.");
      const f = new FormData(form.current);
      const str = (k: string) => String(f.get(k) ?? "").trim();
      const key = String(f.get("dkim_private_key") ?? "");
      if (domain) {
        if (signing === "app" && mode === "replace" && key.trim() === "") {
          throw new Error("Paste the private key, or choose to keep the stored one.");
        }
        const body = diff(f, domain);
        // A change to how the domain signs resets the checks, so there is nothing an
        // acknowledgement could have reviewed: it is never sent with one, and the API withdraws
        // the old one. Otherwise it is sent only when the box changed.
        if (!resetsAck(body) && ack !== was) body.send_unverified_acknowledged = ack;
        if (Object.keys(body).length === 0) return;
        const { data, error } = await api.PATCH(
          "/api/v1/workspaces/{workspace_id}/sending-domains/{id}",
          { params: { path: { workspace_id: ws, id: domain.id } }, body },
        );
        if (!data) throw new Error(problemText(error, KNOWN, "The domain could not be saved."));
      } else {
        const { data, error } = await api.POST("/api/v1/workspaces/{workspace_id}/sending-domains", {
          params: { path: { workspace_id: ws } },
          body: {
            domain: str("domain"),
            signing,
            from_name: str("from_name") || null,
            dkim_selector: str("dkim_selector") || null,
            return_path_domain: str("return_path_domain") || null,
            ...(signing === "app" ? { dkim_private_key: key } : {}),
          },
        });
        if (!data) throw new Error(problemText(error, KNOWN, "The domain could not be added."));
      }
    },
    onSuccess: async () => {
      setKeyMode("keep");
        onClose();
      await qc.invalidateQueries({ queryKey: ["sending-domains", ws] });
    },
  });

  const close = () => {
    setKeyMode("keep");
    save.reset();
    onClose();
  };
  // The signing select is state, so it must start from the domain being edited each time.
  const [seen, setSeen] = useState<Domain | "new" | null>(null);
  if (editing !== seen) {
    setSeen(editing);
    setSigning(domain?.signing ?? "relay");
    setAck(domain?.send_unverified_acknowledged_at != null);
  }

  return (
    <Dialog
      open={editing !== null}
      onClose={close}
      title={domain ? `Edit the sending domain ${domain.domain}` : "Add a sending domain"}
    >
      <form
        ref={form}
        onChange={() => rerender((n) => n + 1)}
        onSubmit={(e) => {
          e.preventDefault();
          // `busy` does not disable the button, so a second press must be ignored here.
          if (!save.isPending) save.mutate();
        }}
      >
        {domain ? null : (
          <Field
            id="sd-domain"
            label="Domain"
            name="domain"
            required
            autoComplete="off"
            hint="The part after the @ in your from address, such as example.com. It cannot be changed later."
          />
        )}
        <Field
          id="sd-from-name"
          label="From name (optional)"
          name="from_name"
          maxLength={100}
          defaultValue={domain?.from_name ?? ""}
          hint="What a campaign from this domain shows as the sender by default."
        />
        <div className="mt-4">
          <label className="block font-medium" htmlFor="sd-signing">
            Who signs the mail with DKIM
          </label>
          <Select
            id="sd-signing"
            className="mt-1 w-full"
            value={signing}
            onChange={(e) => setSigning(e.target.value as Signing)}
          >
            {(Object.keys(SIGNING) as Signing[]).map((s) => (
              <option key={s} value={s}>{SIGNING[s]}</option>
            ))}
          </Select>
        </div>
        <Field
          id="sd-selector"
          label="DKIM selector"
          name="dkim_selector"
          autoComplete="off"
          defaultValue={domain?.dkim_selector ?? "nm1"}
          hint="The record is published at selector._domainkey.your-domain."
        />
        <Field
          id="sd-return-path"
          label="Return-Path domain (optional)"
          name="return_path_domain"
          autoComplete="off"
          defaultValue={domain?.return_path_domain ?? ""}
          hint="A domain whose CNAME carries bounces to the relay: the sending domain or a subdomain of it."
        />
        {signing === "app" ? (
          <>
            {domain?.has_dkim_key ? (
              <div className="mt-4">
                <label className="block font-medium" htmlFor="sd-key-mode">
                  DKIM private key: set
                </label>
                <Select
                  id="sd-key-mode"
                  className="mt-1 w-full"
                  value={mode}
                  onChange={(e) => setKeyMode(e.target.value as KeyMode)}
                >
                  <option value="keep">Keep the stored key</option>
                  <option value="replace">Replace it</option>
                  <option value="clear">Remove it</option>
                </Select>
              </div>
            ) : null}
            {mode === "replace" ? (
              <div className="mt-4">
                <label className="block font-medium" htmlFor="sd-key">
                  {domain?.has_dkim_key ? "New DKIM private key" : "DKIM private key"}
                </label>
                <textarea
                  id="sd-key"
                  name="dkim_private_key"
                  rows={6}
                  required
                  spellCheck={false}
                  autoComplete="off"
                  autoCapitalize="off"
                  className={`${CONTROL} mt-1 w-full py-2 font-mono text-sm`}
                  aria-describedby="sd-key-hint"
                />
                <span id="sd-key-hint" className="mt-1 block text-sm text-fg-muted">
                  NM does not generate keys. Make one on your own machine with this command, then
                  paste the contents of the file (an unencrypted RSA key in PEM form). It is stored
                  encrypted and cannot be read back, here or through the API.
                </span>
                <pre className="mt-2 overflow-x-auto rounded-sm bg-sunken p-2 text-sm">
                  <code>{OPENSSL}</code>
                </pre>
              </div>
            ) : null}
          </>
        ) : null}
        {domain ? (
          <div className="mt-6">
            <label className="flex items-start gap-2">
              <Checkbox
                name="ack"
                checked={withdraws ? false : ack}
                disabled={withdraws}
                onChange={(e) => setAck(e.target.checked)}
              />
              <span>
                <span className="font-medium">
                  Allow sending from this domain even if its DNS checks are not all passing
                </span>
                <span className="mt-1 block text-sm text-fg-muted">
                  Without SPF, DKIM and DMARC that line up, mail from this domain is likely to land
                  in recipients&apos; spam folders, or be rejected outright. A campaign will not send
                  from a domain with a failing check until an admin ticks this. Changing the signing
                  mode, the selector, the return-path domain or the key withdraws it, because it
                  would be an acknowledgement of something else: tick it again after the change.
                  Running a check never withdraws it.
                </span>
                {withdraws ? (
                  <span role="status" className="mt-1 block text-sm font-medium">
                    {was
                      ? "This change withdraws the acknowledgement. Save it, run a check, then tick this again."
                      : "This change resets the DNS checks. Save it first, then tick this."}
                  </span>
                ) : null}
              </span>
            </label>
          </div>
        ) : null}
        {save.error ? <Alert>{save.error.message}</Alert> : null}
        <div className="mt-6 flex flex-wrap justify-end gap-3">
          <Button onClick={close}>Cancel</Button>
          <Button type="submit" variant="solid" busy={save.isPending}>
            {domain ? "Save domain" : "Add domain"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
