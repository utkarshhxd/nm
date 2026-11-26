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

/** `{ field, message }` entries of a 422 `validation` problem, by field name. */
export function fieldErrors(error: unknown): Record<string, string> {
  const list = (error as { errors?: { field?: unknown; message?: unknown }[] } | null)?.errors;
  const out: Record<string, string> = {};
  for (const e of list ?? []) {
    if (typeof e.field === "string" && typeof e.message === "string") out[e.field] ??= e.message;
  }
  return out;
}

/** `Name <local@domain>`, or `local@domain` with no name; `null` while either half is missing. */
export function fromAddress(name: string, local: string, domain: string): string | null {
  if (local === "" || domain === "") return null;
  return name === "" ? `${local}@${domain}` : `${name} <${local}@${domain}>`;
}

/**
 * The instant a `datetime-local` value is in an IANA zone, as RFC 3339 UTC; `null` for a value or
 * zone that is not one. The confirmation dialog shows this before the server has answered.
 *
 * shortcut: a time inside a DST gap or overlap resolves to one of its two readings, not by the
 * RFC 5545 rule. The server's `send_at`, shown once scheduled, is the authority.
 */
export function resolveLocal(local: string, zone: string): string | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})$/.exec(local);
  if (!m) return null;
  const wall = Date.UTC(+m[1]!, +m[2]! - 1, +m[3]!, +m[4]!, +m[5]!);
  try {
    const fmt = new Intl.DateTimeFormat("en-US", {
      timeZone: zone,
      hourCycle: "h23",
      year: "numeric",
      month: "numeric",
      day: "numeric",
      hour: "numeric",
      minute: "numeric",
    });
    /** The zone's offset from UTC, in ms, at instant `t`. */
    const offset = (t: number) => {
      const p = Object.fromEntries(fmt.formatToParts(t).map((x) => [x.type, Number(x.value)]));
      return Date.UTC(p["year"]!, p["month"]! - 1, p["day"]!, p["hour"]!, p["minute"]!) - t;
    };
    // The offset at the guess may differ from the one at the answer across a DST change: ask twice.
    const first = wall - offset(wall);
    return new Date(wall - offset(first)).toISOString();
  } catch {
    return null;
  }
}

/** What the editor's inputs hold: every value a string, `""` where the wire has `null`. */
export type EditorForm = {
  list_ids: string[];
  segment_id: string;
  sending_domain_id: string;
  from_local_part: string;
  from_name: string;
  reply_to: string;
  variants: { subject: string; template_id: string; from_name: string }[];
  ab_test_percent: string;
};

export const BLANK_VARIANT = { subject: "", template_id: "", from_name: "" };

/** A fresh draft has no variants; the editor always shows at least one to fill in. */
export function initForm(c: Campaign): EditorForm {
  return {
    list_ids: c.list_ids,
    segment_id: c.segment_id ?? "",
    sending_domain_id: c.sending_domain_id ?? "",
    from_local_part: c.from_local_part ?? "",
    from_name: c.from_name ?? "",
    reply_to: c.reply_to ?? "",
    variants:
      c.variants.length > 0
        ? c.variants.map((v) => ({ ...v, from_name: v.from_name ?? "" }))
        : [{ ...BLANK_VARIANT }],
    ab_test_percent: c.variants.length > 1 ? String(c.ab_test_percent ?? 100) : "",
  };
}

/**
 * The wire form of the inputs. The percentage goes only with 2+ variants, so a removed second
 * variant cannot leave a stale percentage behind.
 */
export function toFields(f: EditorForm): EditableFields {
  const text = (s: string) => (s.trim() === "" ? null : s.trim());
  return {
    list_ids: f.list_ids,
    segment_id: text(f.segment_id),
    sending_domain_id: text(f.sending_domain_id),
    from_local_part: text(f.from_local_part),
    from_name: text(f.from_name),
    reply_to: text(f.reply_to),
    variants: f.variants.map((v) => ({ subject: v.subject, template_id: v.template_id, from_name: text(v.from_name) })),
    ab_test_percent: f.variants.length > 1 && f.ab_test_percent !== "" ? Number(f.ab_test_percent) : null,
  };
}

/** The sentence for an estimate; `null` with `exact: false` is the server saying it stopped counting. */
export function estimateText(e: Estimate): string {
  if (e.count === null) return "Too many to count quickly";
  const n = `${e.count.toLocaleString("en-GB")} ${e.count === 1 ? "recipient" : "recipients"}`;
  return e.exact ? n : `About ${n}`;
}
