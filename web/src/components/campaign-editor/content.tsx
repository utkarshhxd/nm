// SPDX-License-Identifier: AGPL-3.0-only

import { BLANK_VARIANT, type EditorForm, type TemplateSummary } from "../../routes/campaign-model";
import { Button } from "../button";
import { Alert, Field } from "../field";
import { SelectField } from "./section";

const MAX_VARIANTS = 4;

/** One to four variants (a split test from two), each a subject, a template and an optional from name. */
export function Content({
  templates,
  form,
  set,
  errors,
}: {
  templates: TemplateSummary[];
  form: EditorForm;
  set: (patch: Partial<EditorForm>) => void;
  errors: Record<string, string>;
}) {
  const { variants } = form;
  const edit = (i: number, patch: Partial<EditorForm["variants"][number]>) =>
    set({ variants: variants.map((v, j) => (j === i ? { ...v, ...patch } : v)) });
  return (
    <>
      {variants.map((v, i) => (
        <fieldset key={i} className="mt-4 min-w-0 rounded-sm border border-border p-4">
          <legend className="px-1 font-medium">{`Variant ${i + 1}`}</legend>
          <Field
            id={`variant-${i}-subject`}
            label={`Variant ${i + 1} subject`}
            type="text"
            autoComplete="off"
            value={v.subject}
            error={errors[`variants[${i}].subject`]}
            onChange={(e) => edit(i, { subject: e.target.value })}
          />
          <SelectField
            id={`variant-${i}-template`}
            label={`Variant ${i + 1} template`}
            error={errors[`variants[${i}].template_id`]}
            value={v.template_id}
            onChange={(e) => edit(i, { template_id: e.target.value })}
          >
            <option value="">Choose a template</option>
            {templates.map((t) => (
              <option key={t.id} value={t.id}>
                {t.name}
              </option>
            ))}
          </SelectField>
          <Field
            id={`variant-${i}-from-name`}
            label={`Variant ${i + 1} from name`}
            hint="Optional. Leave empty to use the sender name."
            type="text"
            autoComplete="off"
            value={v.from_name}
            onChange={(e) => edit(i, { from_name: e.target.value })}
          />
          {variants.length > 1 ? (
            <Button
              className="mt-4"
              variant="danger-ghost"
              onClick={() =>
                set({
                  variants: variants.filter((_, j) => j !== i),
                })
              }
            >
              {`Remove variant ${i + 1}`}
            </Button>
          ) : null}
        </fieldset>
      ))}
      {errors["variants"] ? <Alert>{errors["variants"]}</Alert> : null}
      {variants.length < MAX_VARIANTS ? (
        <Button
          className="mt-4"
          onClick={() =>
            set({
              variants: [...variants, { ...BLANK_VARIANT }],
              // 100 splits everyone between the variants; a smaller share is a test with a winner to pick.
              ab_test_percent: form.ab_test_percent === "" ? "100" : form.ab_test_percent,
            })
          }
        >
          Add variant
        </Button>
      ) : null}
      {variants.length > 1 ? (
        <Field
          id="campaign-ab-percent"
          label="A/B test percentage"
          hint="100 splits everyone between the variants. Less sends to that share first, then you pick the winner for the rest."
          type="number"
          min={10}
          max={100}
          value={form.ab_test_percent}
          error={errors["ab_test_percent"]}
          onChange={(e) => set({ ab_test_percent: e.target.value })}
        />
      ) : null}
    </>
  );
}
