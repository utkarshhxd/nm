// SPDX-License-Identifier: AGPL-3.0-only

/**
 * THE preview's one view of the server: the three types below and the one call at the bottom.
 * Nothing else in the editor knows an API path, and the panel knows only `PreviewSource`.
 *
 * The shape is this lane's, kept because the panel is written against it. `Finding.rule` is on
 * the wire (contract section 12) and is dropped here: nothing on this screen hangs off it yet,
 * and a field no reader has is a field that rots. Add it when a help link or a suppression does.
 */

import { api, problemText } from "../../api/client";
import type { Doc, Loc } from "./model";

export type Finding = {
  check: "lint" | "link";
  severity: "error" | "warning";
  message: string;
  /** The block it is about, so the panel can take the operator there. `null` when it is about the whole email. */
  at: Loc | null;
};

export type PreviewResult = {
  /** A whole HTML document. Its dark styles must be in `@media (prefers-color-scheme: dark)`. */
  html: string;
  /** What a client that shows only the plain-text part will show. */
  text: string;
  findings: Finding[];
};

/** Rejects when the render or the checks could not run. The panel then says so; it never reads that as "no findings". */
export type PreviewSource = (doc: Doc, signal: AbortSignal) => Promise<PreviewResult>;

/**
 * The sentences for the refusals a preview of UNSAVED work can get. A rejection's message is
 * shown under the panel's "unavailable" alert. A code not listed here shows the server's own
 * sentence: for a `422 validation` that is the field's message, which names the merge tag that
 * will not compile. `409 template-stale` is not listed because it cannot happen here — it comes
 * only from previewing what is STORED (an empty body), and these sources always send a document.
 */
export const PREVIEW_PROBLEMS = {
  "not-found": "This template no longer exists. It may have been deleted in another window.",
  "insufficient-role": "Your role cannot preview this template.",
  // 120 a minute for the whole workspace: two people composing at once can reach it, and the
  // one who does has done nothing wrong.
  "rate-limited": "Previewing is busy for this workspace — it will catch up in a moment.",
  // The checks could not RUN. This is not an empty findings list, and the panel must not show it
  // as one; it reaches the panel as a rejection for exactly that reason.
  "template-uncheckable": "This template could not be checked, so nothing has been checked.",
  // Only the stored preview can get this. The server's own sentence says "save it again", which a
  // viewer cannot do.
  "template-stale":
    "This template's stored copy is out of date. Someone who can edit it needs to open it and save it again.",
};

/**
 * One preview of UNSAVED work, compiled fresh against the brand kit and the field definitions as
 * they are now. Nothing is written. Every refusal rejects: returning `{ findings: [] }` for one
 * would be read as a clean pass.
 */
async function ask(
  ws: string,
  id: string,
  body: { tree: Doc } | { source: string } | Record<string, never>,
  signal: AbortSignal,
): Promise<PreviewResult> {
  const { data, error } = await api.POST(
    "/api/v1/workspaces/{workspace_id}/templates/{id}/preview",
    { params: { path: { workspace_id: ws, id } }, body, signal },
  );
  if (!data) {
    throw new Error(problemText(error, PREVIEW_PROBLEMS, "The preview could not be built."));
  }
  return {
    html: data.html,
    text: data.text,
    // `at` is a PATH into the block tree, never an index (contract section 12); absent means the
    // finding is about the whole email.
    findings: data.findings.map((f) => ({
      check: f.check,
      severity: f.severity,
      message: f.message,
      at: f.at ?? null,
    })),
  };
}

/**
 * The STORED rendition, read-only: an empty body is the server's stored-preview mode, which takes
 * a viewer. This is the only preview a role below editor gets, and it can answer `409
 * template-stale` where the unsaved ones cannot.
 */
export const storedPreview =
  (ws: string, id: string) =>
  (_doc: null, signal: AbortSignal): Promise<PreviewResult> =>
    ask(ws, id, {}, signal);

/** The block tree in the editor. */
export const templatePreview =
  (ws: string, id: string): PreviewSource =>
  (doc, signal) =>
    ask(ws, id, { tree: doc }, signal);

/** The MJML or HTML of a hand-written template, as it is in the text box. */
export const sourcePreview =
  (ws: string, id: string) =>
  (source: string, signal: AbortSignal): Promise<PreviewResult> =>
    ask(ws, id, { source }, signal);
