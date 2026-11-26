// SPDX-License-Identifier: AGPL-3.0-only

import { useMemo } from "react";

import type { Mode } from "../../routes/campaign-model";
import { Field } from "../field";

/** The schedule as typed. It is sent with the campaign, not saved on its own. */
export type Sched = { when: "now" | "at"; local: string; zone: string; mode: Mode };

/** Now or at a wall-clock time in a zone, and whether everyone gets it at once or at their own local time. */
export function Schedule({
  sched,
  set,
  variantCount,
  errors,
}: {
  sched: Sched;
  set: (patch: Partial<Sched>) => void;
  variantCount: number;
  errors: Record<string, string>;
}) {
  const zones = useMemo(() => Intl.supportedValuesOf("timeZone"), []);
  const split = variantCount > 1;
  const mode = split ? "instant" : sched.mode;
  const radio = "flex items-center gap-2";
  return (
    <>
      <fieldset className="mt-4 min-w-0">
        <legend className="font-medium">When</legend>
        <label className={radio}>
          <input type="radio" name="when" className="m-0.5 size-5 accent-accent" checked={sched.when === "now"} onChange={() => set({ when: "now" })} />
          Send now
        </label>
        <label className={radio}>
          <input type="radio" name="when" className="m-0.5 size-5 accent-accent" checked={sched.when === "at"} onChange={() => set({ when: "at" })} />
          Send at
        </label>
      </fieldset>
      {sched.when === "at" ? (
        <>
          <Field
            id="campaign-send-local"
            label="Date and time"
            type="datetime-local"
            value={sched.local}
            error={errors["schedule.local"]}
            onChange={(e) => set({ local: e.target.value })}
          />
          <Field
            id="campaign-send-zone"
            label="Zone"
            hint="An IANA zone such as Europe/London."
            type="text"
            autoComplete="off"
            list="campaign-zones"
            value={sched.zone}
            error={errors["schedule.zone"]}
            onChange={(e) => set({ zone: e.target.value })}
          />
          <datalist id="campaign-zones">
            {zones.map((z) => (
              <option key={z} value={z} />
            ))}
          </datalist>
        </>
      ) : null}
      <fieldset className="mt-4 min-w-0">
        <legend className="font-medium">How</legend>
        <label className={radio}>
          <input type="radio" name="mode" className="m-0.5 size-5 accent-accent" checked={mode === "instant"} onChange={() => set({ mode: "instant" })} />
          The same moment for everyone
        </label>
        <label className={radio}>
          <input
            type="radio"
            name="mode"
            className="m-0.5 size-5 accent-accent"
            checked={mode === "recipient_local"}
            disabled={split}
            aria-describedby={split ? "campaign-mode-reason" : undefined}
            onChange={() => set({ mode: "recipient_local" })}
          />
          Each recipient&apos;s local time
        </label>
        {split ? (
          <p id="campaign-mode-reason" className="text-sm text-fg-muted">
            A split test sends at one moment for everyone, so it cannot also follow each recipient&apos;s
            local time. Remove variants to use it.
          </p>
        ) : null}
        {errors["schedule.mode"] ? <p className="mt-1 text-danger-text">{errors["schedule.mode"]}</p> : null}
      </fieldset>
    </>
  );
}
