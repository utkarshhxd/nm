// SPDX-License-Identifier: AGPL-3.0-only

import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";

import { api, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Button } from "./button";
import { Checkbox } from "./checkbox";
import { Input } from "./input";
import { Select } from "./select";
import { Skeleton } from "./skeleton";

type Definition = components["schemas"]["Definition"];

/**
 * The rule document the server parses (spec M1b section 4.1): a group of leaves and nested
 * groups, never both in one node. The generated types describe `rules` as an opaque object, so
 * these are the shapes, and `asRules` is the one cast between them.
 */
export type Doc = { match: "all" | "any"; rules: Node[] };
export type Node = Leaf | { group: Doc };
type Leaf = { field: string; op: string; value?: unknown; status?: string };

const isGroup = (node: Node): node is { group: Doc } => "group" in node;

/** `rules` is `type: object` in the OpenAPI document, hence `Record<string, never>` in the types. */
export const asRules = (doc: Doc) => doc as unknown as Record<string, never>;
export const asDoc = (rules: Record<string, never>) => rules as unknown as Doc;

/** What a field's operators and value control are typed by: the server's `Operand` (section 4.3). */
type Kind = Definition["kind"] | "list" | "tag" | "suppressed";

export type FieldOption = {
  name: string;
  label: string;
  kind: Kind;
  /** A `select` or `multi_select`'s allowed values. */
  options?: string[];
  /**
   * `NOT NULL` on `contacts`, so `is_set` would be true for every contact and the server refuses
   * both presence operators on it (`Field::never_unset`).
   */
  neverUnset?: boolean;
};

const BUILT_IN: FieldOption[] = [
  { name: "email", label: "Email", kind: "text", neverUnset: true },
  { name: "email_domain", label: "Email domain", kind: "text", neverUnset: true },
  { name: "first_name", label: "First name", kind: "text" },
  { name: "last_name", label: "Last name", kind: "text" },
  { name: "timezone", label: "Time zone", kind: "text" },
  { name: "created_at", label: "Created", kind: "datetime", neverUnset: true },
  { name: "updated_at", label: "Updated", kind: "datetime", neverUnset: true },
  { name: "list", label: "List", kind: "list" },
  { name: "tag", label: "Tag", kind: "tag" },
  { name: "suppressed", label: "Suppressed", kind: "suppressed" },
];

/**
 * The field picker's options: the built-in columns plus one per **live** custom field. An
 * archived definition is left out because a *new* rule may not name one (section 4.2); a stored
 * rule that names one is still shown, read-only, by `RuleRow`.
 */
export function fieldOptions(definitions: Definition[]): FieldOption[] {
  return [
    ...BUILT_IN,
    ...definitions
      .filter((d) => !d.archived_at)
      .map((d) => ({
        name: `attr:${d.key}`,
        label: d.label,
        kind: d.kind,
        options: d.options,
      })),
  ];
}

/** Section 4.3's table, in the order the picker offers them. The server refuses anything else. */
const OPS: Record<Kind, [string, string][]> = {
  text: [
    ["contains", "contains"],
    ["not_contains", "does not contain"],
    ["eq", "is"],
    ["neq", "is not"],
    ["starts_with", "starts with"],
    ["ends_with", "ends with"],
    ["is_set", "has a value"],
    ["is_not_set", "has no value"],
  ],
  number: [
    ["eq", "is"],
    ["neq", "is not"],
    ["gt", "is more than"],
    ["gte", "is at least"],
    ["lt", "is less than"],
    ["lte", "is at most"],
    ["is_set", "has a value"],
    ["is_not_set", "has no value"],
  ],
  date: [
    ["on", "on"],
    ["before", "before"],
    ["after", "after"],
    ["in_last_days", "in the last"],
    ["is_set", "has a value"],
    ["is_not_set", "has no value"],
  ],
  datetime: [
    ["on", "on"],
    ["before", "before"],
    ["after", "after"],
    ["in_last_days", "in the last"],
    ["is_set", "has a value"],
    ["is_not_set", "has no value"],
  ],
  boolean: [
    ["is_true", "is yes"],
    ["is_false", "is no"],
    ["is_set", "has a value"],
    ["is_not_set", "has no value"],
  ],
  select: [
    ["eq", "is"],
    ["neq", "is not"],
    ["is_set", "has a value"],
    ["is_not_set", "has no value"],
  ],
  multi_select: [
    ["has_any", "has any of"],
    ["has_none", "has none of"],
    ["is_set", "has a value"],
    ["is_not_set", "has no value"],
  ],
  list: [
    ["member_of", "is a member of"],
    ["not_member_of", "is not a member of"],
  ],
  tag: [
    ["has", "has"],
    ["not_has", "does not have"],
  ],
  suppressed: [
    ["is_true", "yes"],
    ["is_false", "no"],
  ],
};

