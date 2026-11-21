-- SPDX-License-Identifier: AGPL-3.0-only

-- Signup forms: the operator's description of a hosted page that a stranger fills in. The page
-- itself and the submission are M1d Task 9; this is the model and the authenticated CRUD behind
-- it.
--
-- `fields` is jsonb rather than a child table because a form's fields are read and written as one
-- whole — the editor replaces the list, nothing ever queries "every form collecting a timezone",
-- and ordering is part of the value. The shape is `[{"target": …, "required": bool,
-- "label": "…"}]`, where `target` is the adjacently tagged `FormTarget` of `src/forms/mod.rs`:
-- `{"kind":"email"}` … `{"kind":"custom","value":"<custom_fields.key>"}`. There is deliberately
-- no `lists` target: a public form may not let its submitter name the lists they join.
--
-- `redirect_url` is validated on the way IN (absolute http/https only, `src/forms/mod.rs`), not
-- on the way out. `public::page::esc` escapes markup, not URL schemes, so a stored
-- `javascript:` URL would be one careless renderer away from executing. It cannot be stored.
--
-- `unique (workspace_id, id)` is not redundant with the primary key: `form_lists` takes a
-- composite foreign key onto it, exactly as `list_memberships` does onto `lists` in `0012`.
--
-- `slug` is unique per workspace, not globally, and the public route carries the workspace in
-- front of it: `/f/{workspace}/{slug}`, where `{workspace}` is `workspaces.slug` (already
-- `not null unique`, `0005_workspaces.sql:3`). Spec §7.2 was amended to this on 2026-11-20, after
-- Task 8 found it saying `/f/{slug}` with no workspace in the path, which no per-workspace slug
-- can serve. A globally unique form slug was rejected: it would hand the first tenant to register
-- `newsletter` a name no other tenant could ever use, and fixing it after launch would be a
-- breaking URL change for embed codes already pasted into customers' sites.
create table forms (id uuid primary key,
  workspace_id uuid not null references workspaces(id) on delete cascade,
  slug text not null,
  name text not null,
  fields jsonb not null default '[]'::jsonb,
  redirect_url text,
  headline text not null default '',
  button_label text not null default '',
  created_at timestamptz not null default now(), updated_at timestamptz not null default now(),
  unique (workspace_id, slug), unique (workspace_id, id));

-- The lists one submission subscribes to. A separate table rather than a uuid[] column on `forms`
-- because Postgres has no foreign key on an array element, and "every list_id belongs to this
-- workspace" must be the database's answer, not a handler's: BOTH keys are composite and carry
-- `workspace_id`, so a form of one workspace pointing at a list of another cannot be written at
-- all. That is also what refuses a cross-workspace `list_id` at create time, with SQLSTATE 23503.
--
-- `list_id` cascades: deleting a list is routine housekeeping and must not be blocked by a form.
-- The consequence is that a form can be left with no lists at all, which the create/update
-- validation ("at least one list") cannot prevent. Task 9 must therefore treat a form with no
-- lists as not accepting submissions, rather than assuming the invariant holds at read time.
create table form_lists (workspace_id uuid not null,
  form_id uuid not null, list_id uuid not null,
  primary key (form_id, list_id),
  foreign key (workspace_id, form_id) references forms (workspace_id, id) on delete cascade,
  foreign key (workspace_id, list_id) references lists (workspace_id, id) on delete cascade);

create index form_lists_by_list on form_lists (workspace_id, list_id);

