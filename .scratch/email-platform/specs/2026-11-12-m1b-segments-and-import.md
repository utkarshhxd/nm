# M1b — Segment engine, import and export

Milestone M1b of NM. Follows M1a (contact core and suppressions), which is merged.

This milestone answers the one question `spec.md` left explicitly open — *"How segment rules
compile into SQL"* (`spec.md:512`) — and delivers the three things M1 promised that M1a did not:
live segments with counts, CSV import with field mapping and de-duplication, and export.

It also closes the two issues M1a filed as blocking for this milestone, and it closes one of them
by making it obsolete rather than by mitigating it.

---

## 1. Scope

**In:**
- A segment rule AST, stored as JSON, compiled to parameterised SQL.
- A rule builder in the UI with counts.
- Count preview: exact when cheap, a labelled estimate when not.
- Segments as a first-class target for the existing bulk/selection engine.
- CSV import: upload, field mapping, validation, de-duplication, upsert, per-row error report.
- NDJSON and CSV streaming import over the API; `POST /contacts:batchUpsert` up to 10k.
- Export of a list, a segment, or the whole workspace, streamed.
- `list_memberships.updated_at` and the as-of fact bound (§10) that closes issue `04`.
- The `remove` → `unsubscribe` rename (issue `08`).

**Out, deferred with reasons:**
- Engagement rules (`opened`, `clicked`, `bounced`). There is no send path yet, so there are no
  events to match. The AST reserves the shape; §4.6 says how.
- The Interspire and listmonk importers. They are M6. This milestone builds the generic CSV and
  NDJSON path they will reuse.
- A background worker (`nm worker`). §7.4 explains why imports run in-process for now and what
  must change when the worker lands.
- Double opt-in forms, preference center, consent log. Those are M1c.
- AI segment-from-a-sentence (`spec.md:179`). Needs the rule AST to be stable first; this
  milestone makes it so.

---

## 2. Decisions carried in

From M0 and M1a, unchanged and load-bearing:

- `workspace_id` on every tenant table; composite foreign keys `(workspace_id, id)` so a
  cross-tenant row is unrepresentable. `authorize` returns 404 before it checks the role.
- `contacts` has **no status column**. A workspace-wide opt-out is a `suppressions` row with
  reason `unsubscribed_all`. ADR 0001.
- `mailable_list_members` is the **only** definition of eligibility. Four separate `NOT EXISTS`,
  never two combined with `OR`. A segment is a *selection* predicate, not an eligibility one —
  §4.7 says how the two relate and why they must not be merged.
- Every request body carries `#[serde(deny_unknown_fields)]`, including any signed payload. Three
  near-misses in M1a came from the absence of it.
- A bulk action is bounded by the count its token was issued with; delete and subscribe refuse a
  widened selection, the rest report it. The axis is **consent-creating**, not irreversible.
  ADR 0003.
- Emails are stored already normalised by `users::normalize_email`. No `citext`, no `lower()`
  index.
- IDs are UUIDv7 generated in Rust.

---

## 3. Domain model

A **segment** is a named, saved predicate over contacts in one workspace. It is **live**: it
stores rules, never a membership list, and is re-evaluated on every read. A contact is "in" a
segment exactly when the rules match it now.

A segment is not a list. A list is a thing a person joins and whose membership carries consent
and a status; a segment is a question asked of the data. Nothing is ever "added to" a segment,
there is no membership row, and removing a contact from a segment is not an operation — you
change the rules or you change the contact.

Segments have a **version**, an integer bumped on every rule change. §4.8 explains why a
selection token must carry it.

---

## 4. How segment rules compile into SQL

This is the open question from `spec.md:512`. The answer has five parts: the AST, the whitelist,
the parameterisation, the negation rule, and the index plan.

### 4.1 The AST

