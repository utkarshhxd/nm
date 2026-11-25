// SPDX-License-Identifier: AGPL-3.0-only

/**
 * The preview mock, moved out of the shipped tree by task W6 when the real endpoint landed.
 *
 * It is a stand-in with the same shape, for the component tests only: it ships in no bundle and
 * is not what any route talks to. The product's source is `templatePreview` in
 * `src/components/block-editor/preview-source.ts`, which calls the server.
 */

import type { Block, Loc, Run } from "../../../src/components/block-editor/model";
import type { Finding, PreviewSource } from "../../../src/components/block-editor/preview-source";
import { hrefAllowed } from "../../../src/components/block-editor/runs";

const esc = (s: string) => s.replace(/[&<>"]/g, (c) => `&#${c.charCodeAt(0)};`);
const plain = (r: Run) => (r.type === "Tag" ? r.source : r.text);

function inline(r: Run): string {
  if (r.type === "Tag") return esc(r.source);
  let h = esc(r.text).replace(/\n/g, "<br>");
  if (r.marks.includes("bold")) h = `<strong>${h}</strong>`;
  if (r.marks.includes("italic")) h = `<em>${h}</em>`;
  return r.type === "Link" ? `<a href="${hrefAllowed(r.href) ? esc(r.href) : "#"}">${h}</a>` : h;
}

function html(b: Block): string {
  switch (b.type) {
    case "Text":
      return `<p>${b.runs.map(inline).join("")}</p>`;
    case "Heading":
      return `<h${b.level}>${b.runs.map(inline).join("")}</h${b.level}>`;
    case "Image":
      return `<p><img src="${esc(b.src)}" alt="${esc(b.alt)}"></p>`;
    case "Button":
      return `<p><a class="btn" href="${hrefAllowed(b.href) ? esc(b.href) : "#"}">${esc(b.label)}</a></p>`;
    case "Divider":
      return "<hr>";
    case "Spacer":
      return `<div style="height:${b.height}px"></div>`;
    case "Columns":
      return `<div class="cols">${b.children.map((c) => `<div>${c.map(html).join("")}</div>`).join("")}</div>`;
  }
}

function text(b: Block): string {
  switch (b.type) {
    case "Text":
    case "Heading":
      return b.runs.map((r) => (r.type === "Link" ? `${r.text} (${r.href})` : plain(r))).join("");
    case "Image":
      return b.alt ? `[${b.alt}]` : "";
    case "Button":
      return `${b.label}: ${b.href}`;
    case "Divider":
      return "----";
    case "Spacer":
      return "";
    case "Columns":
      return b.children.flat().map(text).filter(Boolean).join("\n\n");
  }
}

const CSS =
  "body{margin:0;padding:16px;font:16px/1.5 sans-serif;background:#fff;color:#1a1a1a}" +
  "a{color:#1a56db}.btn{display:inline-block;padding:10px 20px;border-radius:4px;background:#1a56db;color:#fff;text-decoration:none}" +
  "img{max-width:100%}hr{border:0;border-top:1px solid #767676}.cols{display:flex;gap:12px}.cols>div{flex:1}" +
  "@media (max-width:480px){.cols{display:block}}" +
  "@media (prefers-color-scheme: dark){body{background:#111;color:#eee}a{color:#9ab8ff}.btn{background:#9ab8ff;color:#111}hr{border-top-color:#999}}";

function check(b: Block, at: Loc, out: Finding[]) {
  const bad = (what: string, href: string) => {
    const scheme = /^([a-z][a-z0-9+.-]*):/i.exec(href)?.[1];
    if (href === "") out.push({ check: "link", severity: "error", message: `${what} has no link address.`, at });
    else if (!hrefAllowed(href)) {
      const why = scheme ? `starts with ${scheme}, which email clients block` : "is not an http, https, mailto or tel address";
      out.push({ check: "link", severity: "error", message: `${what} link ${why}.`, at });
    }
  };
  if (b.type === "Image") {
    if (b.src === "") out.push({ check: "lint", severity: "error", message: "Image has no address.", at });
    if (b.alt === "") out.push({ check: "lint", severity: "warning", message: "Image has no description, so it is blank when images are blocked.", at });
  }
  if (b.type === "Button") bad("Button", b.href);
  if (b.type === "Text" || b.type === "Heading") for (const r of b.runs) if (r.type === "Link") bad("A link in the text", r.href);
  if (b.type === "Columns") b.children.forEach((c, i) => c.forEach((child, j) => check(child, [...at, i, j], out)));
}

export const mockPreview: PreviewSource = async (doc, signal) => {
  signal.throwIfAborted();
  const findings: Finding[] = [];
  doc.blocks.forEach((b, i) => check(b, [i], findings));
  return {
    html: `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Email</title><style>${CSS}</style></head><body>${doc.blocks.map(html).join("")}</body></html>`,
    text: doc.blocks.map(text).filter(Boolean).join("\n\n"),
    findings,
  };
};
