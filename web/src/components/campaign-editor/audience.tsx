// SPDX-License-Identifier: AGPL-3.0-only

import type { EditorForm, Options } from "../../routes/campaign-model";
import { Checkbox } from "../checkbox";
import { Alert } from "../field";
import { SelectField } from "./section";

/** Lists (one or more), an optional segment, and how many people that reaches. */
export function Audience({
  options,
  form,
  set,
  errors,
  estimate,
}: {
  options: Options;
  form: EditorForm;
  set: (patch: Partial<EditorForm>) => void;
  errors: Record<string, string>;
  /** The sentence for the saved audience's estimate, already worded. */
  estimate: string;
}) {
  const toggle = (id: string, on: boolean) =>
    set({ list_ids: on ? [...form.list_ids, id] : form.list_ids.filter((x) => x !== id) });
  return (
    <>
      <fieldset className="mt-4 min-w-0">
        <legend className="font-medium">Lists</legend>
        {options.lists.map((l) => (
          <label key={l.id} className="flex items-center gap-2 wrap-anywhere">
            <Checkbox checked={form.list_ids.includes(l.id)} onChange={(e) => toggle(l.id, e.target.checked)} />
            {l.name}
          </label>
        ))}
        {errors["list_ids"] ? <Alert>{errors["list_ids"]}</Alert> : null}
      </fieldset>
      <SelectField
        id="campaign-segment"
        label="Segment"
        error={errors["segment_id"]}
        value={form.segment_id}
        onChange={(e) => set({ segment_id: e.target.value })}
      >
        <option value="">No segment: everyone on the lists</option>
        {options.segments.map((s) => (
          <option key={s.id} value={s.id}>
            {s.name}
          </option>
        ))}
      </SelectField>
      <p className="mt-4 text-fg-muted">
        Reaches: <span className="font-medium text-fg">{estimate}</span> (the saved audience)
      </p>
    </>
  );
}
