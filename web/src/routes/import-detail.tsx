// SPDX-License-Identifier: AGPL-3.0-only

import { useQuery } from "@tanstack/react-query";
import { Link, useParams } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";

import { api, problemMessage } from "../api/client";
import type { components } from "../api/schema";
import { Button, buttonClass } from "../components/button";
import { EmptyState } from "../components/empty-state";
import { Select } from "../components/select";
import { TableScroller } from "../components/table-scroller";
import { CARD } from "../lib/ui";
import { type Uploaded, uploadKey } from "./imports";
import { useWorkspace } from "./workspace-layout";

type ColumnTarget = components["schemas"]["ColumnTarget"];
type ContactField = components["schemas"]["ContactField"];
type Definition = components["schemas"]["Definition"];

const th = "py-2 pe-3 text-start font-medium";
const link = "text-accent-text underline";

/** In the order the picker offers them. */
const FIELDS: [ContactField, string][] = [
  ["email", "Email"],
  ["first_name", "First name"],
  ["last_name", "Last name"],
  ["timezone", "Time zone"],
  ["tags", "Tags"],
  ["lists", "Lists"],
];

const DELIMITERS: Record<string, string> = { ",": "comma", ";": "semicolon", "\t": "tab", "|": "pipe" };

/** Decision 4, in the words the operator reads. */
const BLANK_CELLS = "A blank cell leaves the existing value alone.";

const encode = (t: ColumnTarget) => (t.kind === "ignore" ? "ignore" : `${t.kind}:${t.value}`);

function decode(value: string): ColumnTarget {
  const at = value.indexOf(":");
  const [kind, rest] = [value.slice(0, at), value.slice(at + 1)];
  if (kind === "custom") return { kind, value: rest };
  const field = FIELDS.find(([f]) => f === rest)?.[0];
  return kind === "field" && field ? { kind, value: field } : { kind: "ignore" };
}

/** Tags and lists gather from several columns; every other target is written by one. */
const single = (t: ColumnTarget) =>
  t.kind === "custom" || (t.kind === "field" && t.value !== "tags" && t.value !== "lists");

export default function ImportDetail() {
  const { workspace } = useWorkspace();
  const { importId } = useParams({ from: "/w/$slug/imports/$importId" });
  // Never fetches: the upload form put it in the cache. Observed here so it cannot be
  // collected while the screen is open.
  const held = useQuery<Uploaded | null>({
    queryKey: uploadKey(workspace?.id ?? "", importId),
    queryFn: () => null,
    enabled: false,
  });
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;
  return (
    <section className="mt-8">
      <Link to="/w/$slug/imports" params={{ slug: workspace.slug }} className={link}>
        Upload another file
      </Link>
      {held.data ? (
        <Mapping ws={workspace.id} uploaded={held.data} />
      ) : (
        <EmptyState
          title="This upload is not open here"
          actions={
            <Link to="/w/$slug/imports" params={{ slug: workspace.slug }} className={buttonClass("outline")}>
              Upload a file
            </Link>
          }
        >
          The mapping is kept in this tab only, and a reload closes it. Upload the file again to
          start over.
        </EmptyState>
      )}
    </section>
  );
}

