// SPDX-License-Identifier: AGPL-3.0-only

import { useQuery } from "@tanstack/react-query";
import { useState } from "react";

import { problemMessage } from "../../api/client";
import { campaignsKey } from "../../routes/campaigns";
import { campaignSource } from "../../routes/campaign-source";
import { Button } from "../button";
import { Alert } from "../field";
import { SelectField } from "./section";

/**
 * The saved campaign as the server renders it, footer and all. The HTML sits in a sandboxed frame
 * with nothing allowed: no scripts, no same-origin access, no navigation.
 */
export function Preview({
  ws,
  id,
  variantCount,
  savedAt,
}: {
  ws: string;
  id: string;
  variantCount: number;
  /** The campaign's `updated_at`: a save asks for a new rendering. */
  savedAt: string;
}) {
  const [variant, setVariant] = useState(0);
  const [mode, setMode] = useState<"html" | "text">("html");
  const view = useQuery({
    queryKey: [...campaignsKey(ws), id, "preview", variant, savedAt],
    queryFn: async () => {
      const r = await campaignSource.preview(ws, id, { variant });
      if (!r.ok) throw new Error(problemMessage(r.error, "The preview could not be made."));
      return r.data;
    },
    retry: false,
  });
  return (
    <>
      {variantCount > 1 ? (
        <SelectField id="preview-variant" label="Variant to preview" value={variant} onChange={(e) => setVariant(Number(e.target.value))}>
          {Array.from({ length: variantCount }, (_, i) => (
            <option key={i} value={i}>{`Variant ${i + 1}`}</option>
          ))}
        </SelectField>
      ) : null}
      <div className="mt-4 flex flex-wrap gap-3">
        <Button aria-pressed={mode === "html"} variant={mode === "html" ? "solid" : "outline"} onClick={() => setMode("html")}>
          HTML
        </Button>
        <Button aria-pressed={mode === "text"} variant={mode === "text" ? "solid" : "outline"} onClick={() => setMode("text")}>
          Plain text
        </Button>
      </div>
      <div className="mt-4">
        {view.isPending ? (
          <p className="text-fg-muted">Loading the preview…</p>
        ) : view.isError ? (
          <Alert>{view.error.message}</Alert>
        ) : (
          <>
            <p className="mb-3 text-fg-muted wrap-anywhere">Subject: {view.data.subject}</p>
            {mode === "text" ? (
              <pre className="rounded-sm border border-border bg-sunken p-3 font-mono text-sm wrap-anywhere whitespace-pre-wrap">
                {view.data.text}
              </pre>
            ) : (
              <iframe
                title="Campaign preview"
                sandbox=""
                srcDoc={view.data.html}
                className="block h-[32rem] w-full max-w-[640px] rounded-sm border border-border-strong bg-white"
              />
            )}
          </>
        )}
      </div>
    </>
  );
}