const operators = (field: FieldOption): [string, string][] =>
  field.neverUnset
    ? OPS[field.kind].filter(([op]) => op !== "is_set" && op !== "is_not_set")
    : OPS[field.kind];

const VALUELESS = new Set(["is_set", "is_not_set", "is_true", "is_false"]);
const needsValue = (op: string) => !VALUELESS.has(op);

const filled = (value: unknown): boolean =>
  Array.isArray(value) ? value.length > 0 : value !== undefined && value !== null && value !== "";

/** The two operators the server takes an empty string for, where it means "the value is empty". */
const EMPTY_IS_A_VALUE = new Set(["eq", "neq"]);

/**
 * Whether a leaf carries a value **the server would accept** — not merely one this builder likes.
 *
 * The difference is not academic. `eq ""` on a text or select field is a document `check_value`
 * accepts, and calling it unfinished stranded a segment that had one stored: the editor refused
 * to save back what it had just loaded. Everywhere else an empty string is refused by the server
 * too (a pattern must not be empty, a list value must be a uuid, a date must parse), so this
 * agrees with it there as well.
 */
function ready(leaf: Leaf, fields: FieldOption[]): boolean {
  if (!needsValue(leaf.op)) return true;
  if (leaf.value === undefined || leaf.value === null) return false;
  if (Array.isArray(leaf.value)) return leaf.value.length > 0;
  if (leaf.value !== "") return true;
  const kind = fields.find((f) => f.name === leaf.field)?.kind;
  return EMPTY_IS_A_VALUE.has(leaf.op) && (kind === "text" || kind === "select");
}

/**
 * Whether the document is worth sending. An unfinished rule is a `422` the person has not caused
 * yet, and preview is 30/minute/workspace: neither the count nor the quota is spent on one.
 *
 * An empty `rules` is incomplete at every level, which is also why the top group — the one
 * `RuleBuilder` cannot remove — never reaches the server empty.
 */
export function complete(doc: Doc, fields: FieldOption[]): boolean {
  return (
    doc.rules.length > 0 &&
    doc.rules.every((node) =>
      isGroup(node) ? complete(node.group, fields) : ready(node, fields),
    )
  );
}

export const emptyDoc = (): Doc => ({ match: "all", rules: [newRule()] });

const newRule = (): Leaf => ({ field: "email", op: "contains", value: "" });

/** The value a field and operator start on, so a freshly picked rule is never a wrong one. */
function defaultValue(field: FieldOption, op: string): unknown {
  if (!needsValue(op)) return undefined;
  if (op === "in_last_days") return 30;
  if (field.kind === "multi_select") return [];
  return "";
}

export type Catalog = {
  fields: FieldOption[];
  lists: { id: string; name: string }[];
  tags: { id: string; name: string }[];
};

/**
 * The editor for one rule document.
 *
 * Nothing here holds a copy of the document: every change rebuilds it from the top, so what is
 * on screen and what would be saved cannot drift apart.
 */
