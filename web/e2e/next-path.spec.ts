// SPDX-License-Identifier: AGPL-3.0-only

// Unit tests for the open-redirect guard. They live here, next to the browser tests, because
// Playwright is the project's only TypeScript test runner (`pnpm test` is `node --test` over
// the plain-JavaScript build scripts). Nothing here needs a browser or the server.
//
// The payloads are the full sign-in URL an attacker would send, not the decoded `next` value,
// so each case covers percent-decoding and URL parsing together — which is where the bug was.

import { expect, test } from "@playwright/test";

import { nextPath } from "../src/lib/navigation";

const ORIGIN = "https://nm.example";
const signInUrl = (next: string) => `${ORIGIN}/login?next=${next}`;

const offsite = [
  "https://evil.example",
  "//evil.example",
  "/\\evil.example",
  "javascript:alert(1)",
  "%2F%2Fevil.example",
  "%2F%5Cevil.example",
  // The regression this file exists for: the URL parser strips ASCII tab, CR and LF before it
  // resolves anything, so each of these parses as `//evil.example`.
  "/%09/evil.example",
  "/%0D/evil.example",
  "/%0A/evil.example",
];

test("an off-site next is refused", () => {
  for (const next of offsite) {
    expect(nextPath(signInUrl(next)), next).toBe("/");
  }
});

test("a same-origin next is kept whole", () => {
  expect(nextPath(signInUrl("/login"))).toBe("/login");
  expect(nextPath(signInUrl("%2Finvite%2Fabc123"))).toBe("/invite/abc123");
  expect(nextPath(signInUrl("%2Fw%2Facme%3Ftab%3Dkeys%23two"))).toBe("/w/acme?tab=keys#two");
  expect(nextPath(`${ORIGIN}/login`)).toBe("/");
  expect(nextPath(signInUrl(""))).toBe("/");
});
