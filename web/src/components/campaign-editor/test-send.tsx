// SPDX-License-Identifier: AGPL-3.0-only

import { useMutation } from "@tanstack/react-query";
import { useState } from "react";

import { problemMessage } from "../../api/client";
import { fieldErrors } from "../../routes/campaign-model";
import { campaignSource } from "../../routes/campaign-source";
import { Button } from "../button";
import { Alert } from "../field";
import { CONTROL } from "../input";
import { SelectField } from "./section";

/** Addresses are split on commas, semicolons and any whitespace. */
const split = (text: string) => text.split(/[\s,;]+/).filter(Boolean);

/** Sends the saved campaign to one to five addresses. The server counts them and says so if there are too many. */
export function TestSend({ ws, id, variantCount }: { ws: string; id: string; variantCount: number }) {
  const [text, setText] = useState("");
  const [variant, setVariant] = useState("0");
  const send = useMutation({
    mutationFn: async () => {
      const r = await campaignSource.test(ws, id, { addresses: split(text), variant: Number(variant) });
      if (!r.ok) throw r.error;
      return r.data;
    },
  });
  const field = "addresses" in fieldErrors(send.error);
  return (
    <>
      <p className="mt-4 text-fg-muted">Test emails use the saved campaign. One to five addresses.</p>
      {variantCount > 1 ? (
        <SelectField id="test-variant" label="Variant to test" value={variant} onChange={(e) => setVariant(e.target.value)}>
          {Array.from({ length: variantCount }, (_, i) => (
            <option key={i} value={i}>{`Variant ${i + 1}`}</option>
          ))}
        </SelectField>
      ) : null}
      <label className="mt-4 block font-medium" htmlFor="test-addresses">
        Addresses
      </label>
      <textarea
        id="test-addresses"
        rows={3}
        className={`${CONTROL} mt-1`}
        value={text}
        aria-invalid={field ? true : undefined}
        aria-describedby={send.error ? "test-addresses-error" : undefined}
        onChange={(e) => setText(e.target.value)}
      />
      {send.error ? <Alert id="test-addresses-error">{problemMessage(send.error, "The test could not be sent.")}</Alert> : null}
      <div className="mt-4 flex flex-wrap items-center gap-3">
        <Button
          busy={send.isPending}
          onClick={() => {
            if (!send.isPending) send.mutate();
          }}
        >
          Send test
        </Button>
        <span role="status" className="text-fg-muted">
          {send.data ? `Test queued to ${send.data.queued} ${send.data.queued === 1 ? "address" : "addresses"}.` : ""}
        </span>
      </div>
    </>
  );
}
