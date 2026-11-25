// SPDX-License-Identifier: AGPL-3.0-only

import { useEffect, useId, useState } from "react";

import { Button } from "../button";
import type { Doc, Loc } from "./model";
import type { PreviewResult, PreviewSource } from "./preview-source";

type Mode = "desktop" | "mobile" | "dark" | "text";
const MODES: [Mode, string][] = [
  ["desktop", "Desktop"],
  ["mobile", "Mobile"],
  ["dark", "Dark"],
  ["text", "Plain text"],
];

/** A pause in editing this long (ms) before the server is asked again. */
const DEBOUNCE = 250;

/**
 * What a dark preview can honestly reproduce: the author's own `prefers-color-scheme: dark` CSS.
 * The query is rewritten to always (or never) match, so the frame shows the same thing whatever
 * the operator's own system theme is. A client that recolours the email itself cannot be shown
 * by a browser, and there is no colour inversion here on purpose: a guess would be a lie.
 * shortcut: a regex over the server's HTML, fine for the one spelling MJML emits; use a parse if that changes.
 */
const themed = (html: string, dark: boolean) =>
  html.replace(/@media\s*\(\s*prefers-color-scheme\s*:\s*dark\s*\)/gi, dark ? "@media all" : "@media not all");

type State = { result: PreviewResult | null; failed: boolean; busy: boolean };

/**
 * The preview and the checks, kept up to date as the document changes: the operator reads a
 * finding while composing, not after a save. If the source fails, nothing is claimed: no preview
 * and no "no findings".
 */
export function Preview({ doc, source, onLocate }: { doc: Doc; source: PreviewSource; onLocate: (at: Loc) => void }) {
  const [mode, setMode] = useState<Mode>("desktop");
  const [state, setState] = useState<State>({ result: null, failed: false, busy: true });
  const dark = useId();
  useEffect(() => {
    const ctl = new AbortController();
    setState((s) => ({ ...s, busy: true }));
    const t = setTimeout(() => {
      source(doc, ctl.signal).then(
        (result) => ctl.signal.aborted || setState({ result, failed: false, busy: false }),
        () => ctl.signal.aborted || setState({ result: null, failed: true, busy: false }),
      );
    }, DEBOUNCE);
    return () => {
      clearTimeout(t);
      ctl.abort();
    };
  }, [doc, source]);

  const { result, failed, busy } = state;
  const findings = result?.findings ?? [];
  const summary = findings.length === 0 ? "No findings." : `${findings.length} ${findings.length === 1 ? "finding" : "findings"}.`;
  return (
    <section aria-label="Preview and checks" className="space-y-3 border-t border-border pt-4">
      <h2 className="text-lg font-semibold">Preview and checks</h2>
      <div role="group" aria-label="Preview mode" className="flex flex-wrap gap-2">
        {MODES.map(([m, name]) => (
          <Button key={m} variant={m === mode ? "solid" : "outline"} aria-pressed={m === mode} onClick={() => setMode(m)}>
            {name}
          </Button>
        ))}
      </div>
      <p data-testid="preview-scope" className="text-sm text-fg-muted">
        Drawn by your browser, not by an email client. Use it for layout and wording, then send a test to the apps your readers use.
      </p>
      {failed && (
        <p role="alert" className="text-danger-text">
          Preview and checks unavailable. Nothing has been checked, so do not read the empty list as a pass.
        </p>
      )}
      {result && mode !== "text" && (
        <p data-testid="preview-images" className="text-sm text-fg-muted">
          Remote images are not loaded in this preview. Each one shows as its description.
        </p>
      )}
      {result && mode === "dark" && (
        <div id={dark} data-testid="dark-coverage" className="space-y-2 border-l-4 border-warning bg-warning-subtle p-3 text-fg">
          <p>
            Dark mode does one of three things in real email apps. This preview shows only the first: it applies the dark styles you wrote.
          </p>
          <ul className="list-disc space-y-1 pl-5">
            <li>Some apps apply your dark styles, Apple Mail for example.</li>
            <li>
              Some apps recolour the email themselves whatever you wrote, for example several Outlook and Gmail mobile apps. This preview
              cannot show that.
            </li>
            <li>
              Many apps ignore dark styles and show the light version, for example Gmail in a browser, Yahoo and classic Outlook. Some
              rewrite the rule so it never applies. This preview cannot show that either.
            </li>
          </ul>
          <p>Sources disagree about which app does which, and it changes. Send a test to the apps your readers use.</p>
        </div>
      )}
      {result && mode === "text" && (
        <section aria-label="Plain text version" className="rounded-sm border border-border bg-sunken p-3">
          <pre className="font-mono text-sm wrap-anywhere whitespace-pre-wrap">{result.text}</pre>
        </section>
      )}
      {result && mode !== "text" && (
        <iframe
          title="Email preview"
          // Nothing in an email runs in the editor: no scripts, no same-origin access, no navigation.
          sandbox=""
          srcDoc={themed(result.html, mode === "dark")}
          aria-describedby={mode === "dark" ? dark : undefined}
          style={{ width: mode === "mobile" ? 375 : 640 }}
          className="block h-[32rem] max-w-full rounded-sm border border-border-strong bg-white"
        />
      )}
      {!failed && (
        <>
          {/* Sighted people see the in-flight state; assistive technology gets the settled result only. */}
          <p data-testid="findings-count" aria-hidden="true" className="font-medium">
            {busy ? "Checking…" : summary}
          </p>
          <p role="status" className="sr-only">
            {result ? summary : ""}
          </p>
        </>
      )}
      {findings.length > 0 && (
        <ul aria-label="Findings" className="space-y-2">
          {findings.map((f, i) => (
            <li key={`${i}:${f.message}`} className="flex flex-wrap items-center gap-2">
              <span className={`font-medium ${f.severity === "error" ? "text-danger-text" : "text-warning-text"}`}>
                {f.severity === "error" ? "Error" : "Warning"}
              </span>
              <span>{f.message}</span>
              <span className="text-sm text-fg-muted">{f.check === "lint" ? "lint" : "link check"}</span>
              {f.at && (
                <Button variant="ghost" aria-label={`Show block ${f.at.map((n) => n + 1).join(".")}`} onClick={() => onLocate(f.at as Loc)}>
                  Show block
                </Button>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
