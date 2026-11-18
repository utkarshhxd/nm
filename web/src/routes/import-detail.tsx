// SPDX-License-Identifier: AGPL-3.0-only

import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "@tanstack/react-router";
import { type RefObject, useEffect, useRef, useState } from "react";

import { api, problemMessage, problemText } from "../api/client";
import type { components } from "../api/schema";
import { Button, buttonClass } from "../components/button";
import { Checkbox } from "../components/checkbox";
import { EmptyState } from "../components/empty-state";
import { ErrorState } from "../components/error-state";
import { Select } from "../components/select";
import { Skeleton } from "../components/skeleton";
import { TableScroller } from "../components/table-scroller";
import { download } from "../lib/download";
import { type Uploaded, uploadKey } from "../lib/import-upload";
import { CARD } from "../lib/ui";
import { useWorkspace } from "./workspace-layout";

type ColumnTarget = components["schemas"]["ColumnTarget"];
type ContactField = components["schemas"]["ContactField"];
type Definition = components["schemas"]["Definition"];
type Dedup = components["schemas"]["Dedup"];
type Report = components["schemas"]["ImportReport"];

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

/** The three modes of spec 7.3, in the order they are offered, the default first. */
const DEDUP: { mode: Dedup; label: string; says: string }[] = [
  {
    mode: "update_existing",
    label: "Update it.",
    says: "A cell in your file replaces what the contact has; a blank cell changes nothing.",
  },
  { mode: "skip_existing", label: "Skip it.", says: "The existing contact is left exactly as it is." },
  {
    mode: "fail_on_duplicate",
    label: "Stop.",
    says: "If any address in the file is already a contact, nothing is imported and the import fails.",
  },
];

/** What each mode becomes in the sentence that says what will happen. */
const DEDUP_WILL: Record<Dedup, string> = {
  update_existing: "An address that is already a contact will be updated: a blank cell in your file leaves the stored value as it is.",
  skip_existing: "An address that is already a contact will be skipped: the contact is left exactly as it is.",
  fail_on_duplicate: "If any address in the file is already a contact, nothing is imported and the import fails.",
};

/** How many rejected rows one request fetches for the table. */
const ERRORS_PAGE = 50;
/** The server's page-size ceiling, for the CSV: fewest requests for up to 10,000 rows. */
const CSV_PAGE = 200;
/** How often a screen opened on a running import asks again. The run is one request, so there is no percentage to poll for. */
const RUNNING_POLL_MS = 3000;

const CSV_HEADER = ["row_number", "column_name", "reason", "value"];

const encode = (t: ColumnTarget) => (t.kind === "ignore" ? "ignore" : `${t.kind}:${t.value}`);

function decode(value: string): ColumnTarget {
  if (value === "ignore") return { kind: "ignore" };
  const at = value.indexOf(":");
  const [kind, rest] = [value.slice(0, at), value.slice(at + 1)];
  if (kind === "custom") return { kind, value: rest };
  const field = FIELDS.find(([f]) => f === rest)?.[0];
  return kind === "field" && field ? { kind, value: field } : { kind: "ignore" };
}

/** Tags and lists gather from several columns; every other target is written by one. */
const single = (t: ColumnTarget) =>
  t.kind === "custom" || (t.kind === "field" && t.value !== "tags" && t.value !== "lists");

const plural = (n: number, one: string, many: string) => `${n.toLocaleString()} ${n === 1 ? one : many}`;

const reportKey = (ws: string, id: string) => ["import", ws, id] as const;

/** The import is not in this workspace, or not anywhere: the server answers both with 404. */
class Missing extends Error {}

const KNOWN = {
  "insufficient-role": "Your role cannot run an import. It takes editor or above.",
  "import-in-progress":
    "Another import is already running in this workspace, perhaps in another tab or started by someone else. Imports run one at a time. Nothing is wrong with this file: wait for that one to finish, then run this import again.",
};

/**
 * Focuses `ref` when `value` changes, and never on mount. It compares against the last value seen
 * rather than skipping a first run, so `StrictMode`'s second dev-only run of an effect cannot
 * focus the heading on the way in.
 */
function useFocusOnChange(ref: RefObject<HTMLElement | null>, value: unknown) {
  const seen = useRef(value);
  useEffect(() => {
    if (seen.current === value) return;
    seen.current = value;
    ref.current?.focus();
  }, [ref, value]);
}