export function RuleBuilder({
  doc,
  onChange,
  catalog,
}: {
  doc: Doc;
  onChange: (doc: Doc) => void;
  catalog: Catalog;
}) {
  return <GroupEditor group={doc} path="" depth={1} onChange={onChange} catalog={catalog} />;
}

const MAX_DEPTH = 3;

function GroupEditor({
  group,
  path,
  depth,
  onChange,
  onRemove,
  catalog,
}: {
  group: Doc;
  /** "" for the top group, else "2" or "2.1": unique, and what every control is named by. */
  path: string;
  depth: number;
  onChange: (group: Doc) => void;
  onRemove?: () => void;
  catalog: Catalog;
}) {
  const name = path === "" ? "Rules" : `Group ${path}`;
  const suffix = path === "" ? "" : ` to group ${path}`;
  // The ref is on the row of buttons rather than on the button, because `Button` is a function
  // component and a ref reaching it through a spread is not something to rest a focus fix on.
  const actions = useRef<HTMLDivElement>(null);

  const setRules = (rules: Node[]) => onChange({ ...group, rules });
  const replace = (index: number, node: Node) => {
    const next = group.rules.slice();
    next[index] = node;
    setRules(next);
  };

  /**
   * **A group that loses its last rule is removed with it** (section 4.1, section 14). An empty
   * `rules` is `422 empty-rules` at every level, and under `match: "all"` it is vacuously true —
   * it compiles to every contact in the workspace.
   *
   * Removal cascades to **any** depth: a group's `onRemove` is its parent's `remove`, so a group
   * emptied by the group it contained is removed by exactly this code one level up, and again
   * above that. The top group has no `onRemove`, so it is allowed to empty on screen, and
   * `complete` is what stops it reaching the server.
   */
  const remove = (index: number) => {
    const next = group.rules.slice();
    next.splice(index, 1);
    if (next.length === 0 && onRemove) {
      // This group goes with it. Whichever group survives the cascade takes the focus, below.
      onRemove();
      return;
    }
    setRules(next);
    // Where the person carries on from: this group's "Add rule". Without it, focus is left on a
    // Remove button whose row has been rewritten underneath it — the rows are keyed by position,
    // so the next rule's Remove is now under the pointer and one keystroke from firing.
    actions.current?.querySelector("button")?.focus();
  };

  return (
    <div
      role="group"
      aria-label={name}
      className={path === "" ? "" : "mt-3 rounded-md border border-border bg-sunken p-3"}
    >
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-fg-muted">Match</span>
        <Select
          aria-label={path === "" ? "Match" : `Match for group ${path}`}
          value={group.match}
          onChange={(e) => onChange({ ...group, match: e.target.value as Doc["match"] })}
        >
          <option value="all">all</option>
          <option value="any">any</option>
        </Select>
        <span className="text-fg-muted">of these rules</span>
      </div>

      {group.rules.map((node, index) => {
        const child = path === "" ? `${index + 1}` : `${path}.${index + 1}`;
        return isGroup(node) ? (
          <GroupEditor
            key={child}
            group={node.group}
            path={child}
            depth={depth + 1}
            onChange={(next) => replace(index, { group: next })}
            onRemove={() => remove(index)}
            catalog={catalog}
          />
        ) : (
          <RuleRow
            key={child}
            leaf={node}
            path={child}
            onChange={(next) => replace(index, next)}
            onRemove={() => remove(index)}
            catalog={catalog}
          />
        );
      })}

      <div ref={actions} className="mt-3 flex flex-wrap gap-2">
        <Button onClick={() => setRules([...group.rules, newRule()])}>{`Add rule${suffix}`}</Button>
        {/* Deeper than three is `422 rules-too-deep`, so the button is not offered there. */}
        {depth < MAX_DEPTH ? (
          <Button
            onClick={() => setRules([...group.rules, { group: { match: "any", rules: [newRule()] } }])}
          >
            {`Add group${suffix}`}
          </Button>
        ) : null}
        {onRemove ? (
          <Button variant="danger-ghost" onClick={onRemove}>
            {`Remove group ${path}`}
          </Button>
        ) : null}
      </div>
    </div>
  );
}

