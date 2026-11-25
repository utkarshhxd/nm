// SPDX-License-Identifier: AGPL-3.0-only

// The block editor shell on its own, for the component tests. The document the editor reports is
// kept on `window.__doc` so a test can assert what would be serialised. Not part of the shipped app.

import { createRoot } from "react-dom/client";

import { BlockEditor } from "../../../src/components/block-editor/editor";
import type { Doc } from "../../../src/components/block-editor/model";
import { type PreviewSource, previewSource } from "../../../src/components/block-editor/preview-source";
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

// ?preview=...: sources for the preview tests. None is the product; each is one way the real one can behave.
const sheet = (css: string, head = "") =>
  `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Email</title>${head}<style>div{height:20px;background:#fff}${css}</style></head>` +
  `<body><div id="a"></div><div id="b"></div><div id="c"></div><div id="d"></div><div id="e"></div></body></html>`;
const ok =
  (html: string): PreviewSource =>
  async () => ({ html, text: "", findings: [] });
const SPELLINGS =
  "#e{background:#000}@media (prefers-color-scheme: light){#e{background:#fff}}" +
  "@media screen and (prefers-color-scheme: dark){#a{background:#000}}" +
  "@media (min-width:1px) and (prefers-color-scheme: dark){#b{background:#000}}" +
  "@media only screen and (prefers-color-scheme: dark) and (max-width:480px){#c{background:#000}}";
const sources: Record<string, PreviewSource> = {
  fail: () => Promise.reject(new Error("down")),
  slow: () => new Promise(() => {}),
  // Fails until a test sets window.__heal.
  flaky: (doc, signal) =>
    (window as unknown as { __heal?: boolean }).__heal ? previewSource(doc, signal) : Promise.reject(new Error("down")),
  spellings: ok(sheet(SPELLINGS, '<style media="(prefers-color-scheme: dark)">#d{background:#000}</style>')),
  nodark: ok(sheet("")),
  leftover: ok(sheet("@media (prefers-color-scheme: dark){#a{background:#000}}@media (prefers-color-scheme){#b{background:#000}}")),
};
const failing = sources[new URLSearchParams(location.search).get("preview") ?? ""];

const container = document.getElementById("root");
if (!container) throw new Error("editor/index.html is missing #root");
(window as unknown as { __doc: Doc }).__doc = initial;
createRoot(container).render(
  <main className="mx-auto max-w-3xl p-6">
    <h1 className="text-xl font-semibold">Editor</h1>
    <BlockEditor initial={initial} source={failing} onChange={(doc) => ((window as unknown as { __doc: Doc }).__doc = doc)} />
  </main>,
);
