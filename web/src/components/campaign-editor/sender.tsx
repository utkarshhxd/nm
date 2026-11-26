// SPDX-License-Identifier: AGPL-3.0-only

import type { EditorForm, Option } from "../../routes/campaign-model";
import { Field } from "../field";
import { SelectField } from "./section";

/** The From address and where replies go. */
export function Sender({
  domains,
  form,
  set,
  errors,
}: {
  domains: Option[];
  form: EditorForm;
  set: (patch: Partial<EditorForm>) => void;
  errors: Record<string, string>;
}) {
  return (
    <>
      <SelectField
        id="campaign-domain"
        label="Domain"
        error={errors["sending_domain_id"]}
        value={form.sending_domain_id}
        onChange={(e) => set({ sending_domain_id: e.target.value })}
      >
        <option value="">Choose a sending domain</option>
        {domains.map((d) => (
          <option key={d.id} value={d.id}>
            {d.name}
          </option>
        ))}
      </SelectField>
      <Field
        id="campaign-local-part"
        label="Local part"
        hint="The part before the @, such as hello."
        type="text"
        autoComplete="off"
        value={form.from_local_part}
        error={errors["from_local_part"]}
        onChange={(e) => set({ from_local_part: e.target.value })}
      />
      <Field
        id="campaign-from-name"
        label="Sender name"
        type="text"
        autoComplete="off"
        value={form.from_name}
        onChange={(e) => set({ from_name: e.target.value })}
      />
      <Field
        id="campaign-reply-to"
        label="Reply-to"
        type="email"
        autoComplete="off"
        value={form.reply_to}
        error={errors["reply_to"]}
        onChange={(e) => set({ reply_to: e.target.value })}
      />
    </>
  );
}