const STATUSES = ["pending", "subscribed", "unsubscribed"] as const;

function RuleRow({
  leaf,
  path,
  onChange,
  onRemove,
  catalog,
}: {
  leaf: Leaf;
  path: string;
  onChange: (leaf: Leaf) => void;
  onRemove: () => void;
  catalog: Catalog;
}) {
  const field = catalog.fields.find((f) => f.name === leaf.field);

  /**
   * A stored rule naming a field that has since been archived. It is shown rather than hidden and
   * kept exactly as it is: the field picker has no option for it, so offering one would rewrite
   * the rule into a different question the moment anything else on the screen changed. The server
   * accepts the stored document unchanged (section 4.2); removing the rule is the way out.
   */
  if (!field) {
    return (
      <div
        role="group"
        aria-label={`Rule ${path}`}
        className="mt-3 flex flex-wrap items-center gap-2 rounded-md border border-border p-3"
      >
        <span className="wrap-anywhere">
          {leaf.field} {leaf.op} {JSON.stringify(leaf.value ?? "")}
        </span>
        <span className="text-fg-muted">— this field was archived and cannot be edited.</span>
        <Button variant="danger-ghost" aria-label={`Remove rule ${path}`} onClick={onRemove}>
          Remove
        </Button>
      </div>
    );
  }

  const pickField = (name: string) => {
    const next = catalog.fields.find((f) => f.name === name) ?? field;
    const op = operators(next)[0]?.[0] ?? "eq";
    // A new field means a new kind, so neither the old value nor a list status survives it.
    onChange({ field: name, op, value: defaultValue(next, op) });
  };

  // `in_last_days` takes a number of days where the other date operators take a date, so moving
  // between those two is the one change within a field that cannot carry the value over.
  const sameShape = (a: string, b: string) => (a === "in_last_days") === (b === "in_last_days");

  return (
    <div
      role="group"
      aria-label={`Rule ${path}`}
      className="mt-3 flex flex-wrap items-center gap-2 rounded-md border border-border p-3"
    >
      <Select
        aria-label={`Field for rule ${path}`}
        value={leaf.field}
        onChange={(e) => pickField(e.target.value)}
      >
        {catalog.fields.map((f) => (
          <option key={f.name} value={f.name}>
            {f.label}
          </option>
        ))}
      </Select>

      <Select
        aria-label={`Operator for rule ${path}`}
        value={leaf.op}
        onChange={(e) => {
          const op = e.target.value;
          const keep = needsValue(op) && sameShape(leaf.op, op) && filled(leaf.value);
          // An undefined key is absent once serialised, which is what `is_set` and friends need:
          // "this operator takes no value" is a 422.
          onChange({
            ...leaf,
            op,
            value: keep ? leaf.value : defaultValue(field, op),
          });
        }}
      >
        {operators(field).map(([op, label]) => (
          <option key={op} value={op}>
            {label}
          </option>
        ))}
      </Select>

      <ValueControl
        leaf={leaf}
        field={field}
        path={path}
        catalog={catalog}
        onChange={(value) => onChange({ ...leaf, value })}
      />

      {field.kind === "list" ? (
        <Select
          aria-label={`Membership status for rule ${path}`}
          value={leaf.status ?? ""}
          onChange={(e) =>
            onChange({ ...leaf, status: e.target.value === "" ? undefined : e.target.value })
          }
        >
          <option value="">any status</option>
          {STATUSES.map((s) => (
            <option key={s} value={s}>
              {s}
            </option>
          ))}
        </Select>
      ) : null}

      <Button
        variant="danger-ghost"
        className="ms-auto"
        aria-label={`Remove rule ${path}`}
        onClick={onRemove}
      >
        Remove
      </Button>
    </div>
  );
}