function Mapping({ ws, uploaded: { upload, filename } }: { ws: string; uploaded: Uploaded }) {
  const [mapping, setMapping] = useState<ColumnTarget[]>(() => upload.columns.map((c) => c.proposed));
  const [reviewing, setReviewing] = useState(false);
  // The button that changes the step unmounts with it, so focus would fall to <body> and nothing
  // would say the heading changed. Focusing the heading announces it and keeps the place.
  const heading = useRef<HTMLHeadingElement>(null);
  const first = useRef(true);
  useEffect(() => {
    if (first.current) first.current = false;
    else heading.current?.focus();
  }, [reviewing]);

  const custom = useQuery({
    // Under the custom-fields screen's key as a prefix, never equal to it, as the lists do.
    queryKey: ["custom-fields", ws, "import"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/custom-fields", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      if (!data) throw new Error(problemMessage(error, "The custom fields could not be loaded."));
      // shortcut: the first 200 fields only, and an archived one is not offered. Raise with a "Load more".
      return data.data.filter((d) => !d.archived_at);
    },
    retry: false,
  });
  const defs: Definition[] = custom.data ?? [];

  const label = (t: ColumnTarget) =>
    t.kind === "ignore"
      ? "Not imported"
      : t.kind === "field"
        ? (FIELDS.find(([f]) => f === t.value)?.[1] ?? t.value)
        : (defs.find((d) => d.key === t.value)?.label ?? t.value);

  // What the server would refuse or silently resolve: no email column fails every row, and two
  // columns writing one field leave the last one's value.
  const problems = [
    ...(mapping.some((t) => t.kind === "field" && t.value === "email") ? [] : ["Map one column to Email."]),
    ...[...new Set(mapping.filter(single).map(encode))].flatMap((key) => {
      const n = mapping.filter((t) => encode(t) === key).length;
      return n > 1 ? [`${label(decode(key))} is chosen for ${n} columns.`] : [];
    }),
  ];

  return (
    <>
      <h2 ref={heading} tabIndex={-1} className="mt-4 text-xl font-semibold">{reviewing ? "Review the mapping" : "Map the columns"}</h2>
      <p className="mt-2 text-fg-muted">
        <span className="wrap-anywhere">{filename}</span>: {upload.columns.length.toLocaleString()} columns,
        separated by a {DELIMITERS[upload.delimiter] ?? "custom separator"};{" "}
        {upload.has_header ? "the first row is a header." : "there is no header row, so the columns are numbered."}
      </p>
      <p className="mt-2">{BLANK_CELLS} An empty cell in your file never clears what a contact already has.</p>

      {reviewing ? (
        <div className={CARD}>
          <TableScroller label="Mapping to review">
            <table className="w-full">
              <caption className="sr-only">Mapping to review</caption>
              <thead>
                <tr className="border-b border-border">
                  <th scope="col" className={th}>Column</th>
                  <th scope="col" className={th}>First sample</th>
                  <th scope="col" className={th}>Imported as</th>
                </tr>
              </thead>
              <tbody>
                {upload.columns.map((c, i) => {
                  const t = mapping[i];
                  return t && t.kind !== "ignore" ? (
                    <tr key={i} className="border-b border-border align-top">
                      <th scope="row" className="py-2 pe-3 text-start font-medium wrap-anywhere">{c.name}</th>
                      <td className="py-2 pe-3 wrap-anywhere">{c.samples[0] ?? ""}</td>
                      <td className="py-2 wrap-anywhere">
                        {label(t)}
                        {t.kind === "custom" ? " (custom field)" : ""}
                      </td>
                    </tr>
                  ) : null;
                })}
              </tbody>
            </table>
          </TableScroller>
          <p className="mt-4 text-fg-muted">
            {mapping.filter((t) => t.kind === "ignore").length.toLocaleString()} columns are not imported.
          </p>
          <div className="mt-6 flex justify-end">
            <Button onClick={() => setReviewing(false)}>Back to mapping</Button>
          </div>
        </div>
      ) : (
        <div className={CARD}>
          <TableScroller label="Column mapping">
            <table className="w-full">
              <caption className="sr-only">Each column of the file, a sample of its values, and what it is imported as</caption>
              <thead>
                <tr className="border-b border-border">
                  <th scope="col" className={th}>Column</th>
                  <th scope="col" className={th}>Sample from your file</th>
                  <th scope="col" className={th}>Import as</th>
                </tr>
              </thead>
              <tbody>
                {upload.columns.map((c, i) => {
                  const t = mapping[i] ?? { kind: "ignore" as const };
                  return (
                    <tr key={i} className="border-b border-border align-top">
                      <th scope="row" className="py-2 pe-3 text-start font-medium wrap-anywhere">{c.name}</th>
                      <td className="py-2 pe-3 wrap-anywhere">
                        {c.samples.length === 0 ? (
                          <span className="text-fg-muted">Blank in the first 100 rows</span>
                        ) : (
                          <ul>
                            {c.samples.map((s, j) => (
                              <li key={j}>{s}</li>
                            ))}
                          </ul>
                        )}
                      </td>
                      <td className="py-2">
                        <Select
                          aria-label={`Import column ${c.name} as`}
                          value={encode(t)}
                          onChange={(e) => setMapping((m) => m.map((old, k) => (k === i ? decode(e.target.value) : old)))}
                        >
                          <option value="ignore">Do not import</option>
                          <optgroup label="Contact field">
                            {FIELDS.map(([f, name]) => (
                              <option key={f} value={`field:${f}`}>
                                {name}
                              </option>
                            ))}
                          </optgroup>
                          <optgroup label="Custom field">
                            {defs.map((d) => (
                              <option key={d.key} value={`custom:${d.key}`}>
                                {d.label}
                              </option>
                            ))}
                            {/* A key the list did not return stays selectable: the value shown is the value held. */}
                            {t.kind === "custom" && !defs.some((d) => d.key === t.value) ? (
                              <option value={encode(t)}>{t.value}</option>
                            ) : null}
                          </optgroup>
                        </Select>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </TableScroller>
          {custom.isError ? (
            <p role="alert" className="mt-3">
              {custom.error.message} Custom fields are not offered until they load.{" "}
              <Button variant="ghost" onClick={() => void custom.refetch()}>
                Retry
              </Button>
            </p>
          ) : null}
          {problems.length > 0 ? (
            <ul id="mapping-problems" className="mt-4 list-disc ps-6 text-danger-text">
              {problems.map((p) => (
                <li key={p}>{p}</li>
              ))}
            </ul>
          ) : null}
          <div className="mt-6 flex justify-end">
            <Button
              variant="solid"
              disabled={problems.length > 0}
              aria-describedby={problems.length > 0 ? "mapping-problems" : undefined}
              onClick={() => setReviewing(true)}
            >
              Review mapping
            </Button>
          </div>
        </div>
      )}
    </>
  );
}