```jsonc
{
  "match": "all",            // "all" | "any"
  "rules": [
    { "field": "email",        "op": "contains",  "value": "@acme.com" },
    { "field": "attr:plan",    "op": "eq",        "value": "pro" },
    { "field": "list",         "op": "member_of", "value": "<uuid>", "status": "subscribed" },
    { "field": "tag",          "op": "not_has",   "value": "<uuid>" },
    { "group": { "match": "any", "rules": [ … ] } }
  ]
}
```

A node is **either** a rule **or** a group, never both. Both shapes carry
`#[serde(deny_unknown_fields)]`.

**Limits, enforced at parse:** depth ≤ 3, ≤ 50 rules in one segment, ≤ 20 rules in one group.
A segment that exceeds any of them is `422`, not truncated. The limits exist so the compiler's
output stays plannable and so a rule document cannot be used as a denial-of-service input; they
are generous for a rule builder a human drives.

### 4.2 Fields are a whitelist, never a passthrough

A `field` resolves through a closed set:

| Field | Source |
|---|---|
| `email`, `first_name`, `last_name`, `timezone` | a column on `contacts` |
| `email_domain` | the generated stored column |
| `created_at`, `updated_at` | a column on `contacts` |
| `attr:<key>` | `contacts.attributes ->> <key>`, where `<key>` **must** name a live custom field definition |
| `list` | `list_memberships` |
| `tag` | `contact_tags` |
| `suppressed` | the `blocking()` predicate |

Anything else is `422 unknown-field`. An `attr:` key that does not match a definition in this
workspace is `422 unknown-field` too — **not** an always-false rule. A segment that silently
matches nothing because of a typo is the same failure shape as M1a's all-`None` filter, and that
one nearly deleted a workspace.

Archived definitions resolve for reading, so an existing segment keeps working after a field is
archived, but a **new** rule cannot name one.

### 4.3 Operators are typed by the definition, not by the value

The permitted operators come from the field's declared kind, not from what the JSON value looks
like:

- **text:** `eq`, `neq`, `contains`, `not_contains`, `starts_with`, `ends_with`, `is_set`,
  `is_not_set`
- **number:** `eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `is_set`, `is_not_set`
- **date / datetime:** `before`, `after`, `on`, `in_last_days`, `is_set`, `is_not_set`
- **boolean:** `is_true`, `is_false`, `is_set`, `is_not_set`
- **list:** `member_of`, `not_member_of`, each with an optional `status`
- **tag:** `has`, `not_has`
- **suppressed:** `is_true`, `is_false`

An operator outside its kind's set is `422`. Casting follows the **definition's** kind —
`(attributes ->> $n)::numeric` for a number field — which is safe only because
`validate_attributes` enforces the type on write. That coupling is load-bearing: if a write path
ever stores an untyped attribute, this cast becomes a runtime error on a read path, and the
segment that triggers it will be someone else's.

### 4.4 Values are bound, never formatted

The compiler emits SQL through `sqlx::QueryBuilder` and every value, **including every attribute
key**, arrives via `push_bind`. Postgres accepts a bound key in `attributes ->> $n`, so there is
no path on which user text becomes SQL text.

There is no string concatenation of a field name either: a field resolves to a *fixed* SQL
fragment chosen by a match on the whitelist. The compiler must not have a branch that writes a
caller-supplied identifier.

**Test obligation:** a rule whose attribute key is `plan'); drop table contacts; --` compiles,
runs, matches nothing, and leaves `contacts` present. This test must exist and must be shown to
fail if any value is interpolated instead of bound.

### 4.5 Negation is `NOT EXISTS`, never `!=`

`not_has`, `not_member_of` and `not_contains` compile to `NOT EXISTS (...)`, not to a negated
join predicate. The join formulation silently excludes contacts that have *no* rows at all — a
contact with no tags does not satisfy `tag != x` under a join, though it plainly should.

This is the same lesson as `mailable_list_members`'s four separate `NOT EXISTS`. The reviewer
check is: for every negative operator, a contact with **zero** related rows must match.