/** The value control is chosen by the field's kind, never by what the value looks like. */
function ValueControl({
  leaf,
  field,
  path,
  catalog,
  onChange,
}: {
  leaf: Leaf;
  field: FieldOption;
  path: string;
  catalog: Catalog;
  onChange: (value: unknown) => void;
}) {
  const label = `Value for rule ${path}`;
  if (!needsValue(leaf.op)) return null;

  if (leaf.op === "in_last_days") {
    return (
      <span className="flex items-center gap-2">
        <Input
          type="number"
          min={1}
          max={36500}
          className="w-24"
          aria-label={label}
          value={typeof leaf.value === "number" ? leaf.value : ""}
          onChange={(e) => onChange(e.target.value === "" ? "" : Number(e.target.value))}
        />
        <span className="text-fg-muted">days</span>
      </span>
    );
  }

  if (field.kind === "list" || field.kind === "tag") {
    const options = field.kind === "list" ? catalog.lists : catalog.tags;
    return (
      <Select
        aria-label={label}
        value={typeof leaf.value === "string" ? leaf.value : ""}
        onChange={(e) => onChange(e.target.value)}
      >
        <option value="">Choose one</option>
        {options.map((o) => (
          <option key={o.id} value={o.id}>
            {o.name}
          </option>
        ))}
      </Select>
    );
  }

  if (field.kind === "select") {
    return (
      <Select
        aria-label={label}
        value={typeof leaf.value === "string" ? leaf.value : ""}
        onChange={(e) => onChange(e.target.value)}
      >
        <option value="">Choose one</option>
        {(field.options ?? []).map((o) => (
          <option key={o} value={o}>
            {o}
          </option>
        ))}
      </Select>
    );
  }

  if (field.kind === "multi_select") {
    const chosen = Array.isArray(leaf.value) ? (leaf.value as string[]) : [];
    return (
      <div role="group" aria-label={label} className="flex flex-wrap items-center gap-3">
        {(field.options ?? []).map((o) => (
          <label key={o} className="flex items-center gap-1 wrap-anywhere">
            <Checkbox
              checked={chosen.includes(o)}
              onChange={(e) =>
                onChange(e.target.checked ? [...chosen, o] : chosen.filter((c) => c !== o))
              }
            />
            {o}
          </label>
        ))}
      </div>
    );
  }

  if (field.kind === "number") {
    return (
      <Input
        type="number"
        className="w-32"
        aria-label={label}
        value={typeof leaf.value === "number" ? leaf.value : ""}
        onChange={(e) => onChange(e.target.value === "" ? "" : Number(e.target.value))}
      />
    );
  }

  if (field.kind === "date" || field.kind === "datetime") {
    // shortcut: a date even for a `datetime` field, which the server accepts as `YYYY-MM-DD`.
    // Offer a time the day someone needs to segment on one.
    return (
      <Input
        type="date"
        className="w-44"
        aria-label={label}
        value={typeof leaf.value === "string" ? leaf.value : ""}
        onChange={(e) => onChange(e.target.value)}
      />
    );
  }

  return (
    <Input
      className="w-56"
      aria-label={label}
      value={typeof leaf.value === "string" ? leaf.value : ""}
      onChange={(e) => onChange(e.target.value)}
    />
  );
}

/**
 * How long the count waits after the last keystroke. Preview is 30 per minute per workspace
 * (`PREVIEW_PER_MINUTE`) and each call may hold a pooled connection for 1.5 seconds, so a count
 * per keystroke would spend a workspace's whole window on one person typing one value.
 */
const DEBOUNCE_MS = 500;

const PREVIEW_PROBLEMS = {
  "rate-limited": "Counting is busy — slow down for a moment and the count will catch up.",
  "insufficient-role": "Your role cannot count a segment.",
};