-- The work queue. M1d only ever writes rows; M2 owns the worker that drains them. That makes the
-- row shape a contract between milestones rather than an implementation detail, so it is written
-- as one and the columns below are stable.
--
-- `kind` is a stable string, not an enum: M2 adds kinds without a migration here. The one kind
-- M1d enqueues is `confirmation_email`, whose `payload` M2 parses as
--
--     {"contact_id": uuid, "list_id": uuid, "email": "…", "token": "<public confirm token>"}
--
-- `token` is the ALREADY-ISSUED token string from `public::token::issue`, carried verbatim. It is
-- not a new field inside the token: `src/public/token.rs` documents that the token's field list
-- is frozen once the first unsubscribe link ships (`deny_unknown_fields`, no TTL on
-- `Purpose::Unsubscribe`), so anything a job needs travels beside the token, never inside it.
-- One row per (contact, list): a `Confirm` token carries exactly one `list_id`, so a submission
-- naming several double-opt-in lists enqueues several rows.
--
-- HOW A ROW IS CLAIMED. This is a REQUIREMENT ON THE CONSUMER, not a description of one possible
-- worker. M2 must claim with a single statement of this shape:
--
--     update jobs set attempts = attempts + 1, run_after = now() + <lease>
--     where id in (select id from jobs
--                  where run_after <= now() and failed_at is null
--                  order by run_after, id
--                  for update skip locked
--                  limit $batch)
--     returning *;
--
-- Two things in it are load-bearing and neither is optional:
--
--  * `for update skip locked`. Without it, two pollers select the same row and both run it.
--  * `run_after` advanced in the SAME statement. It is the LEASE, not merely a schedule. A claim
--    that leaves `run_after` in the past re-offers the row at the next poll, before the first
--    worker has finished with it.
--
-- A plain `select ... where run_after <= now()` followed by a separate update is NOT a claim:
-- between the two statements another worker reads the same row. For `confirmation_email` that is
-- one person receiving the same confirmation twice, and nothing here will catch it — there is no
-- unique constraint to lean on, because the same job may legitimately be enqueued twice (a
-- subscriber who submits the form again before confirming).
--
-- A worker that dies mid-job leaves the row claimed until its lease expires, at which point the
-- row becomes runnable again and `attempts` has already counted the dead attempt. That is the
-- intended behaviour: at-least-once delivery, with the duplicate bounded by the lease.
--
-- `kind` is a stable string, not an enum: M2 adds kinds without a migration here. The one kind
-- M1d enqueues is `confirmation_email`, whose `payload` M2 parses as
--
--     {"contact_id": uuid, "list_id": uuid, "email": "…", "token": "<public confirm token>"}
--
-- `token` is the ALREADY-ISSUED token string from `public::token::issue`, carried verbatim. It is
-- not a new field inside the token: `src/public/token.rs` documents that the token's field list
-- is frozen once the first unsubscribe link ships (`deny_unknown_fields`, no TTL on
-- `Purpose::Unsubscribe`), so anything a job needs travels beside the token, never inside it.
-- One row per (contact, list): a `Confirm` token carries exactly one `list_id`, so a submission
-- naming several double-opt-in lists enqueues several rows.
--
-- GIVING UP. `attempts` counts claims, `max_attempts` is where the worker stops: on the attempt
-- that reaches it, set `failed_at = now()` and `last_error` and leave the row for an operator to
-- read. A failed row is skipped by the claim above (`failed_at is null`) rather than deleted,
-- because "this confirmation was never sent" is the question someone will ask months later, and a
-- deleted row cannot answer it. These three columns follow `imports`, which keeps `error_reason`
-- beside its `failed` status for the same reason. They are here now because `jobs` is empty
-- today: adding them in M2 is a migration on a table that by then holds every email the product
-- has queued.
--
-- `run_after` is the earliest run time, and `created_at` is when it was enqueued — they diverge
-- the moment a row is claimed, so neither substitutes for the other. Every column but
-- `workspace_id`, `kind` and `payload` has a default, so an enqueue names only those three.
create table jobs (id uuid primary key,
  workspace_id uuid not null references workspaces(id) on delete cascade,
  kind text not null,
  payload jsonb not null,
  run_after timestamptz not null default now(),
  attempts int not null default 0,
  max_attempts int not null default 5,
  last_error text,
  failed_at timestamptz,
  created_at timestamptz not null default now());

-- The claim query's index, and it carries that query's own predicate: a partial index on the
-- rows still worth looking at. Rows that have given up stay in the table (see GIVING UP above)
-- and would otherwise sit in this index forever, read on every poll and never claimed.
-- `(run_after, id)` is the claim's `order by`; `id` is a v7 uuid, so it breaks ties by enqueue
-- order.
create index jobs_runnable on jobs (run_after, id) where failed_at is null;
