// SPDX-License-Identifier: AGPL-3.0-only

import { createHmac, randomUUID } from "node:crypto";

import { signingKey } from "./server";

/**
 * The link a subscriber's email would carry, minted by the harness.
 *
 * **Why the harness has to mint one at all.** `src/public/token.rs::issue` is called from exactly
 * one place in the product — `src/public/forms.rs::enqueue_confirmation`, into a `jobs` payload —
 * and nothing in the API or the CLI reads `jobs`. So a browser cannot be handed a confirmation
 * link. The unsubscribe side is worse: **nothing in the product issues a `Purpose::Unsubscribe`
 * token at all.** The link lives in an email footer, and M1d writes `jobs` rows that no worker
 * drains until M2, so there is no row to read either. A Postgres client in the harness would
 * therefore have reached the confirm link and not the preference centre, which is the page this
 * task is mostly about.
 *
 * **What it costs, and what already covers it.** A minted link proves the route, not the issuer:
 * if `enqueue_confirmation` stopped putting a usable token in the payload, nothing here would go
 * red. That exact thing is covered in Rust, through the real HTTP stack, by
 * `tests/public_pages.rs::the_enqueued_job_carries_a_token_that_the_confirm_route_accepts` — it
 * submits the form, reads the queued payload and follows the token to `GET /c/{token}`. What this
 * file adds is a browser on the other end of the same kind of link.
 *
 * It cannot drift silently: the server verifies the signature, so a payload shaped differently
 * from `src/public/token.rs::Token` — a renamed field, a missing `list_id`, a different domain
 * label — is a `422` page and a failing test, not a quiet pass.
 *
 * shortcut: the signing scheme is written out twice, here and in Rust. Delete this file the day
 * anything in the product hands out a link a test can read — an operator-facing queue inspector,
 * or M2's sender.
 */

/** `src/public/token.rs::PUBLIC_DOMAIN`. A different label gives an unrelated key. */
const DOMAIN = "nm:public-token:v1";

/**
 * `src/public/token.rs::Token`, field for field, less the two optional ones it gained in M1d's
 * final wave: `kid` (which key signed the link) and `ref_id` (what sent it). Both are `Option`
 * and both are skipped when absent, so a payload without them is exactly what the product itself
 * issues today — which is the compatibility guarantee this file would notice losing. It is
 * `deny_unknown_fields` on the far side, so a field invented here is a 422.
 */
type Token = {
  workspace_id: string;
  contact_id: string;
  purpose: "confirm" | "unsubscribe";
  /** `Some` only for `confirm`; `parse` refuses either mismatch. */
  list_id: string | null;
  /** Unix seconds. */
  issued_at: number;
  nonce: string;
};

/** `base64url(json).base64url(hmac)`, keyed by `hmac_sha256(master, DOMAIN)`. */
function mint(token: Token): string {
  const master = Buffer.from(signingKey(), "base64");
  if (master.length !== 32) throw new Error(`the signing key is ${master.length} bytes, not 32`);
  const key = createHmac("sha256", master).update(DOMAIN).digest();
  const payload = Buffer.from(JSON.stringify(token), "utf8");
  const mac = createHmac("sha256", key).update(payload).digest();
  return `${payload.toString("base64url")}.${mac.toString("base64url")}`;
}

/** `/c/{token}`: the second half of double opt-in, for one list. */
export function confirmPath(workspace_id: string, contact_id: string, list_id: string): string {
  return `/c/${mint({
    workspace_id,
    contact_id,
    purpose: "confirm",
    list_id,
    issued_at: Math.floor(Date.now() / 1000),
    nonce: randomUUID(),
  })}`;
}

/** `/u/{token}`: the preference centre, the RFC 8058 one-click POST, and the footer link. */
export function unsubscribePath(workspace_id: string, contact_id: string): string {
  return `/u/${mint({
    workspace_id,
    contact_id,
    purpose: "unsubscribe",
    list_id: null,
    issued_at: Math.floor(Date.now() / 1000),
    nonce: randomUUID(),
  })}`;
}