/**
 * What a count area has to show. One type for the builder and for a row of the list, so the
 * wording and the three shapes of section 5 are written once.
 */
export type CountState =
  | { kind: "unfinished" }
  | { kind: "counting" }
  | { kind: "failed"; message: string; retry: () => void }
  | { kind: "counted"; count: number | null | undefined; exact: boolean };

/**
 * The count for `doc`, refreshed on a debounce and **only when the document really changed**:
 * the query is keyed by the document itself, so a character typed and taken back is answered
 * from the cache and costs no request.
 *
 * **A number is only ever shown for the document that produced it.** While the debounce or the
 * request is outstanding the answer is `counting`, never the previous document's figure: a stale
 * number presented as this document's answer is a lie of exactly the kind section 5 is about.
 *
 * Nothing here is on the typing path. The controls are driven by `doc`, which this only reads.
 */
export function usePreview(ws: string, doc: Doc, fields: FieldOption[]): CountState {
  const json = JSON.stringify(doc);
  const [settled, setSettled] = useState(json);
  useEffect(() => {
    const timer = window.setTimeout(() => setSettled(json), DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [json]);

  const finished = complete(doc, fields);
  const query = useQuery({
    queryKey: ["segment-preview", ws, settled],
    queryFn: async () => {
      const { data, error } = await api.POST(
        "/api/v1/workspaces/{workspace_id}/segments/preview",
        {
          params: { path: { workspace_id: ws } },
          body: { rules: JSON.parse(settled) as Record<string, never> },
        },
      );
      if (!data) throw new Error(problemText(error, PREVIEW_PROBLEMS, "The count could not be loaded."));
      return data;
    },
    enabled: finished && json === settled,
    retry: false,
    refetchOnWindowFocus: false,
    // A count is only ever a number about this moment, and the window it is drawn from is a
    // minute wide: re-asking the same question inside that is the request the limit is for.
    staleTime: 60_000,
  });

  if (!finished) return { kind: "unfinished" };
  if (json !== settled) return { kind: "counting" };
  if (query.isError) {
    return { kind: "failed", message: query.error.message, retry: () => void query.refetch() };
  }
  return query.data
    ? { kind: "counted", count: query.data.count, exact: query.data.exact }
    : { kind: "counting" };
}

/** A count area: the pending, failed and unfinished states around [`CountText`]. */
export function Count({ state, testId }: { state: CountState; testId: string }) {
  if (state.kind === "unfinished") {
    return <span>no count yet — every rule needs a value.</span>;
  }
  if (state.kind === "failed") {
    return (
      <span role="alert">
        {state.message}{" "}
        <Button variant="ghost" onClick={state.retry}>
          Retry
        </Button>
      </span>
    );
  }
  if (state.kind === "counting") {
    return (
      <div className="w-24">
        <Skeleton label="Counting contacts" />
      </div>
    );
  }
  return <CountText count={state.count} exact={state.exact} testId={testId} />;
}

/**
 * A count, the three ways section 5 allows it to be said: an exact number plainly, an estimate
 * as `~12,400` with the word beside it, and "too many to count exactly" where the server refuses
 * to guess — a rule set filtering on a custom attribute, where its estimate measured 66x low.
 *
 * "estimate" is text, never colour: a count whose honesty is carried by a shade is a count a
 * colour-blind reader acts on as exact (WCAG 1.4.1).
 */
export function CountText({
  count,
  exact,
  testId,
}: {
  count: number | null | undefined;
  exact: boolean;
  testId: string;
}) {
  if (count === null || count === undefined) {
    return <span data-testid={testId}>too many to count exactly</span>;
  }
  const number = count.toLocaleString();
  return (
    <>
      <span data-testid={testId} className="tabular-nums">
        {exact ? number : `~${number}`}
      </span>
      {exact ? null : <span className="ms-2 text-fg-muted">estimate</span>}
    </>
  );
}