export default function ImportDetail() {
  const { workspace } = useWorkspace();
  const { importId } = useParams({ from: "/w/$slug/imports/$importId" });
  const ws = workspace?.id ?? "";
  // Never fetches: the upload form put it in the cache. Observed here so it cannot be
  // collected while the screen is open.
  const held = useQuery<Uploaded | null>({
    queryKey: uploadKey(ws, importId),
    queryFn: () => null,
    enabled: false,
  });
  const report = useQuery({
    queryKey: reportKey(ws, importId),
    enabled: ws !== "",
    queryFn: async () => {
      const { data, error, response } = await api.GET("/api/v1/workspaces/{workspace_id}/imports/{id}", {
        params: { path: { workspace_id: ws, id: importId } },
      });
      if (data) return data;
      if (response.status === 404) throw new Missing();
      throw new Error(problemMessage(error, "The import could not be loaded."));
    },
    retry: false,
    // A run that was started elsewhere, or before a reload, is still the server's to finish.
    refetchInterval: (q) => (q.state.data?.status === "running" ? RUNNING_POLL_MS : false),
  });
  // The import whose run the mapping step sent, so the report that replaces it takes focus: the
  // button that was pressed is gone, and focus would otherwise fall to <body>. Compared to the
  // current id, not a flag, so a report for another import never takes focus it did not earn.
  const startedFor = useRef<string | null>(null);
  // The layout renders no `Outlet` until the workspace resolves, so this is only a type guard.
  if (!workspace) return null;

  const status = report.data?.status;
  let body;
  if (held.data && (status === undefined || status === "pending")) {
    body = <Mapping ws={workspace.id} id={importId} uploaded={held.data} onStart={() => (startedFor.current = importId)} />;
  } else if (report.isPending) {
    body = (
      <div className="mt-8">
        <Skeleton label="Loading the import" rows={3} />
      </div>
    );
  } else if (report.error instanceof Missing) {
    body = (
      <EmptyState title="Import not found">
        There is no import with this address in this workspace.
      </EmptyState>
    );
  } else if (!report.data) {
    body = (
      <div className="mt-8">
        <ErrorState title="The import could not be loaded" onRetry={() => void report.refetch()}>
          {report.error?.message}
        </ErrorState>
      </div>
    );
  } else if (status === "pending") {
    body = (
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
    );
  } else if (status === "running") {
    body = (
      <div role="status" className={CARD}>
        <h2 className="text-xl font-semibold">This import is running</h2>
        <p className="mt-2 text-fg-muted wrap-anywhere">
          {report.data.filename} has not finished. This page asks again every few seconds and shows
          the report when it is done.
        </p>
      </div>
    );
  } else {
    body = <ReportView ws={workspace.id} report={report.data} focus={startedFor.current === importId} />;
  }

  return (
    <section className="mt-8">
      <Link to="/w/$slug/imports" params={{ slug: workspace.slug }} className={link}>
        Upload another file
      </Link>
      {body}
    </section>
  );
}

