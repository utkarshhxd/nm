// SPDX-License-Identifier: AGPL-3.0-only

import type { ReactNode } from "react";

import { ButtonBlock } from "./blocks/button";
import type { Surface } from "./blocks/surface";
import { TextBlock } from "./blocks/text";
import type { Block } from "./model";

/**
 * One block as the editor shows it. Text, Heading and Button edit in place; Image, Divider, Spacer
 * and Columns are edited in the property panel, and show here as placeholders (an operator's image
 * address is not fetched on an editing surface; previews are W4). `columns` is the already-rendered
 * content of each column of a Columns block, because the editor owns selection and so owns the
 * recursion.
 */
export function BlockView({
  block,
  label,
  s,
  columns,
}: {
  block: Block;
  label: string;
  s: Surface;
  columns?: ReactNode[];
}) {
  switch (block.type) {
    case "Text":
    case "Heading":
      return <TextBlock block={block} label={label} s={s} />;
    case "Button":
      return <ButtonBlock block={block} label={label} s={s} />;
    case "Image":
      return (
        <div className="rounded-sm border border-dashed border-border-strong bg-sunken p-4 text-fg-muted">
          {block.alt || "Image with no description"}
        </div>
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
