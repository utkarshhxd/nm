// SPDX-License-Identifier: AGPL-3.0-only

// The block editor shell on its own, for the component tests. The document the editor reports is
// kept on `window.__doc` so a test can assert what would be serialised. Not part of the shipped app.

import { createRoot } from "react-dom/client";

import { BlockEditor } from "../../../src/components/block-editor/editor";
import type { Doc } from "../../../src/components/block-editor/model";
import "../../../src/styles.css";

const initial: Doc = {
  blocks: [
    { type: "Heading", level: 1, runs: [{ type: "Plain", text: "Welcome" }] },
    {
      type: "Text",
      runs: [
        { type: "Plain", text: "Hi " },
        { type: "Tag", source: 'first_name | "there"' },
        { type: "Bold", text: "!" },
      ],
    },
    {
      type: "Columns",
      children: [
        [{ type: "Text", runs: [{ type: "Plain", text: "left" }] }],
        [{ type: "Button", label: "Read more", href: "https://example.com" }],
      ],
    },
  ],
};

const container = document.getElementById("root");
if (!container) throw new Error("editor/index.html is missing #root");
(window as unknown as { __doc: Doc }).__doc = initial;
createRoot(container).render(
  <main className="mx-auto max-w-3xl p-6">
    <h1 className="text-xl font-semibold">Editor</h1>
    <BlockEditor initial={initial} onChange={(doc) => ((window as unknown as { __doc: Doc }).__doc = doc)} />
  </main>,
);
