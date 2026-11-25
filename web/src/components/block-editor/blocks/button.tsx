// SPDX-License-Identifier: AGPL-3.0-only

import type { Block } from "../model";
import type { Surface } from "./surface";

/** The button as the reader sees it, with its label editable in place. The address is in the panel. */
export function ButtonBlock({ block, label, s }: { block: Extract<Block, { type: "Button" }>; label: string; s: Surface }) {
  return (
    <div>
      <input
        aria-label={`${label} label`}
        value={block.label}
        onFocus={s.focus}
        onChange={(e) => s.change({ ...block, label: e.target.value }, { burst: "label" })}
        className="field-sizing-content min-w-24 rounded-sm bg-accent px-4 py-2 font-medium text-on-accent"
      />
      {/* Said, not shown: no preview we can build draws Outlook's Word engine. Beside the button so no layout hides it. */}
      <p role="note" data-testid="outlook-button-note" className="mt-1 text-sm text-fg-muted">
        In classic Outlook on Windows this button is drawn with square corners, whatever its corners look like in the preview. The preview
        cannot show that.
      </p>
    </div>
  );
}
