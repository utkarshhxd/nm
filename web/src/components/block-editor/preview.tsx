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
      {result && mode === "dark" && (
        <p id={dark} data-testid="dark-coverage" className="border-l-4 border-warning bg-warning-subtle p-3 text-fg">
          This shows the dark styles you wrote, as Apple Mail applies them. Gmail and Outlook apps can change your colours on their own,
          and this preview cannot show that. Send a test to those apps before you rely on dark mode.
        </p>
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
        <p data-testid="findings-count" role="status" className="font-medium">
          {busy ? "Checking…" : findings.length === 0 ? "No findings." : `${findings.length} ${findings.length === 1 ? "finding" : "findings"}.`}
        </p>
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
