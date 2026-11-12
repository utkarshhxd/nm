// SPDX-License-Identifier: AGPL-3.0-only

import type { components } from "../api/schema";

type Reason = components["schemas"]["Reason"];

/**
 * Why an address is blocked, in words, for every screen that shows one.
 *
 * One definition: the workspace suppressions screen, the instance one and contact detail all
 * render the same reasons, and two copies of this is how "Unsubscribed from everything" becomes
 * two slightly different sentences. `Record<Reason, string>` makes a new server-side reason a
 * type error here rather than a blank cell.
 */
export const REASON: Record<Reason, string> = {
  unsubscribed_all: "Unsubscribed from everything",
  bounced: "Bounced",
  complained: "Complained",
  manual: "Blocked manually",
  gdpr_erasure: "Erased on request (GDPR)",
};