`is_not_set` on an `attr:` field must match both a missing key and a JSON `null`.

### 4.6 Engagement rules are reserved, not implemented

The AST reserves `field` values `opened`, `clicked`, `bounced` and `sent`. In M1b they are
`422 not-yet-available` with a detail naming the milestone. They are written down now so the
builder's shape does not change when sends land, and so a stored segment from M1b never has to be
migrated.

### 4.7 A segment is a selection predicate, not an eligibility one

`mailable_list_members` answers "may we mail this person". A segment answers "which contacts does
this rule describe". They are different questions and must not be merged:

- A segment may deliberately describe suppressed or unsubscribed contacts — "everyone who
  unsubscribed in the last 30 days" is a legitimate segment and a reporting need.
- Eligibility is applied at **send** time, on top of whatever audience a campaign names.

So the compiler never silently adds a suppression filter. `suppressed` is an explicit field a
rule may use. When M2 launches a campaign against a segment it intersects with
`mailable_list_members` at that point, and the UI says how many the intersection removed.

### 4.8 A segment's rules can change under a selection token

M1a's token binds `{workspace_id, filter, as_of, issued}` and refuses a widened delete. A segment
introduces a new way for the predicate itself to move: someone edits the rules between the
selection and the action.

The token therefore carries `segment_version` whenever the filter names a segment, and
`begin_bulk` refuses with `409 segment-changed` if the stored version differs. This is a refusal
for **every** action kind, not only the consent-creating ones, because a changed predicate means
the token no longer describes what the person was shown — that is a different failure from the
data moving underneath a stable predicate.

---

## 5. Counts: exact when cheap, estimated when not

`spec.md:457` budgets p95 < 2 s for a count preview at 10M contacts and explicitly allows an
estimate while typing.

`POST /segments/preview` returns `{ "count": n, "exact": true|false, "took_ms": n }`.

The implementation runs the count inside a transaction with `SET LOCAL statement_timeout =
'1500ms'`. On timeout it falls back to the planner's estimate from `EXPLAIN (FORMAT JSON)` and
returns `exact: false`. The UI renders an exact count plainly and an estimate as `~12,400`, with
the distinction available to assistive technology as text, not colour.

**The fallback must be an honest fallback.** It is not acceptable to always return the estimate
because it is faster, nor to return `exact: true` for a planner number. A test asserts that a
cheap segment reports `exact: true` and that a deliberately expensive one reports `exact: false`
rather than erroring or hanging.

`statement_timeout` is set `LOCAL`, inside the transaction, so it cannot leak to another request
on a pooled connection. That leak is a real failure mode of this pattern and the test suite
pins it: a timed-out preview must be followed by a normal query on the same pool that is not
subject to the short timeout.

---

## 6. Schema

```sql
-- 0016_segments.sql
create table segments (
  id uuid primary key,
  workspace_id uuid not null references workspaces(id) on delete cascade,
  name text not null,
  description text not null default '',
  rules jsonb not null,
  version integer not null default 1,
  created_by uuid references users(id) on delete set null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  unique (workspace_id, name),
  unique (workspace_id, id)
);
create index segments_keyset on segments (workspace_id, id desc);

-- 0017_segment_support_indexes.sql
create index contacts_attributes_gin on contacts using gin (attributes jsonb_path_ops);
create index contacts_updated_at on contacts (workspace_id, updated_at);
create index contact_tags_created_at on contact_tags (workspace_id, created_at);

-- 0018_membership_updated_at.sql
alter table list_memberships add column updated_at timestamptz not null default now();
update list_memberships set updated_at = created_at;
create index list_memberships_updated_at on list_memberships (workspace_id, updated_at);

-- 0019_imports.sql
create table imports (
  id uuid primary key,
  workspace_id uuid not null references workspaces(id) on delete cascade,
  filename text not null,
  status text not null check (status in ('pending','running','succeeded','failed','cancelled')),
  mapping jsonb not null,
  total_rows bigint,
  inserted bigint not null default 0,
  updated bigint not null default 0,
  skipped bigint not null default 0,
  failed bigint not null default 0,
  error text,
  created_by uuid references users(id) on delete set null,
  created_at timestamptz not null default now(),
  finished_at timestamptz,
  unique (workspace_id, id)
);
create index imports_keyset on imports (workspace_id, id desc);

create table import_errors (
  workspace_id uuid not null,
  import_id uuid not null,
  row_number bigint not null,
  column_name text,
  message text not null,
  raw_value text,
  foreign key (workspace_id, import_id) references imports (workspace_id, id) on delete cascade,
  primary key (workspace_id, import_id, row_number, message)
);
```

