// SPDX-License-Identifier: AGPL-3.0-only

// The block editor shell on its own, for the component tests. The document the editor reports is
// kept on `window.__doc` so a test can assert what would be serialised. Not part of the shipped app.

import { createRoot } from "react-dom/client";

import { BlockEditor } from "../../../src/components/block-editor/editor";
import type { Doc } from "../../../src/components/block-editor/model";
import "../../../src/styles.css";

const fixture: Doc = {
  blocks: [
    { type: "Heading", level: 1, runs: [{ type: "Text", text: "Welcome", marks: [] }] },
    {
      type: "Text",
      runs: [
        { type: "Text", text: "Hi ", marks: [] },
        { type: "Tag", source: '{{ first_name | "there" }}' },
        { type: "Text", text: "!", marks: ["bold"] },
      ],
    },
    {
      type: "Columns",
      children: [
        [{ type: "Text", runs: [{ type: "Text", text: "left", marks: [] }] }],
        [{ type: "Button", label: "Read more", href: "https://example.com" }],
      ],
    },
  ],
};

// ?doc=<json>: a document the editor did not make, e.g. one a server stored, which need not be normalised.
const given = new URLSearchParams(location.search).get("doc");
const initial: Doc = given ? (JSON.parse(given) as Doc) : fixture;

// ?preview=fail: a source whose render and checks fail, to see the editor not claim a clean bill.
const failing = new URLSearchParams(location.search).get("preview") === "fail" ? () => Promise.reject(new Error("down")) : undefined;

const container = document.getElementById("root");
if (!container) throw new Error("editor/index.html is missing #root");
(window as unknown as { __doc: Doc }).__doc = initial;
createRoot(container).render(
  <main className="mx-auto max-w-3xl p-6">
    <h1 className="text-xl font-semibold">Editor</h1>
    <BlockEditor initial={initial} source={failing} onChange={(doc) => ((window as unknown as { __doc: Doc }).__doc = doc)} />
  </main>,
);
