// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useLayoutEffect, useRef } from "react";

import type { Mark, Run } from "./model";
import { edit, hasTag, length, normalize } from "./runs";
import { isTagToken, tagToken } from "./tag-token";

/** A selection as flat offsets into the block's runs; a merge tag counts 1. `from <= to`. */
export type Sel = { from: number; to: number };

/*
 * The one place the DOM is read back. The document is the source of truth and the DOM is only its
 * paint: `paint` writes runs out, `parse` reads one block's runs back after the browser has edited
 * the text, and nothing else is ever taken from the DOM. A merge tag is a non-editable element, so
 * the browser cannot put a caret inside it; `parse` and the deletion handler treat it as one unit.
 */

function paint(root: HTMLElement, runs: Run[]) {
  const last = runs.at(-1);
  root.replaceChildren(
    ...runs.map((r): Node => {
      if (r.type === "Tag") return tagToken(r.source);
      let node: Node = document.createTextNode(r.text);
      for (const m of [...r.marks].reverse()) {
        const wrap = document.createElement(m === "bold" ? "strong" : "em");
        wrap.append(node);
        node = wrap;
      }
      if (r.type !== "Link") return node;
      // Not an <a>: the href is operator input and this is an editing surface, not a destination.
      const link = document.createElement("span");
      link.dataset["href"] = r.href;
      link.title = r.href;
      link.className = "underline";
      link.append(node);
      return link;
    }),
    // A line break at the very end of pre-wrapped text shows no new line until something follows
    // it, so the caret would sit on the old line. `parse` ignores the <br>.
    ...(last && last.type !== "Tag" && last.text.endsWith("\n") ? [document.createElement("br")] : []),
  );
}

function parse(root: HTMLElement): Run[] {
  const out: Run[] = [];
  const walk = (parent: Node, marks: Mark[], href: string | undefined) => {
    for (const child of parent.childNodes) {
      if (child instanceof Text) {
        out.push(href === undefined ? { type: "Text", text: child.data, marks } : { type: "Link", href, text: child.data, marks });
      } else if (isTagToken(child)) {
        out.push({ type: "Tag", source: child.dataset["source"] ?? child.textContent ?? "" });
      } else if (child instanceof HTMLElement) {
        const next = [...marks];
        if (child.tagName === "STRONG" || child.tagName === "B") next.push("bold");
        if (child.tagName === "EM" || child.tagName === "I") next.push("italic");
        walk(child, next, child.dataset["href"] ?? href);
      }
    }
  };
  walk(root, [], undefined);
  return normalize(out);
}

/** The text nodes and merge-tag elements of `node`, in document order. A tag's own text is not one. */
function leaves(node: Node, out: Node[] = []): Node[] {
  for (const child of node.childNodes) {
    if (child instanceof Text || isTagToken(child)) out.push(child);
    else leaves(child, out);
  }
  return out;
}

const indexOf = (node: Node) => Array.prototype.indexOf.call(node.parentNode?.childNodes ?? [], node) as number;

function toFlat(root: HTMLElement, node: Node, offset: number): number {
  // The caret is never inside a tag; if a browser reports it so, it is before or after the tag.
  const tag = isTagToken(node) ? node : node.parentElement?.closest<HTMLElement>("[data-tag]");
  if (tag) [node, offset] = [tag.parentNode as Node, indexOf(tag) + (offset > 0 || node !== tag ? 1 : 0)];
  const range = document.createRange();
  range.setStart(root, 0);
  range.setEnd(node, offset);
  let n = 0;
  for (const leaf of leaves(root)) {
    if (leaf === node) return n + offset;
    const [end, at] = leaf instanceof Text ? [leaf, leaf.data.length] : [leaf.parentNode as Node, indexOf(leaf) + 1];
    if (range.comparePoint(end, at) > 0) break;
    n += leaf instanceof Text ? leaf.data.length : 1;
  }
  return n;
}

function point(root: HTMLElement, flat: number): [Node, number] {
  let rest = flat;
  for (const leaf of leaves(root)) {
    if (leaf instanceof Text) {
      if (rest <= leaf.data.length) return [leaf, rest];
      rest -= leaf.data.length;
    } else {
      if (rest === 0) return [leaf.parentNode as Node, indexOf(leaf)];
      rest -= 1;
    }
  }
  return [root, root.childNodes.length];
}

function selection(root: HTMLElement): Sel | null {
  const s = document.getSelection();
  if (!s?.anchorNode || !s.focusNode || !root.contains(s.anchorNode) || !root.contains(s.focusNode)) return null;
  const a = toFlat(root, s.anchorNode, s.anchorOffset);
  const b = toFlat(root, s.focusNode, s.focusOffset);
  return { from: Math.min(a, b), to: Math.max(a, b) };
}