Notes that are not obvious from the DDL:

- `jsonb_path_ops` rather than the default GIN opclass: it is smaller and faster for the
  containment queries segment rules generate, at the cost of not supporting key-existence
  queries. `is_set` therefore compiles to `attributes ? $n`, which this opclass cannot serve —
  that one operator takes a sequential scan on a large table and §5's estimate path covers it.
  Recorded here so the next person does not "fix" the opclass without measuring.
- `import_errors.raw_value` **may contain personal data**, because the whole point is to show the
  operator the cell that failed. It is deleted with its import and §8.5 caps how long an import
  row is kept.
- `list_memberships.updated_at` is new and §10 is the reason.

---

## 7. Import

### 7.1 The pipeline

1. **Upload.** The file is streamed to a temporary path. A row is written to `imports` with
   `status = 'pending'`.
2. **Sniff.** The first 100 rows are parsed and returned with the detected delimiter, a header
   guess, and a proposed mapping. Nothing is written to `contacts` yet.
3. **Map.** The operator confirms the mapping of CSV column → contact field or custom field,
   chooses the de-duplication behaviour, and optionally names a list to subscribe to.
4. **Run.** Rows are streamed into a per-import staging table via `COPY`, then a single set-based
   statement upserts from staging into `contacts`.
5. **Report.** Counts land on `imports`; per-row failures land in `import_errors`.

### 7.2 `COPY` into staging, then one set-based upsert

`spec.md:459` budgets a 1M-row CSV at under two minutes. Row-at-a-time `INSERT … ON CONFLICT`
does not reach that on any hardware worth buying. The pipeline is:

```
create temporary table import_staging_<id> (…) on commit drop;
COPY import_staging_<id> FROM STDIN (FORMAT csv);
-- validate inside SQL, writing failures to import_errors
-- then one INSERT … SELECT … ON CONFLICT (workspace_id, email) DO UPDATE
```

Validation that can be expressed in SQL (email shape, numeric attributes, required fields) runs
there. Validation that cannot (normalisation through `users::normalize_email`) happens in Rust as
rows stream into `COPY`, which is the only place each row is touched individually.

**The normalisation must be the same function the rest of the system uses.** An import that
writes an address the application would have normalised differently creates a contact that can
never be matched again — the M1a lesson that plaintext and hash must derive from one
`normalize_email` applies here with the same force.

### 7.3 De-duplication

Within a workspace, identity is the normalised email. The operator chooses:

- **Update existing** (default) — matched rows are updated field by field, with blank CSV cells
  leaving the existing value alone. A blank cell is not a null; clearing a field through import
  requires an explicit sentinel, and the mapping UI says so.
- **Skip existing** — matched rows are counted in `skipped` and nothing is written.
- **Fail on duplicate** — any match aborts the import before any write.

Duplicates *within the file itself* are resolved last-wins, and the count is reported. Silently
collapsing them without saying so is how an operator ends up with the wrong data and no reason to
look.

### 7.4 Imports run in-process, with a guard

`spec.md:188` puts imports on `nm worker`. That worker does not exist yet, and building it is not
this milestone. Imports therefore run in the request's own task, and:

