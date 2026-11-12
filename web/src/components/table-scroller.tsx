// SPDX-License-Identifier: AGPL-3.0-only

import type { ReactNode } from "react";

/**
 * A data table that scrolls inside itself when the screen is narrower than its columns, rather
 * than making the whole page scroll. A scrollable region has to be reachable without a mouse, so
 * it takes a tab stop and a name. Put `wrap-anywhere` on any cell that shows user text: an
 * address or a name with no spaces would otherwise make the table as wide as the text. `relative` makes it the containing block for the
 * `sr-only` column headers, which would otherwise be placed past the scroller and widen the page.
 */
export function TableScroller({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div role="region" aria-label={label} tabIndex={0} className="relative overflow-x-auto">
      {children}
    </div>
  );
}
