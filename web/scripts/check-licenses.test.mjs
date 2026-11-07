// SPDX-License-Identifier: AGPL-3.0-only

import assert from "node:assert/strict";
import { test } from "node:test";

import { problems } from "./check-licenses.mjs";

test("an empty report fails closed", () => {
  const found = problems({});
  assert.equal(found.length, 1);
  assert.match(found[0], /refusing to pass on an empty report/);
});

test("an allowed license passes", () => {
  assert.deepEqual(problems({ MIT: [{ name: "react" }] }), []);
});

test("a license outside the list is reported with its packages", () => {
  const found = problems({
    MIT: [{ name: "react" }],
    "SSPL-1.0": [{ name: "something" }, { name: "else" }],
  });
  assert.deepEqual(found, ["DENY SSPL-1.0: something, else"]);
});
