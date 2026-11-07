// SPDX-License-Identifier: AGPL-3.0-only

// The guard on the harness's own destructive step. `dropLeaked` force-drops every name in its
// ledger, so what it will and will not accept is worth asserting rather than reading. Needs no
// browser and no server.

import { expect, test } from "@playwright/test";

import { isOurDatabase } from "./support/server";

test("only this harness's own database names are sweepable", () => {
  expect(isOurDatabase("nm_e2e_0123456789ab")).toBe(true);
  expect(isOurDatabase("nm_e2e_ffffffffffff")).toBe(true);

  const refused = [
    // The databases a corrupted or hand-edited ledger could otherwise aim a DROP at.
    "nm",
    "postgres",
    "template1",
    "oci_email",
    // Plausible-looking but not the generated shape: wrong alphabet, wrong length, a suffix.
    "nm_e2e_zzz",
    "nm_e2e_zzzzzzzzzzzz",
    "nm_e2e_0123456789",
    "nm_e2e_0123456789abc",
    "nm_e2e_0123456789ab_old",
    "xnm_e2e_0123456789ab",
    "NM_E2E_0123456789AB",
    // Anchoring: a newline must not let a second name ride along.
    "nm_e2e_0123456789ab\nnm",
    'nm"; drop database nm; --',
    "",
    " nm_e2e_0123456789ab",
  ];
  const actual = Object.fromEntries(refused.map((name) => [name, isOurDatabase(name)]));
  expect(actual).toEqual(Object.fromEntries(refused.map((name) => [name, false])));
});
