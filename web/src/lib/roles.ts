// SPDX-License-Identifier: AGPL-3.0-only

import type { components } from "../api/schema";

export type Role = components["schemas"]["Role"];

/** Least to most privileged, the order `Role` is declared in on the server. */
export const ROLES: readonly Role[] = ["viewer", "editor", "admin", "owner"];

/** Whether `role` is at least `min`; mirrors the server's `role >= min` comparisons. */
export function atLeast(role: Role, min: Role): boolean {
  return ROLES.indexOf(role) >= ROLES.indexOf(min);
}

/**
 * Whether `actor` may put someone in `target`, where `current` is their role now and `null` means
 * a new member. Mirrors `can_assign` in `src/workspaces/roles.rs`: an owner may do anything; an
 * admin may set viewer, editor or admin, but only on someone who is not an owner; nobody below
 * admin manages anyone.
 *
 * **This disables controls and nothing else.** The server runs the same rule on every request and
 * is the only thing deciding it; a stale `/me` here must never be the reason a change is allowed.
 */
export function canAssign(actor: Role, current: Role | null, target: Role): boolean {
  if (actor === "owner") return true;
  if (actor === "admin") return target !== "owner" && current !== "owner";
  return false;
}
