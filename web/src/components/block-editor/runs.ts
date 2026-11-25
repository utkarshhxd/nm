// SPDX-License-Identifier: AGPL-3.0-only

/**
 * Pure edits on a list of runs, addressed by a flat offset: a character of text counts 1 (a UTF-16
 * unit, which is what the DOM reports) and a merge tag counts 1. Nothing here mutates its input,
 * and a mark list is always rebuilt, never edited, because split runs share one.
 */

import type { Mark, Run } from "./model";

type Inline = Exclude<Run, { type: "Tag" }>;

const MARKS: Mark[] = ["bold", "italic"];
const size = (r: Run) => (r.type === "Tag" ? 1 : r.text.length);
export const length = (runs: Run[]) => runs.reduce((n, r) => n + size(r), 0);

const sameKind = (a: Inline, b: Inline) =>
  a.type === b.type &&
  a.marks.join() === b.marks.join() &&
  (a.type !== "Link" || a.href === (b as typeof a).href);

/**
 * The one canonical spelling of a list of runs: empty runs dropped, neighbours of the same kind
 * merged, marks in a fixed order, and an empty list as a single empty run, the same as `blank`.
 */
export function normalize(runs: Run[]): Run[] {
  const out: Run[] = [];
  for (const r of runs) {
    if (r.type === "Tag") {
      out.push(r);
      continue;
    }
    if (r.text === "") continue;
    const next: Inline = { ...r, marks: MARKS.filter((m) => r.marks.includes(m)) };
    const prev = out.at(-1);
    if (prev && prev.type !== "Tag" && sameKind(prev, next)) out[out.length - 1] = { ...prev, text: prev.text + next.text };
    else out.push(next);
  }
  return out.length ? out : [{ type: "Text", text: "", marks: [] }];
}

/** Cuts the list in two at `at`. A tag is on one side or the other; an emoji is never torn. */
export function splitAt(runs: Run[], at: number): [Run[], Run[]] {
  const target = Math.min(Math.max(at, 0), length(runs));
  let pos = 0;
  for (let i = 0; i < runs.length; i++) {
    const r = runs[i] as Run;
    const n = size(r);
    if (target <= pos + n) {
      let k = target - pos;
      if (r.type === "Tag") {
        return k === 0 ? [runs.slice(0, i), runs.slice(i)] : [runs.slice(0, i + 1), runs.slice(i + 1)];
      }
      // Between a high and a low surrogate is not a place: step past the pair.
      const high = r.text.charCodeAt(k - 1);
      const low = r.text.charCodeAt(k);
      if (high >= 0xd800 && high <= 0xdbff && low >= 0xdc00 && low <= 0xdfff) k++;
      return [
        [...runs.slice(0, i), { ...r, text: r.text.slice(0, k) }],
        [{ ...r, text: r.text.slice(k) }, ...runs.slice(i + 1)],
      ];
    }
    pos += n;
  }
  return [runs, []];
}

/** The runs between two offsets, as pieces (empty ones removed). */
function slice(runs: Run[], from: number, to: number): Run[] {
  const [, rest] = splitAt(runs, from);
  return splitAt(rest, to - from)[0].filter((r) => r.type === "Tag" || r.text !== "");
}

/** Replaces `[from, to)` with `insert`. With nothing to insert, it deletes. */
export function edit(runs: Run[], from: number, to: number, insert: Run[] = []): Run[] {
  const [head] = splitAt(runs, from);
  const [, tail] = splitAt(runs, to);
  return normalize([...head, ...insert, ...tail]);
}

function mapRange(runs: Run[], from: number, to: number, f: (r: Inline) => Run): Run[] {
  const [head, rest] = splitAt(runs, from);
  const [mid, tail] = splitAt(rest, to - from);
  return normalize([...head, ...mid.map((r) => (r.type === "Tag" ? r : f(r))), ...tail]);
}

/** True when the range holds text and every piece of it carries `mark`. */
export function marked(runs: Run[], from: number, to: number, mark: Mark): boolean {
  const pieces = slice(runs, from, to).filter((r): r is Inline => r.type !== "Tag");
  return pieces.length > 0 && pieces.every((r) => r.marks.includes(mark));
}

/** Turns `mark` off when the whole range has it, on otherwise. */
export function toggleMark(runs: Run[], from: number, to: number, mark: Mark): Run[] {
  const on = !marked(runs, from, to, mark);
  return mapRange(runs, from, to, (r) => ({
    ...r,
    marks: on ? [...r.marks, mark] : r.marks.filter((m) => m !== mark),
  }));
}

/** Links the range to `href`, or unlinks it with `null`. A tag in the range is left as it is. */
export function setLink(runs: Run[], from: number, to: number, href: string | null): Run[] {
  return mapRange(runs, from, to, (r): Run => {
    const { text, marks } = r;
    return href === null ? { type: "Text", text, marks } : { type: "Link", href, text, marks };
  });
}

/** The address of the first link in the range, if there is one. */
export function hrefAt(runs: Run[], from: number, to: number): string | null {
  const link = slice(runs, from, to).find((r) => r.type === "Link");
  return link?.type === "Link" ? link.href : null;
}

/** True when `[from, to)` covers a merge tag, which is what makes a deletion the model's to do. */
export const hasTag = (runs: Run[], from: number, to: number) => slice(runs, from, to).some((r) => r.type === "Tag");

/** What an operator typed in the merge-tag field, as a tag's source; `null` when it is not usable. */
export function tagSource(raw: string): string | null {
  const inner = raw.trim().replace(/^\{\{/, "").replace(/\}\}$/, "").trim();
  return inner === "" || /\{\{|\}\}|[\r\n]/.test(inner) ? null : `{{ ${inner} }}`;
}

/** The URL scheme allowlist, frozen: a link is http, https, mailto or tel and nothing else. */
export function hrefAllowed(href: string): boolean {
  try {
    return ["http:", "https:", "mailto:", "tel:"].includes(new URL(href).protocol);
  } catch {
    return false;
  }
}
