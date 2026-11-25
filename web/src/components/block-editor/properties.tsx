// SPDX-License-Identifier: AGPL-3.0-only

import { useId, useState } from "react";

import { Button } from "../button";
import { Field } from "../field";
import { Select } from "../select";
import type { Block, Mark } from "./model";
import type { Sel } from "./rich-text";
import { hrefAllowed, hrefAt, marked } from "./runs";
import { TagInsert } from "./tag-token";

/** What the panel can do to the text under the selection. Each returns an error to show, or null. */
export type TextTools = {
  sel: Sel | null;
  mark: (mark: Mark) => void;
  link: (href: string | null) => string | null;
  tag: (source: string) => void;
};

type Props = {
  /** The selected block, or null. The panel edits this block and no other. */
  block: Block | null;
  /** Names the block, so a field is never reused by the next block that is selected. */
  id: string;
  change: (block: Block, burst?: string) => void;
  text: TextTools;
  addTo: (column: number, type: Block["type"]) => void;
};

const ADDABLE: Block["type"][] = ["Text", "Heading", "Image", "Button", "Divider", "Spacer"];
const MAX_COLUMNS = 4;

function TextProperties({ block, text }: { block: Extract<Block, { runs: unknown }>; text: TextTools }) {
  const id = useId();
  const [href, setHref] = useState("");
  const [error, setError] = useState<string>();
  const sel = text.sel && text.sel.from !== text.sel.to ? text.sel : null;
  const current = sel ? hrefAt(block.runs, sel.from, sel.to) : null;
  const keep = (e: { preventDefault(): void }) => e.preventDefault(); // keeps the caret and selection in the text
  return (
    <div role="group" aria-label="Text formatting" className="space-y-3">
      <div className="flex flex-wrap gap-2">
        {(["bold", "italic"] as const).map((m) => (
          <Button
            key={m}
            disabled={!sel}
            aria-pressed={sel ? marked(block.runs, sel.from, sel.to, m) : false}
            onMouseDown={keep}
            onClick={() => text.mark(m)}
          >
            {m === "bold" ? "Bold" : "Italic"}
          </Button>
        ))}
      </div>
      {!sel && <p className="text-sm text-fg-muted">Select some text to make it bold, italic or a link.</p>}
      <div>
        <Field
          id={`${id}-href`}
          label="Link address"
          value={href}
          disabled={!sel}
          error={error}
          hint="http, https, mailto or tel"
          onChange={(e) => setHref(e.target.value)}
        />
        <div className="mt-2 flex gap-2">
          <Button
            disabled={!sel}
            onClick={() => {
              const problem = text.link(href.trim());
              setError(problem ?? undefined);
              if (!problem) setHref("");
            }}
          >
            Apply link
          </Button>
          {current !== null && (
            <Button
              onClick={() => {
                setError(undefined);
                text.link(null);
              }}
            >
              Remove link
            </Button>
          )}
        </div>
        {current !== null && <p className="mt-1 text-sm text-fg-muted wrap-anywhere">Linked to {current}</p>}
      </div>
      <TagInsert onInsert={text.tag} />
    </div>
  );
}

/** The selected block's properties. Every control here writes one `update` to that one block. */
export function Properties({ block, id, change, text, addTo }: Props) {
  const adding = useId();
  const [kind, setKind] = useState<Block["type"]>("Text");
  if (!block) return <p className="text-fg-muted">Select a block to edit its properties.</p>;
  const f = (name: string) => `${id}-${name}`;
  return (
    <div role="group" aria-label={`${block.type} properties`}>
      <h2 className="text-lg font-semibold">{block.type} block</h2>
      {block.type === "Heading" && (
        <div className="mt-4">
          <label htmlFor={f("level")} className="block font-medium">
            Heading level
          </label>
          <Select
            id={f("level")}
            className="mt-1"
            value={block.level}
            onChange={(e) => change({ ...block, level: Number(e.target.value) })}
          >
            {[1, 2, 3, 4, 5, 6].map((n) => (
              <option key={n} value={n}>
                Heading {n}
              </option>
            ))}
          </Select>
        </div>
      )}
      {(block.type === "Text" || block.type === "Heading") && <TextProperties key={id} block={block} text={text} />}
      {block.type === "Image" && (
        <>
          <Field id={f("src")} label="Image address" value={block.src} onChange={(e) => change({ ...block, src: e.target.value }, "src")} />
          <Field
            id={f("alt")}
            label="Description"
            hint="Read aloud, and shown when the image is blocked."
            value={block.alt}
            onChange={(e) => change({ ...block, alt: e.target.value }, "alt")}
          />
        </>
      )}
      {block.type === "Button" && (
        <>
          <Field id={f("label")} label="Label" value={block.label} onChange={(e) => change({ ...block, label: e.target.value }, "label")} />
          <Field
            id={f("href")}
            label="Link address"
            hint="http, https, mailto or tel"
            value={block.href}
            error={block.href !== "" && !hrefAllowed(block.href) ? "Use an address that starts with http, https, mailto or tel." : undefined}
            onChange={(e) => change({ ...block, href: e.target.value }, "href")}
          />
          {/* Said, not shown: no preview we can build draws Outlook's Word engine. */}
          <p role="note" data-testid="outlook-button-note" className="mt-3 text-sm text-fg-muted">
            In classic Outlook on Windows this button is drawn with square corners, whatever its corners look like in the preview. The
            preview cannot show that.
          </p>
        </>
      )}
      {block.type === "Spacer" && (
        <Field
          id={f("height")}
          label="Height in pixels"
          type="number"
          min={0}
          max={65535}
          value={block.height}
          onChange={(e) => change({ ...block, height: Math.min(65535, Math.max(0, Math.trunc(Number(e.target.value)) || 0)) }, "height")}
        />
      )}
      {block.type === "Divider" && <p className="mt-2 text-fg-muted">A divider has no properties.</p>}
      {block.type === "Columns" && (
        <div className="mt-4 space-y-3">
          <p>
            {block.children.length} {block.children.length === 1 ? "column" : "columns"}
          </p>
          <div className="flex flex-wrap gap-2">
            <Button
              disabled={block.children.length >= MAX_COLUMNS}
              onClick={() => change({ ...block, children: [...block.children, []] })}
            >
              Add column
            </Button>
            {/* Never drops blocks: a column with content cannot be removed. */}
            <Button
              disabled={block.children.length <= 1 || (block.children.at(-1)?.length ?? 0) > 0}
              onClick={() => change({ ...block, children: block.children.slice(0, -1) })}
            >
              Remove last column
            </Button>
          </div>
          <div>
            <label htmlFor={adding} className="block font-medium">
              Block to add
            </label>
            <Select id={adding} className="mt-1" value={kind} onChange={(e) => setKind(e.target.value as Block["type"])}>
              {ADDABLE.map((t) => (
                <option key={t}>{t}</option>
              ))}
            </Select>
          </div>
          <div className="flex flex-wrap gap-2">
            {block.children.map((_, c) => (
              <Button key={c} onClick={() => addTo(c, kind)}>
                Add to column {c + 1}
              </Button>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
