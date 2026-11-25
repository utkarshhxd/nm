// SPDX-License-Identifier: AGPL-3.0-only

import type { ReactNode } from "react";

import type { Block, Run } from "./model";

/** Read-only rendering of inline runs. A Tag shows as one chip: it is a token, not text. */
export function Runs({ runs }: { runs: Run[] }) {
  return runs.map((run, i) => {
    switch (run.type) {
      case "Plain":
        return <span key={i}>{run.text}</span>;
      case "Bold":
        return <strong key={i}>{run.text}</strong>;
      case "Italic":
        return <em key={i}>{run.text}</em>;
      // Not an <a>: the href is operator input and this is an editing surface, not a destination.
      case "Link":
        return (
          <u key={i} title={run.href}>
            {run.text}
          </u>
        );
      case "Tag":
        return (
          <span key={i} data-tag className="rounded-sm bg-accent-subtle px-1 font-mono text-sm text-accent-text">
            {`{{ ${run.source} }}`}
          </span>
        );
    }
  });
}

/**
 * One block as the editor shows it. The editing surfaces (a contenteditable per text block, the
 * property panel) replace the bodies below; this is the shell they plug into. `columns` is the
 * already-rendered content of each column of a Columns block, because the editor owns selection
 * and so owns the recursion.
 */
export function BlockView({ block, columns }: { block: Block; columns?: ReactNode[] }) {
  switch (block.type) {
    case "Text":
      return (
        <p className="min-h-6">
          <Runs runs={block.runs} />
        </p>
      );
    case "Heading":
      // Not an <h1>-<h6>: a template's heading must not become part of the editor page's outline.
      return (
        <p className="min-h-8 text-xl font-semibold" data-level={block.level}>
          <Runs runs={block.runs} />
        </p>
      );
    case "Image":
      return (
        <div className="rounded-sm border border-dashed border-border-strong bg-sunken p-4 text-fg-muted">
          {block.alt || "Image with no description"}
        </div>
      );
    case "Button":
      return (
        <span className="inline-block rounded-sm bg-accent px-4 py-2 font-medium text-on-accent">
          {block.label}
        </span>
      );
    case "Divider":
      return <hr className="border-border-strong" />;
    case "Spacer":
      return <div style={{ height: block.height }} className="rounded-sm bg-sunken" />;
    case "Columns":
      return (
        <div className="grid gap-3" style={{ gridTemplateColumns: `repeat(${block.children.length}, 1fr)` }}>
          {columns}
        </div>
      );
  }
}