- A workspace may have **one** running import at a time. A second returns `409 import-in-progress`
  naming the running one. This is enforced with a Postgres advisory lock keyed on the workspace,
  not with a status check, so two concurrent requests cannot both pass the check.
- An import that is `running` when the process restarts is swept to `failed` with a stated
  reason on the next boot. A row stuck in `running` forever is worse than a clear failure.
- The cap is 1M rows per file in M1b. Above that the upload is refused with a detail naming the
  cap, rather than accepted and timed out.

When the worker lands, the queue replaces the advisory lock and the sweep; the staging-and-upsert
core does not change. This is written down so that work is a substitution, not a rewrite.

### 7.5 What an import may and may not do

An import **may** create contacts, update their fields, add tags, and subscribe them to a list.

An import **may not**:

- **Resurrect an unsubscribed membership.** Importing a row whose email is already
  `unsubscribed` on the target list leaves it unsubscribed and counts it in `skipped`. This is
  the exact M1a bulk-subscribe lesson: unsubscribing removes a row, it does not un-send, and an
  import that quietly re-subscribes people is the compliance failure the whole suppression design
  exists to prevent.
- **Create a contact whose address is erased.** `is_erased` refuses it, the row is counted in
  `failed`, and `import_errors` says the address was erased — without echoing anything beyond the
  address the operator supplied.
- **Bypass a suppression.** A suppressed address may be imported as a contact, because
  suppression is about mailing and not about existence, but it does not become mailable and the
  import summary says how many imported contacts are suppressed.

The import summary names each of these counts separately. "Imported 940,000 of 1,000,000" with no
breakdown is not a report.

---

## 8. Export

- `POST /contacts/export` takes the same `ContactFilter` the grid and bulk engine use — including
  `segment_id` — and streams CSV.
- The response streams; it is never buffered. A 10M-row export must not hold memory proportional
  to the result.
- Columns are the contact's own fields plus every live custom field, plus `lists` and `tags` as
  semicolon-joined names, plus a `suppressed` column.
- Export is **audited** — actor, filter, row count — because an export is the single easiest way
  to remove every address from the system, and an unaudited one is indistinguishable from a leak.
  The filter is recorded; the search term `q` is **not**, for the reason M1a recorded: audit
  metadata outlives a GDPR erasure.
- Export is `Role::Admin`. Editors can read contacts a page at a time; taking the whole book out
  of the building is a different act.
- Role gating, streaming and audit are pinned by tests.

---

## 9. Permissions

| Operation | Role |
|---|---|
| Read segments, preview a count | viewer |
| Create, edit, delete a segment | editor |
| Start an import, read its report | editor |
| Export contacts | **admin** |
| Everything inherited from M1a | unchanged |

---

## 10. Issue `04` is closed by bounding facts, not by counting them

M1a's widening guard counts: it guarantees no action touches more contacts than the dialog
showed, but not *which* contacts. A same-count swap still substitutes people nobody selected, and
an importer is precisely what makes that routine. The issue was filed as blocking for this
milestone.

The fix is to make the snapshot cover the **facts the predicate reads**, not just contact
creation. The token gains `as_of_time`, and the compiler requires every fact it joins to predate
it:

- `contact_tags.created_at <= as_of_time`
- `list_memberships.updated_at <= as_of_time`
- `contacts.updated_at <= as_of_time` for any rule reading a contact column

The consequences are worth stating precisely, because the guarantee changes shape:

- A fact **added** after the token (a tag applied, a membership created, a name edited to match)
  does not satisfy the bound, so the contact is not matched. The widening is gone at the source.
- A fact **removed** after the token (a tag deleted) leaves no row, so the contact is not matched
  either. That is *under*-inclusion: the action touches fewer contacts than were shown.

So the resolved set becomes a **subset of what the person saw**, always. That is strictly
stronger than the count bound and it is the guarantee we actually wanted. The count bound stays
as a cheap backstop and because it costs nothing to keep.

