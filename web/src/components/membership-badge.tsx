// SPDX-License-Identifier: AGPL-3.0-only

import type { components } from "../api/schema";
import { Badge, type BadgeGlyph, type BadgeVariant } from "./badge";

export type MembershipStatus = components["schemas"]["MembershipStatus"];

const LOOK: Record<MembershipStatus, { text: string; variant: BadgeVariant; glyph: BadgeGlyph }> = {
  subscribed: { text: "Subscribed", variant: "success", glyph: "dot" },
  pending: { text: "Pending", variant: "warning", glyph: "ring" },
  unsubscribed: { text: "Unsubscribed", variant: "neutral", glyph: "slash" },
};

/** A member's standing on a list, as a word and a shape, never a colour alone. */
export function MembershipBadge({ status }: { status: MembershipStatus }) {
  const { text, variant, glyph } = LOOK[status];
  return (
    <Badge variant={variant} glyph={glyph}>
      {text}
    </Badge>
  );
}
