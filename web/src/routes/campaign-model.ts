// SPDX-License-Identifier: AGPL-3.0-only

/**
 * The campaign shapes as the wire carries them (`.superpowers/sdd/m2c/contract.md` section 2) and
 * the one rule table the screens share: what a role may do to a campaign in a state (section 4).
 *
 * **`actionsFor` hides controls and nothing else.** The server runs the same table on every
 * request; a stale `/me` here is never the reason anything is allowed.
 */

import { type Role, atLeast } from "../lib/roles";

export type CampaignState =
  | "draft"
  | "scheduled"
  | "sending"
  | "paused"
  | "sent"
  | "cancelled"
  | "failed";
export type Mode = "instant" | "recipient_local";

export type Variant = { subject: string; template_id: string; from_name: string | null };

export type Schedule = {
  mode: Mode;
  /** IANA name. */
  zone: string;
  /** `YYYY-MM-DDTHH:MM`, no zone; `null` is "send now". */
  local: string | null;
  /** The server-resolved instant; the earliest zone's for `recipient_local`. */
  send_at: string | null;
};

/** `queued = total - sent - failed - unknown - skipped`, in flight included. */
export type Counts = {
  total: number;
  queued: number;
  sent: number;
  failed: number;
  unknown: number;
  skipped: number;
};

export type Campaign = {
  id: string;
  workspace_id: string;
  name: string;
  state: CampaignState;
  list_ids: string[];
  segment_id: string | null;
  sending_domain_id: string | null;
  from_local_part: string | null;
  from_name: string | null;
  reply_to: string | null;
  /** 1..4 once saved with content; empty on a fresh draft. */
  variants: Variant[];
  /** 10..100, only with 2+ variants; 100 is a pure split. */
  ab_test_percent: number | null;
  winner: number | null;
  awaiting_winner: boolean;
  schedule: Schedule | null;
  counts: Counts;
  failure_reason: string | null;
  pause_reason: string | null;
  created_at: string;
  updated_at: string;
  launched_at: string | null;
  finished_at: string | null;
};

export type CampaignSummary = {
  id: string;
  name: string;
  state: CampaignState;
  send_at: string | null;
  counts: Counts;
  updated_at: string;
};

export type EditableFields = Pick<
  Campaign,
  | "list_ids"
  | "segment_id"
  | "sending_domain_id"
  | "from_local_part"
  | "from_name"
  | "reply_to"
  | "variants"
  | "ab_test_percent"
>;
export type NewCampaign = { name: string } & Partial<EditableFields>;
/** Absent is unchanged; `null` clears. */
export type UpdateCampaign = Partial<EditableFields> & { name?: string };

export type ScheduleRequest = { mode: Mode; zone: string; local: string | null };
/** 1..5 addresses; `variant` defaults to 0. */
export type TestSendRequest = { addresses: string[]; variant?: number };
export type TestSendResult = { queued: number };
export type WinnerRequest = { variant: number };
export type PreviewRequest = { variant?: number };
export type CampaignPreview = { subject: string; html: string; text: string };
/** `null` with `exact: false` is "too many to count quickly". */
export type Estimate = { count: number | null; exact: boolean };

export type Page<T> = { data: T[]; next_cursor: string | null };

/** A pick-list entry for the editor: a list, a segment or a sending domain (its domain name). */
export type Option = { id: string; name: string };
export type TemplateSummary = {
  id: string;
  name: string;
  kind: "blocks" | "mjml" | "html";
  created_at: string;
  updated_at: string;
};
export type Options = {
  lists: Option[];
  segments: Option[];
  sendingDomains: Option[];
  templates: TemplateSummary[];
};

/**
 * What a role may do. `edit` is the table's "edit/delete" column. Preview and estimate are
 * viewer-level in every state, so they are always present.
 */
export type Action =
  | "edit"
  | "preview"
  | "estimate"
  | "test"
  | "send"
  | "unschedule"
  | "pause"
  | "resume"
  | "cancel"
  | "winner";

/** The one state the editor may change. */
export function canEdit(s: CampaignState): boolean {
  return s === "draft";
}

/** A split test whose first phase is done and whose winner nobody has picked. */
export function awaitingWinner(c: Pick<Campaign, "state" | "awaiting_winner" | "winner">): boolean {
  return c.awaiting_winner && c.winner === null && (c.state === "sending" || c.state === "paused");
}

/** Contract section 4, row by row. Order follows the table's columns. */
export function actionsFor(
  s: CampaignState,
  role: Role,
  // A summary has no split-test fields; it passes the nothing-awaited pair.
  c: Pick<Campaign, "awaiting_winner" | "winner">,
): Action[] {
  const editor = atLeast(role, "editor");
  const admin = atLeast(role, "admin");
  const out: Action[] = [];
  if (editor && s === "draft") out.push("edit");
  out.push("preview", "estimate");
  if (editor && (s === "draft" || s === "scheduled")) out.push("test");
  if (admin && s === "draft") out.push("send");
  if (admin && s === "scheduled") out.push("unschedule");
  if (editor && s === "sending") out.push("pause");
  if (admin && s === "paused") out.push("resume");
  if (admin && (s === "scheduled" || s === "sending" || s === "paused")) out.push("cancel");
  if (admin && awaitingWinner({ ...c, state: s })) out.push("winner");
  return out;
}
