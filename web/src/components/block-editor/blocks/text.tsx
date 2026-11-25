// SPDX-License-Identifier: AGPL-3.0-only

import type { Block } from "../model";
import { RichText } from "../rich-text";
import type { Surface } from "./surface";

// Not an <h1>-<h6>: a template's heading must not become part of the editor page's outline.
const SIZE = ["text-3xl", "text-2xl", "text-xl", "text-lg", "text-base", "text-sm"];

/** Text and Heading: one contenteditable over the block's runs. */
export function TextBlock({ block, label, s }: { block: Extract<Block, { runs: unknown }>; label: string; s: Surface }) {
  const heading = block.type === "Heading";
  return (
    <RichText
      runs={block.runs}
      label={label}
      className={heading ? `${SIZE[block.level - 1] ?? "text-xl"} font-semibold` : ""}
      restore={s.restore}
      onRuns={(runs, restore) =>
        s.change({ ...block, runs }, restore ? { restore } : { burst: "text" })
      }
      onSelect={s.select}
      onFocus={s.focus}
      onMark={s.mark}
    />
  );
}
