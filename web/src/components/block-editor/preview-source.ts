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
 * The sentences for the refusals this call can get. The server's own `detail` is kept wherever it
 * says something this screen could not: a `422 validation` names the merge tag or the field that
 * will not compile, and a `409 template-stale` names which rendition went stale. Both already end
 * in the cure, which for every stale variant is the same one — save the template again.
 */
export const PREVIEW_PROBLEMS = {
  "not-found": "This template no longer exists. It may have been deleted in another window.",
  "insufficient-role": "Your role cannot preview this template.",
  // Shared with the segment preview, 30 a minute for the whole workspace: two people composing
  // at once can reach it, and the one who does has done nothing wrong.
  "rate-limited": "Previewing is busy for this workspace — it will catch up in a moment.",
  // The checks could not RUN. This is not an empty findings list, and the panel must not show it
  // as one; it reaches the panel as a rejection for exactly that reason.
  "template-uncheckable": "This template could not be checked, so nothing has been checked.",
};

/**
 * Previews UNSAVED work: the document in the editor, compiled fresh against the brand kit and the
 * field definitions as they are now. Nothing is written. A body with no `tree` would preview what
 * is STORED instead, which is a different question and not one this panel asks.
 */
export function templatePreview(ws: string, id: string): PreviewSource {
  return async (doc, signal) => {
    const { data, error } = await api.POST(
      "/api/v1/workspaces/{workspace_id}/templates/{id}/preview",
      { params: { path: { workspace_id: ws, id } }, body: { tree: doc }, signal },
    );
    // Every refusal rejects. Returning `{ findings: [] }` for one would be read as a clean pass.
    if (!data) {
      throw new Error(problemText(error, PREVIEW_PROBLEMS, "The preview could not be built."));
    }
    return {
      html: data.html,
      text: data.text,
      // `at` is a PATH into the block tree, never an index (contract section 12); absent means
      // the finding is about the whole email.
      findings: data.findings.map((f) => ({
        check: f.check,
        severity: f.severity,
        message: f.message,
        at: f.at ?? null,
      })),
    };
  };
}
