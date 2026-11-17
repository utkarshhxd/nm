-- SPDX-License-Identifier: AGPL-3.0-only

-- One row per CSV upload, and one row per CSV row it rejected (spec M1c section 7).
--
-- THE FIVE ROW-LEVEL COUNTS ARE MUTUALLY EXCLUSIVE: every row of the file lands in exactly one of
-- `imported`, `updated`, `skipped`, `collapsed`, `failed`, so on a `complete` import those five sum to
-- `total_rows` (a `failed` one died part-way: they may not sum, and `total_rows` may be NULL). The
-- report test asserts exactly that for `complete`. `suppressed` and `memberships_declined` are NOT row-level: they
-- describe rows already counted in one of the five (a suppressed address is still an `imported` or
-- `updated` contact, whose membership is simply not mailable; a declined membership is a contact
-- that was imported or updated while its subscription was refused). Adding all seven counts rows
-- twice, and a reader who does will think the sum assertion is wrong. It is not.
--
-- `collapsed` is its own count, not part of `skipped`: it is a row dropped because an EARLIER row
-- of the same file names the same address (the file de-duplicates itself), where `skipped` is a
-- row the importer chose not to apply to a contact that already exists. Folding one into the other
-- reports "these were already in your database" for rows the database never had.
--
-- `memberships_declined` exists because section 7.5 files a row whose membership is already
-- `unsubscribed` under `skipped`, while section 7.3 uses `skipped` for de-duplication. They are
-- different events: in the first the contact IS still imported or updated and only the
-- subscription is declined. Merging them is the breakdown-free report section 7.5 forbids.
--
-- `total_rows` is nullable on purpose: the file is streamed into the database by `COPY`, so the
-- row count is unknown until that finishes. NULL means "still counting", which is true; a `not
-- null default 0` would report an empty file for an import that has not read its first row.
--
-- `delimiter` and `has_header` are what the sniff reported at upload, stored because the operator
-- confirms a mapping against THOSE columns: a second sniff at start could disagree and silently
-- re-point every column. THE RULE FOR THE RUN: use these stored values, never sniff again. The
-- start request may carry optional `delimiter` and `has_header` OVERRIDES, which replace the stored
-- values: a wrong guess (a headerless file read as headed silently drops its first row; a
-- misdetected delimiter misparses every row) is corrected by the operator who was shown it, which
-- is not a silent disagreement. A request that carries neither uses what is stored here.
-- `delimiter` is the delimiter byte
-- as a `smallint` (0..=255), not `"char"`: `"char"` is Postgres's internal one-byte type and
-- sqlx has no friendly mapping for a byte in it, while `smallint` reads back as `i16` and the
-- check keeps it a byte. Both are not null: an import row exists only because an upload was sniffed.
--
-- `unique (workspace_id, id)` is not redundant with the primary key: `import_errors` takes a
-- composite foreign key onto it, so an error row cannot name another workspace's import. The
-- database refuses it, not a handler.
create table imports (id uuid primary key,
  workspace_id uuid not null references workspaces(id) on delete cascade,
  status text not null check (status in ('pending','running','complete','failed')),
  filename text not null,
  delimiter smallint not null check (delimiter between 0 and 255), has_header boolean not null,
  mapping jsonb, dedup text, list_id uuid,
  imported bigint not null default 0, updated bigint not null default 0,
  skipped bigint not null default 0, failed bigint not null default 0,
  collapsed bigint not null default 0,
  suppressed bigint not null default 0, memberships_declined bigint not null default 0,
  total_rows bigint,
  error_reason text, created_by uuid references users(id) on delete set null,
  created_at timestamptz not null default now(), started_at timestamptz, finished_at timestamptz,
  unique (workspace_id, id),
  -- Composite, so an import cannot name another workspace's list. The column list on `set null` is
  -- load-bearing: a bare `set null` would null `workspace_id` too (not null) and deleting a list
  -- would fail. Cascade is wrong: deleting a list must not delete the import's history.
  foreign key (workspace_id, list_id) references lists (workspace_id, id) on delete set null (list_id));

-- The rejected rows, for the download and for the cursor that pages them: `row_number` is the CSV
-- row, so the index returns them in file order.
create table import_errors (import_id uuid not null, workspace_id uuid not null,
  row_number bigint not null, column_name text, reason text not null, value text,
  foreign key (workspace_id, import_id) references imports (workspace_id, id) on delete cascade);

create index import_errors_by_import on import_errors (workspace_id, import_id, row_number);