function Mapping({
  ws,
  id,
  uploaded: { upload, filename },
  onStart,
}: {
  ws: string;
  id: string;
  uploaded: Uploaded;
  onStart: () => void;
}) {
  const qc = useQueryClient();
  const [mapping, setMapping] = useState<ColumnTarget[]>(() => upload.columns.map((c) => c.proposed));
  const [reviewing, setReviewing] = useState(false);
  const [mode, setMode] = useState<Dedup>("update_existing");
  const [listId, setListId] = useState("");
  // What the file will be read with. They start as the sniff's guess and are sent only when they
  // differ from it: the server keeps what it stored for a request that carries neither.
  const [delimiter, setDelimiter] = useState(upload.delimiter);
  const [hasHeader, setHasHeader] = useState(upload.has_header);
  // The button that changes the step unmounts with it, so focus would fall to <body> and nothing
  // would say the heading changed. Focusing the heading announces it and keeps the place.
  const heading = useRef<HTMLHeadingElement>(null);
  useFocusOnChange(heading, reviewing);

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

  const lists = useQuery({
    queryKey: ["lists", ws, "import"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/lists", {
        params: { path: { workspace_id: ws }, query: { limit: 200 } },
      });
      if (!data) throw new Error(problemMessage(error, "The lists could not be loaded."));
      // shortcut: the first 200 lists only, as the contacts screen's pickers. Raise with a "Load more".
      return data.data;
    },
    retry: false,
  });
  const target = lists.data?.find((l) => l.id === listId);

  const start = useMutation({
    mutationFn: async () => {
      let result;
      try {
        result = await api.POST("/api/v1/workspaces/{workspace_id}/imports/{id}/start", {
          params: { path: { workspace_id: ws, id } },
          body: {
            mapping,
            dedup: mode,
            list_id: listId || null,
            ...(delimiter === upload.delimiter ? {} : { delimiter }),
            ...(hasHeader === upload.has_header ? {} : { has_header: hasHeader }),
          },
        });
      } catch {
        // `fetch` REJECTS when the connection goes, and the browser says "Failed to fetch". This
        // request is the whole run, so losing it says nothing about whether the run finished —
        // the server carries on with nobody attached, and pressing Run again while it does
        // answers a 409 that cannot name the import, because an uncommitted run is in no list.
        // So the only safe instruction is to go and look.
        throw new Error(
          "The connection to the server was lost while the import was running. The import may still be finishing on the server: reload this page in a minute to see its report before running anything again.",
        );
      }
      const { data, error } = result;
      if (!data) throw new Error(problemText(error, KNOWN, "The import could not be run."));
      return data;
    },
    onSuccess: (done) => qc.setQueryData(reportKey(ws, id), done),
    // A refusal can still have changed the import: `import-duplicate-address` leaves it `failed`,
    // with the rows it rejected kept. Ask the server what it is now rather than guessing.
    onError: () => qc.invalidateQueries({ queryKey: reportKey(ws, id) }),
  });

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
  const ignored = mapping.filter((t) => t.kind === "ignore").length;

  return (
    <>
      <h2 ref={heading} tabIndex={-1} className="mt-4 text-xl font-semibold">{reviewing ? "Review the mapping" : "Map the columns"}</h2>
      <p className="mt-2 text-fg-muted">
        <span className="wrap-anywhere">{filename}</span>: {plural(upload.columns.length, "column", "columns")},
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
            {ignored === 0
              ? "Every column is imported."
              : `${plural(ignored, "column is", "columns are")} not imported.`}
          </p>

          <fieldset disabled={start.isPending} className="mt-6 min-w-0">
            <legend className="font-medium">How the file is read</legend>
            {/* The old copy said to look at the samples again after a change. The samples and the
                column list come from the upload, and nothing re-reads the file here, so there is
                nothing new to look at — and if the correction changes how many columns the file
                has, the mapping above is positional against the wrong parse. Re-reading the file
                on a correction is server-side work and is ticketed; until then the copy says what
                an operator can actually do. */}
            <p className="mt-1 text-sm text-fg-muted">
              The columns above came from the guess shown at the top. If it was wrong, go back and
              upload the file again — the table above was read with the old guess and is not
              re-read here, and the mapping applies by column position. Correct it here only if
              you are sure the columns stay the same.
            </p>
            <label className="mt-3 block font-medium" htmlFor="import-delimiter">
              Separator
            </label>
            <Select
              id="import-delimiter"
              className="mt-1"
              value={delimiter}
              onChange={(e) => setDelimiter(e.target.value)}
            >
              {Object.entries(DELIMITERS).map(([char, name]) => (
                <option key={char} value={char}>
                  {name.charAt(0).toUpperCase() + name.slice(1)}
                </option>
              ))}
            </Select>
            <label className="mt-3 flex items-start gap-2">
              <Checkbox checked={hasHeader} onChange={(e) => setHasHeader(e.target.checked)} />
              <span>The first row is a header</span>
            </label>
          </fieldset>

          <fieldset disabled={start.isPending} className="mt-6 min-w-0">
            <legend className="font-medium">If an address is already a contact</legend>
            {DEDUP.map((d) => (
              <label key={d.mode} className="mt-2 flex items-start gap-2">
                <input
                  type="radio"
                  name="dedup"
                  value={d.mode}
                  checked={mode === d.mode}
                  onChange={() => setMode(d.mode)}
                  className="m-0.5 size-5 shrink-0 accent-accent"
                />
                <span>
                  <span className="font-medium">{d.label}</span> {d.says}
                </span>
              </label>
            ))}
          </fieldset>

          <div className="mt-6">
            <label className="block font-medium" htmlFor="import-list">
              Add every contact to a list
            </label>
            <Select
              id="import-list"
              className="mt-1"
              value={listId}
              disabled={start.isPending}
              onChange={(e) => setListId(e.target.value)}
            >
              <option value="">No list</option>
              {lists.data?.map((l) => (
                <option key={l.id} value={l.id}>
                  {l.name}
                </option>
              ))}
            </Select>
            {lists.isError ? (
              <p role="alert" className="mt-2">
                {lists.error.message} Lists are not offered until they load.{" "}
                <Button variant="ghost" onClick={() => void lists.refetch()}>
                  Retry
                </Button>
              </p>
            ) : null}
          </div>

          <section aria-labelledby="import-will" className="mt-6 rounded-md border border-border-strong p-4">
            <h3 id="import-will" className="font-medium">What will happen</h3>
            <ul className="mt-2 list-disc ps-6 wrap-anywhere">
              <li>
                Columns are separated by a {DELIMITERS[delimiter] ?? "custom separator"}, and the first
                row is {hasHeader ? "skipped as a header" : "read as data"}.
              </li>
              <li>
                {(mapping.length - ignored).toLocaleString()} of {plural(mapping.length, "column is", "columns are")}{" "}
                imported, as listed above.
              </li>
              <li>{DEDUP_WILL[mode]}</li>
              <li>If an address appears more than once in the file, it is imported once, from its last row.</li>
              <li>
                {target ? (
                  <>
                    {target.double_optin
                      ? `Every contact is added to ${target.name} as pending until they confirm: it uses double opt-in.`
                      : `Every contact is subscribed to ${target.name}.`}{" "}
                    An address that has already unsubscribed from it stays unsubscribed.
                  </>
                ) : (
                  "Contacts are not added to a list, apart from any list your file's own lists column names."
                )}
              </li>
              <li>An address on a suppression list is still imported, and is not mailable.</li>
            </ul>
          </section>

          {start.error ? <p role="alert" className="mt-4 text-danger-text wrap-anywhere">{start.error.message}</p> : null}
          <p role="status" className="mt-4 text-fg-muted">
            {start.isPending ? "Running the import. Keep this tab open until it finishes." : ""}
          </p>
          <div className="mt-6 flex flex-wrap justify-end gap-3">
            <Button disabled={start.isPending} onClick={() => setReviewing(false)}>
              Back to mapping
            </Button>
            <Button
              variant="solid"
              busy={start.isPending}
              onClick={() => {
                if (start.isPending) return;
                onStart();
                start.mutate();
              }}
            >
              Run the import
            </Button>
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
                          <span className="text-fg-muted">Blank in the rows sampled</span>
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
          {/* A live region that is always there, so a change to the list is announced and not just drawn. */}
          <div id="mapping-problems" role="status" className="mt-4 text-danger-text">
            {problems.length > 0 ? (
              <ul className="list-disc ps-6">
                {problems.map((p) => (
                  <li key={p}>{p}</li>
                ))}
              </ul>
            ) : null}
          </div>
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

/**
 * One cell of the error CSV, as `contacts/export.rs` writes one: neutralised against formulas, then quoted if it must be.
 *
 * Deliberate, ruled in review: a rejected `=1+1` is `=1+1` in the table and `'=1+1` here. The CSV is
 * opened in a spreadsheet, and the table's first row matching the CSV's means the same row, not
 * the same bytes. Do not "fix" the prefix away.
 */
function csvCell(cell: string): string {
  // shortcut: a leading tab or CR is always prefixed and every other lead is exempt only when the
  // whole cell is a finite number. JS and Rust agree on that for the cells an import can reject,
  // and where they disagree (`Number` trims, `f64::from_str` does not) this errs towards the prefix.
  const lead = /^[=+\-@\t\r']/.test(cell);
  const number = !/^[\t\r]/.test(cell) && cell === cell.trim() && Number.isFinite(Number(cell));
  const safe = lead && !number ? `'${cell}` : cell;
  return /[,"\n\r]/.test(safe) ? `"${safe.replaceAll('"', '""')}"` : safe;
}

function ReportView({ ws, report, focus }: { ws: string; report: Report; focus: boolean }) {
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => {
    if (focus) heading.current?.focus();
  }, [focus]);
  const failedRun = report.status === "failed";
  // Only the first `errors_recorded` rejected rows were kept, in file order. When that is fewer
  // than `failed`, the table and the CSV are the start of the damage and not all of it.
  const unlisted = Math.max(0, report.failed - report.errors_recorded);

  return (
    <>
      <h2 ref={heading} tabIndex={-1} className="mt-4 text-xl font-semibold">Import report</h2>
      <p className="mt-2 text-fg-muted wrap-anywhere">{report.filename}</p>

      {failedRun ? (
        <section aria-labelledby="import-failed" className="mt-6 rounded-md border border-danger bg-surface p-4">
          <h3 id="import-failed" className="font-semibold">This import failed</h3>
          <p className="mt-2 wrap-anywhere">{report.error_reason ?? "It did not finish, and the server gave no reason."}</p>
          <p className="mt-2 text-fg-muted">The counts below are what it had reached when it stopped.</p>
        </section>
      ) : null}

      <div className={CARD}>
        <p>
          {report.total_rows == null
            ? "The rows in the file were not counted."
            : `${plural(report.total_rows, "row", "rows")} in the file.`}{" "}
          {failedRun ? "" : "Each row is in exactly one of the five outcomes below, so they add up to that number."}
        </p>
        <TableScroller label="Rows in the file, by outcome">
          <table className="mt-4 w-full">
            <caption className="sr-only">Rows in the file, by outcome</caption>
            <tbody>
              <CountRow name="Imported" says="Rows that became a new contact." n={report.imported} />
              <CountRow name="Updated" says="Rows whose address was already a contact, and changed it." n={report.updated} />
              <CountRow name="Skipped" says="Rows left alone because the contact already existed." n={report.skipped} />
              <CountRow
                name="Collapsed"
                says="Rows replaced by a later row of the same file with the same address."
                n={report.collapsed}
              />
              <CountRow name="Failed" says="Rows rejected. Each one is listed below with its reason." n={report.failed} />
            </tbody>
          </table>
        </TableScroller>

        <p className="mt-6 font-medium">These two are not extra rows</p>
        <p className="mt-1 text-fg-muted">
          They describe rows already counted above, so adding them to the five counts those rows twice.
        </p>
        <TableScroller label="Counted inside those rows, not extra rows">
          <table className="mt-2 w-full">
            <caption className="sr-only">Counted inside those rows, not extra rows</caption>
            <tbody>
              <CountRow
                name="Suppressed"
                says="Imported or updated contacts whose address or domain is suppressed. They exist and are not mailable."
                n={report.suppressed}
              />
              <CountRow
                name="Memberships declined"
                says="Subscriptions the import asked for and did not get, because the address is already unsubscribed from that list."
                n={report.memberships_declined}
              />
            </tbody>
          </table>
        </TableScroller>
      </div>

      {report.failed > 0 || report.errors_recorded > 0 ? (
        <RejectedRows ws={ws} report={report} unlisted={unlisted} />
      ) : (
        <p className="mt-6 text-fg-muted">No row was rejected.</p>
      )}
    </>
  );
}

function CountRow({ name, says, n }: { name: string; says: string; n: number }) {
  return (
    <tr className="border-b border-border align-top">
      <th scope="row" className="py-2 pe-3 text-start font-medium">
        {name}
        <span className="block font-normal text-fg-muted wrap-anywhere">{says}</span>
      </th>
      <td className="py-2 text-end tabular-nums">{n.toLocaleString()}</td>
    </tr>
  );
}

function RejectedRows({ ws, report, unlisted }: { ws: string; report: Report; unlisted: number }) {
  const { id } = report;
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState("");
  const rows = useInfiniteQuery({
    queryKey: ["import-errors", ws, id],
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) => {
      const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/imports/{id}/errors", {
        params: { path: { workspace_id: ws, id }, query: { limit: ERRORS_PAGE, cursor: pageParam } },
      });
      if (!data) throw new Error(problemMessage(error, "The rejected rows could not be loaded."));
      return data;
    },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
    retry: false,
    // Nothing was kept (the run failed before it wrote any): there is no list to fetch.
    enabled: report.errors_recorded > 0,
  });
  const shown = rows.data?.pages.flatMap((p) => p.data) ?? [];

  /**
   * Every kept row, from the start, as a CSV. Not the rows on screen: the table pages, the file does not.
   *
   * shortcut: held in browser memory until the download starts. At most `MAX_IMPORT_ERRORS`
   * (10,000) rows of 200-character values, a few megabytes; upgrade if that cap is ever lifted.
   */
  async function downloadCsv() {
    if (busy) return;
    setBusy(true);
    setFailure("");
    try {
      const lines = [CSV_HEADER.join(",")];
      let cursor: string | undefined;
      do {
        const { data, error } = await api.GET("/api/v1/workspaces/{workspace_id}/imports/{id}/errors", {
          params: { path: { workspace_id: ws, id }, query: { limit: CSV_PAGE, cursor } },
        });
        if (!data) {
          setFailure(problemMessage(error, "The rejected rows could not be loaded, so no file was saved."));
          return;
        }
        for (const r of data.data) {
          lines.push([String(r.row_number), r.column_name ?? "", r.reason, r.value ?? ""].map(csvCell).join(","));
        }
        cursor = data.next_cursor ?? undefined;
      } while (cursor);
      download(new Blob([`${lines.join("\r\n")}\r\n`], { type: "text/csv" }), "import-errors.csv");
    } catch {
      setFailure("The download did not finish, so no file was saved. Try again.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <section aria-labelledby="import-rejected" className="mt-8">
      <h3 id="import-rejected" className="text-lg font-semibold">Rejected rows</h3>
      {report.errors_recorded === 0 ? (
        <div
          role="note"
          aria-label="No rejected rows were kept"
          className="mt-3 rounded-md border border-border-strong bg-surface p-4"
        >
          <p className="font-medium">
            {plural(report.failed, "row was", "rows were")} rejected. None {report.failed === 1 ? "was" : "were"} kept.
          </p>
          <p className="mt-1">There is no list of them here and no CSV to download.</p>
        </div>
      ) : null}
      {unlisted > 0 && report.errors_recorded > 0 ? (
        <div
          role="note"
          aria-label="The rejected rows are not all listed"
          className="mt-3 rounded-md border border-border-strong bg-surface p-4"
        >
          <p className="font-medium">
            Showing the first {report.errors_recorded.toLocaleString()} of {report.failed.toLocaleString()} rejected
            rows, in file order.
          </p>
          <p className="mt-1">
            The other {plural(unlisted, "rejected row was", "rejected rows were")} not kept: {unlisted === 1 ? "it is" : "they are"}{" "}
            in no list here and not in the CSV.
          </p>
        </div>
      ) : null}
      {report.errors_recorded > 0 ? (
      <div className="mt-3">
        <Button busy={busy} onClick={() => void downloadCsv()}>
          Download the {plural(report.errors_recorded, "listed row", "listed rows")} as CSV
        </Button>
      </div>
      ) : null}
      {failure ? <p role="alert" className="mt-2 text-danger-text">{failure}</p> : null}

      {report.errors_recorded === 0 ? null : rows.isPending ? (
        <div className="mt-4">
          <Skeleton label="Loading the rejected rows" rows={3} />
        </div>
      ) : rows.isError ? (
        <div className="mt-4">
          <ErrorState title="The rejected rows could not be loaded" onRetry={() => void rows.refetch()}>
            {rows.error.message}
          </ErrorState>
        </div>
      ) : (
        <>
          <TableScroller label="Rejected rows, scrollable">
            <table className="mt-4 w-full">
              <caption className="sr-only">Rejected rows</caption>
              <thead>
                <tr className="border-b border-border">
                  <th scope="col" className={th}>Row</th>
                  <th scope="col" className={th}>Column</th>
                  <th scope="col" className={th}>Reason</th>
                  <th scope="col" className={th}>Value</th>
                </tr>
              </thead>
              <tbody>
                {shown.map((r) => (
                  <tr key={r.row_number} className="border-b border-border align-top">
                    {/* Plain digits, not `toLocaleString`: the CSV's first row must read the same. */}
                    <th scope="row" className="py-2 pe-3 text-start font-medium tabular-nums">{r.row_number}</th>
                    <td className="py-2 pe-3 wrap-anywhere">{r.column_name ?? ""}</td>
                    <td className="py-2 pe-3 wrap-anywhere">{r.reason}</td>
                    <td className="py-2 wrap-anywhere">{r.value ?? ""}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </TableScroller>
          {rows.hasNextPage ? (
            <div className="mt-4">
              <Button busy={rows.isFetchingNextPage} onClick={() => void rows.fetchNextPage()}>
                Load more rejected rows
              </Button>
            </div>
          ) : null}
        </>
      )}
    </section>
  );
}