Under-inclusion is reported, not hidden: the existing `affected` / `selected` gap already carries
it, and the copy gains the case "some contacts changed after you selected them and were left
alone".

`list_memberships.updated_at` must be **maintained** on every status change, not only set on
insert. A trigger is the wrong tool — it hides the write from the reader of the handler. Every
status write goes through `selection::*` or `lists::*`, so each sets it explicitly, and a test
asserts that an unsubscribe moves it.

**The cost, stated:** three more index lookups per rule and a new index on each fact table. The
alternative considered and rejected was materialising the matched id set into a snapshot table at
selection time. That is exactly correct rather than merely subset-correct, but it costs a write
of up to 10M rows on a preview the user may never act on, plus a reaper for abandoned snapshots.
Bounding facts by time buys the property that matters at a fraction of the cost. If a future
requirement needs exact set fidelity — "act on precisely these, even if they changed" — that is
the materialised design and it is a different feature.

---

## 11. Issue `08`: `remove` is renamed to `unsubscribe`

`DELETE /lists/{id}/members/{contact_id}` sets the membership to `unsubscribed` and keeps the
row. Everything about it is named `remove`, which describes an operation the system does not
perform.

Renamed in this milestone, before any external client exists:

- route → `POST /lists/{list_id}/members/{contact_id}/unsubscribe`
- `remove_list_member` → `unsubscribe_list_member`, `remove_member` → `unsubscribe_member`
- audit action `LIST_MEMBER_REMOVED` → `LIST_MEMBER_UNSUBSCRIBED`

Existing audit rows keep the old action string. A rename that rewrites history makes the audit
log a worse record than it was; the reader of an old row should see what the system called it at
the time. The glossary gains both names with a note that one superseded the other.

---

## 12. API

```
GET    /api/v1/workspaces/{ws}/segments                 list, cursor-paginated
POST   /api/v1/workspaces/{ws}/segments                 create
GET    /api/v1/workspaces/{ws}/segments/{id}            read
PATCH  /api/v1/workspaces/{ws}/segments/{id}            merge-patch; bumps version on rule change
DELETE /api/v1/workspaces/{ws}/segments/{id}            delete
POST   /api/v1/workspaces/{ws}/segments/preview         count an unsaved rule document

POST   /api/v1/workspaces/{ws}/imports                  upload, returns a sniffed mapping proposal
POST   /api/v1/workspaces/{ws}/imports/{id}/start       confirm the mapping and run
GET    /api/v1/workspaces/{ws}/imports                  list
GET    /api/v1/workspaces/{ws}/imports/{id}             status and counts
GET    /api/v1/workspaces/{ws}/imports/{id}/errors      per-row failures, cursor-paginated
POST   /api/v1/workspaces/{ws}/contacts:batchUpsert     up to 10k contacts, synchronous
POST   /api/v1/workspaces/{ws}/contacts/export          streamed CSV
```

All of it follows M0 and M1a conventions: RFC 9457 problems, RFC 7386 merge patch, cursor
pagination as `{data, next_cursor}`, `deny_unknown_fields` on every body, a unique `operationId`
per handler, 404 before 403 across tenancy.

`PATCH` bumps `version` only when `rules` actually changes. Renaming a segment must not invalidate
every outstanding selection token, and a test pins that.

---

## 13. UI

- **Segments list** — name, description, a live count (loaded per row, not blocking the list),
  created by, updated. Empty state explains what a segment is in one sentence and offers to build
  one.
- **Rule builder** — groups with all/any, add and remove rules, field picker driven by the live
  custom-field definitions, operator picker driven by the field's kind, a value input whose
  control matches the kind (date picker for dates, toggle for booleans). The count updates as
  rules change, debounced, and never blocks typing.
- **Count rendering** — exact numbers plain, estimates as `~12,400` with the word "estimate"
  available as text.