function select(root: HTMLElement, sel: Sel) {
  const range = document.createRange();
  range.setStart(...point(root, sel.from));
  range.setEnd(...point(root, sel.to));
  const s = document.getSelection();
  s?.removeAllRanges();
  s?.addRange(range);
}

type Props = {
  runs: Run[];
  label: string;
  className: string;
  /** Set by an edit the model made (a formatting change, a deleted tag): where the selection goes next. */
  restore: Sel | null;
  /** `restore` is present when the change is a discrete edit, absent when it is typing. */
  onRuns: (runs: Run[], restore?: Sel) => void;
  onSelect: (sel: Sel) => void;
  onFocus: () => void;
  /** Enter: split the block at the selection. Shift+Enter is a line break and is handled here. */
  onEnter: (runs: Run[], sel: Sel) => void;
  onMark: (mark: Mark) => void;
};

/** One text block's editing surface: a contenteditable painted from `runs`, read back on input. */
export function RichText(props: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const painted = useRef("");
  const latest = useRef(props);
  latest.current = props;

  useLayoutEffect(() => {
    const el = ref.current as HTMLDivElement;
    const key = JSON.stringify(props.runs);
    // What the DOM already shows is not repainted: that would move the caret on every keystroke.
    if (key !== painted.current) {
      paint(el, props.runs);
      painted.current = key;
    }
    if (props.restore) {
      el.focus();
      select(el, props.restore);
    }
  }, [props.runs, props.restore]);

  useEffect(() => {
    const el = ref.current as HTMLDivElement;
    const report = () => {
      const s = selection(el);
      if (s) latest.current.onSelect(s);
    };
    // The selectionchange for the last key can arrive after focus has moved to the panel, where it
    // is no longer in this block: so the selection is also taken the moment the block loses focus.
    el.addEventListener("focusout", report);
    // Deleting next to or across a merge tag is the model's: one step, the whole tag, every browser.
    const beforeInput = (e: InputEvent) => {
      if (e.inputType !== "deleteContentBackward" && e.inputType !== "deleteContentForward") return;
      const s = selection(el);
      if (!s) return;
      let { from, to } = s;
      if (from === to) {
        if (e.inputType === "deleteContentBackward") from -= 1;
        else to += 1;
      }
      const runs = parse(el);
      if (from < 0 || to > length(runs) || !hasTag(runs, from, to)) return;
      e.preventDefault();
      latest.current.onRuns(edit(runs, from, to), { from, to: from });
    };
    document.addEventListener("selectionchange", report);
    el.addEventListener("beforeinput", beforeInput);
    return () => {
      document.removeEventListener("selectionchange", report);
      el.removeEventListener("beforeinput", beforeInput);
      el.removeEventListener("focusout", report);
    };
  }, []);

  return (
    <div
      ref={ref}
      role="textbox"
      aria-multiline="true"
      aria-label={props.label}
      contentEditable
      suppressContentEditableWarning
      spellCheck
      className={`min-h-6 whitespace-pre-wrap wrap-anywhere ${props.className}`}
      onFocus={props.onFocus}
      onInput={(e) => {
        const runs = parse(e.currentTarget);
        // The DOM already shows what the user typed: record it as painted so it is not painted again.
        painted.current = JSON.stringify(runs);
        props.onRuns(runs);
      }}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          const runs = parse(e.currentTarget);
          const sel = selection(e.currentTarget) ?? { from: length(runs), to: length(runs) };
          // A line break is a "\n" in the text, shown as a break because the block is pre-wrapped.
          if (e.shiftKey) props.onRuns(edit(runs, sel.from, sel.to, [{ type: "Text", text: "\n", marks: [] }]), { from: sel.from + 1, to: sel.from + 1 });
          else props.onEnter(runs, sel);
        }
        const mark = e.key === "b" ? "bold" : e.key === "i" ? "italic" : null;
        if (mark && (e.ctrlKey || e.metaKey)) {
          e.preventDefault();
          props.onMark(mark);
        }
      }}
      onPaste={(e) => {
        // Plain text only, on one line: pasted markup would bring formatting the model cannot hold.
        e.preventDefault();
        const text = e.clipboardData.getData("text/plain").replace(/\s*[\r\n]+\s*/g, " ");
        const runs = parse(e.currentTarget);
        const { from, to } = selection(e.currentTarget) ?? { from: length(runs), to: length(runs) };
        const at = from + text.length;
        props.onRuns(edit(runs, from, to, [{ type: "Text", text, marks: [] }]), { from: at, to: at });
      }}
    />
  );
}
