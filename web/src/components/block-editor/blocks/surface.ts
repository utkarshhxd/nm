// SPDX-License-Identifier: AGPL-3.0-only

import type { Block, Mark } from "../model";
import type { Sel } from "../rich-text";

/** What the editor hands one block so its editing surface can change the document and report back. */
export type Surface = {
  /**
   * Replaces this block. `burst` names a run of typing that is one undo step; `restore` is where
   * the selection goes after a discrete edit.
   */
  change: (block: Block, how?: { burst?: string; restore?: Sel }) => void;
  focus: () => void;
  select: (sel: Sel) => void;
  mark: (mark: Mark) => void;
  restore: Sel | null;
};
