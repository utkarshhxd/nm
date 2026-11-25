// SPDX-License-Identifier: AGPL-3.0-only

import type { Block } from "../model";
import type { Surface } from "./surface";

/** The button as the reader sees it, with its label editable in place. The address is in the panel. */
export function ButtonBlock({ block, label, s }: { block: Extract<Block, { type: "Button" }>; label: string; s: Surface }) {
  return (
    <input
      aria-label={`${label} label`}
      value={block.label}
      onFocus={s.focus}
      onChange={(e) => s.change({ ...block, label: e.target.value }, { burst: "label" })}
      className="field-sizing-content min-w-24 rounded-sm bg-accent px-4 py-2 font-medium text-on-accent"
    />
  );
}
