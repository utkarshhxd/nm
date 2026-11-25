// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import { problemText } from "../api/client";
import { Button } from "../components/button";
import { ErrorState } from "../components/error-state";
import { Alert, Field } from "../components/field";
import { CONTROL } from "../components/input";
import { Skeleton } from "../components/skeleton";
import { atLeast } from "../lib/roles";
import {
  type Draft,
  LOGO_MESSAGE,
  MIN_CONTRAST,
  PAIRS,
  PALETTE,
  contrast,
  isColour,
  logoAllowed,
  ratioText,
  resync,
  toDraft,
  toKit,
} from "./brand-kit-model";
import { type BrandKit, brandKitSource } from "./brand-kit-source";
import { useWorkspace } from "./workspace-layout";

const KNOWN = {
  "insufficient-role": "Your role cannot change the brand kit.",
};

export default function BrandKitScreen() {
  const { workspace } = useWorkspace();
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return <Page ws={workspace.id} canEdit={atLeast(workspace.role, "admin")} />;
}

function Page({ ws, canEdit }: { ws: string; canEdit: boolean }) {
  const kit = useQuery({
    queryKey: ["brand-kit", ws],
    queryFn: async () => {
      const { data, error } = await brandKitSource.get(ws);
      if (!data) throw new Error(problemText(error, KNOWN, "The brand kit could not be loaded."));
      return data;
    },
    retry: false,
  });
  return (
    <section className="mt-8">
      <h2 className="text-xl font-semibold">Brand kit</h2>
      <p className="mt-2 text-fg-muted">
        The logo, colours, font and footer this workspace applies to its templates. There is one
        brand kit per workspace.
      </p>
      {kit.isPending ? (
        <Skeleton label="Loading the brand kit" rows={4} />
      ) : kit.isError ? (
        <ErrorState title="The brand kit could not be loaded." onRetry={() => void kit.refetch()}>
          {kit.error.message}
        </ErrorState>
      ) : (
        <KitForm ws={ws} kit={kit.data} canEdit={canEdit} />
      )}
    </section>
  );
}

const COLOUR_FLAG = "Use a colour written as #rgb or #rrggbb, such as #1a56db.";

function KitForm({ ws, kit, canEdit }: { ws: string; kit: BrandKit; canEdit: boolean }) {
  const qc = useQueryClient();
  const [draft, setDraft] = useState<Draft>(() => toDraft(kit));
  // A newer copy of the kit (a refetch, a save) replaces the form unless the operator has typed in
  // it. `kit` is also what a save merges onto, so what a newer version wrote is never overwritten
  // from a stale copy; two operators saving at once is last-write-wins, with no etag.
  const [seen, setSeen] = useState(kit);
  if (kit !== seen) {
    setDraft(resync(draft, seen, kit));
    setSeen(kit);
  }
  const set = (key: keyof Draft) => (e: { target: { value: string } }) => {
    setDraft((d) => ({ ...d, [key]: e.target.value }));
    save.reset();
  };

  const save = useMutation({
    mutationFn: async () => {
      // Typed text is sent as typed. A flag is advice; the server is the gate, and its refusal is
      // what the operator is shown.
      const { data, error } = await brandKitSource.put(ws, toKit(kit, draft));
      if (!data) throw new Error(problemText(error, KNOWN, "The brand kit could not be saved."));
      return data;
    },
    onSuccess: (saved) => {
      qc.setQueryData(["brand-kit", ws], saved);
    },
  });

  return (
    <form
      className="mt-4 max-w-xl"
      onSubmit={(e) => {
        e.preventDefault();
        // `busy` does not disable the button, so a second press must be ignored here.
        if (canEdit && !save.isPending) save.mutate();
      }}
    >
      <fieldset disabled={!canEdit}>
        <legend className="sr-only">Brand kit</legend>
        <Field
          id="bk-logo"
          label="Logo address"
          type="text"
          inputMode="url"
          autoComplete="off"
          value={draft.logo}
          onChange={set("logo")}
          error={draft.logo.trim() !== "" && !logoAllowed(draft.logo.trim()) ? LOGO_MESSAGE : undefined}
          hint="The full address of your logo image. Leave it empty for no logo."
        />

        <div role="group" aria-labelledby="bk-colours" className="mt-6">
          <h3 id="bk-colours" className="font-semibold">Colours</h3>
          {PALETTE.map(([key, label]) => (
            <Field
              key={key}
              id={`bk-${key}`}
              label={label}
              type="text"
              autoComplete="off"
              spellCheck={false}
              value={draft[key]}
              onChange={set(key)}
              error={draft[key].trim() !== "" && !isColour(draft[key].trim()) ? COLOUR_FLAG : undefined}
              hint={
                <>
                  Leave it empty to use the default.
                  {isColour(draft[key].trim()) ? (
                    <span
                      aria-hidden="true"
                      className="ms-2 inline-block size-4 rounded-sm border border-border-strong align-middle"
                      style={{ background: draft[key].trim() }}
                    />
                  ) : null}
                </>
              }
            />
          ))}
          <ul className="mt-4 space-y-1">
            {PAIRS.map(([name, fg, bg]) => {
              const [a, b] = [draft[fg].trim(), draft[bg].trim()];
              if (!isColour(a) || !isColour(b)) return null;
              const ratio = contrast(a, b);
              if (ratio >= MIN_CONTRAST) return null;
              // Advice: the kit saves anyway. It is every email, so it is said here, where it is chosen.
              return (
                <li key={name} className="text-warning-text">
                  {name}: contrast {ratioText(ratio)}:1, below the {MIN_CONTRAST}:1 that keeps text
                  readable. Every email in this workspace uses this pair.
                </li>
              );
            })}
          </ul>
        </div>

        <Field
          id="bk-font"
          label="Font stack"
          type="text"
          autoComplete="off"
          spellCheck={false}
          value={draft.font}
          onChange={set("font")}
          hint="Fonts in the order to try them, such as Georgia, 'Times New Roman', serif. A client that has none of them falls back to its own."
        />

        <div className="mt-6">
          <label className="block font-medium" htmlFor="bk-footer-text">
            Footer text
          </label>
          <textarea
            id="bk-footer-text"
            rows={3}
            className={`${CONTROL} mt-1 w-full py-2`}
            value={draft.footerText}
            onChange={set("footerText")}
          />
        </div>
        <div className="mt-4">
          <label className="block font-medium" htmlFor="bk-footer-address">
            Postal address
          </label>
          <textarea
            id="bk-footer-address"
            rows={2}
            className={`${CONTROL} mt-1 w-full py-2`}
            value={draft.footerAddress}
            onChange={set("footerAddress")}
            aria-describedby="bk-footer-address-hint"
          />
          <span id="bk-footer-address-hint" className="mt-1 block text-sm text-fg-muted">
            Many countries require a sender&apos;s postal address in every marketing email.
          </span>
        </div>
      </fieldset>

      {save.error ? <Alert>{save.error.message}</Alert> : null}
      {save.isSuccess ? (
        <p role="status" className="mt-4">Brand kit saved.</p>
      ) : (
        // A live region that is present before it speaks, so the next message is announced.
        <p role="status" className="sr-only" />
      )}
      {canEdit ? (
        <div className="mt-6">
          <Button type="submit" variant="solid" busy={save.isPending}>
            Save brand kit
          </Button>
        </div>
      ) : (
        <p className="mt-4 text-fg-muted">Your role can read the brand kit but not change it.</p>
      )}
    </form>
  );
}