- **Import wizard** — upload, then a mapping table with each CSV column, a proposed target, and a
  live sample from the file; then a confirmation naming exactly what will happen, including the
  de-duplication choice; then progress; then a report with each count broken out and a link to
  the per-row errors.
- **Import report** — the per-row error table is paginated and downloadable as CSV, because an
  operator fixing 4,000 bad rows works in a spreadsheet, not in a web table.
- Every new screen and dialog goes through the break harness (`worst-case-data.spec.ts`) with
  long values, and is axe-clean in both themes at 320 px and 200 % zoom. New dialogs are added to
  the `opens()` screens list; the `<dialog>` overflow rule from M1a already covers them.
- Route chunks stay under the per-chunk ceiling; the shell stays under 150 KB gzipped.

---

## 14. Deviations from `spec.md`, recorded

1. **Imports run in-process, not on `nm worker`** (`spec.md:188`). The worker is not built. §7.4
   states the guard, the cap, and what changes when it lands.
2. **The 1M-row cap is a refusal, not a target** (`spec.md:459` budgets 1M in under two minutes).
   M1b refuses larger files rather than accepting what it cannot finish. The cap lifts with the
   worker.
3. **Engagement rules are reserved and return 422** (`spec.md:106` implies the builder is
   complete). There are no events to match until M2.
4. **Export is admin-only.** `spec.md` does not say. §8 gives the reason.

---

## 15. Testing

Beyond the per-task tests:

- **The injection test.** An attribute key of `plan'); drop table contacts; --` compiles, runs,
  matches nothing, and `contacts` still exists. Shown to fail if any value is interpolated.
- **Negation over absence.** For every negative operator, a contact with zero related rows
  matches. This is the `mailable_list_members` lesson applied to the compiler.
- **The typo'd field is 422, not empty.** An `attr:` key naming no definition is refused. A
  segment that silently matches nothing is the M1a all-`None` filter again.
- **`statement_timeout` does not leak.** A timed-out preview is followed by a normal query on the
  same pool that is not subject to the short timeout.
- **Exact versus estimate.** A cheap segment reports `exact: true`; an expensive one reports
  `exact: false` rather than erroring.
- **The as-of fact bound.** A tag applied after a token is issued does not bring its contact into
  the action; a tag removed after the token takes its contact out, and the gap is reported.
- **A segment rule change invalidates its tokens** with `409 segment-changed`; a rename does not.
- **Import does not resurrect** an unsubscribed membership, and says so in `skipped`.
- **Import refuses an erased address** and reports it in `failed`.
- **A 1M-row import** completes within the budget on the dev container, or the test states the
  measured number so the regression is visible.
- **Export streams:** memory does not grow with row count, and the audit row exists and carries
  no `q`.
- **Playwright:** build a segment and see the count; import a CSV with a bad row and see the
  error report; export and check the file.
- Every assertion added in this milestone is shown to fail against deliberately broken code.
  Nine vacuous assertions were found in M1a; assume a tenth.

---

## 16. CI

No new jobs. The existing `rust`, `web`, `e2e`, `deny` and `spdx` jobs cover this milestone. The
bundle budgets already fail the build on a regression.

One addition: the import performance test is marked `#[ignore]` by default and run explicitly,
because a two-minute test does not belong in every push. The number it measures is recorded in
the task report so a regression is visible across milestones.

---

## 17. Open, non-blocking

- `is_set` cannot use the `jsonb_path_ops` index (§6). Measure before changing the opclass;
  a second index on `(attributes)` with the default opclass may be cheaper than switching.
- The planner estimate in §5 is only as good as `ANALYZE`. Whether autovacuum keeps `contacts`
  analysed enough at 10M rows under heavy import is unmeasured.
- Segment counts in the list view are N queries for N segments. Acceptable at the page size, and
  the obvious fix (one query with a lateral) is worth doing only if it measures badly.
- No segment is used by anything yet. M2 is where a campaign targets one, and that is where the
  intersection with `mailable_list_members` in §4.7 gets built and tested.
