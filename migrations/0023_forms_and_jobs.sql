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
-- `slug` is unique per workspace, which is what the M1d plan specifies. NOTE for Task 9: the
-- public route is `/f/{slug}` with no workspace in the path, so two workspaces using the same
-- slug cannot both be served from it. Resolving that needs either a global unique index here or a
-- workspace-scoped public path; it is recorded in the Task 8 report rather than decided here.
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
-- `attempts` is the worker's retry counter, `run_after` its earliest run time; both have defaults
-- so an enqueue names only `workspace_id`, `kind` and `payload`.
create table jobs (id uuid primary key,
  workspace_id uuid not null references workspaces(id) on delete cascade,
  kind text not null,
  payload jsonb not null,
  run_after timestamptz not null default now(),
  attempts int not null default 0,
  created_at timestamptz not null default now());

-- The worker's claim query: oldest runnable row first. `id` is a v7 uuid, so it breaks ties by
-- insertion order.
create index jobs_runnable on jobs (run_after, id);
