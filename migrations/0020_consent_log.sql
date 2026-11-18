-- SPDX-License-Identifier: AGPL-3.0-only

-- One row per consent change: the answer, years later, to "prove this person agreed". Written in
-- the same transaction as the membership change it records, so neither can outlive the other.
-- BOTH foreign keys are composite and carry `workspace_id`, as `list_memberships` does: a row
-- joining a contact of one workspace to a list of another cannot be written at all. The database
-- refuses it, not a handler. `list_id` is null for a workspace-wide suppression; a null column
-- skips its foreign key, so the contact key alone still holds that row to its workspace.
-- `on delete cascade` lets erasing a contact take its IP and user agent with it, and lets a
-- workspace delete clean up. No `updated_at`: a row is never changed.
create table consent_log (id uuid primary key,
  workspace_id uuid not null,
  contact_id uuid not null,
  list_id uuid,
  action text not null check (action in ('subscribed','confirmed','unsubscribed','resubscribed','suppressed')),
  source text not null check (source in ('form','import','api','operator','one_click','preference_center','bounce','complaint')),
  -- The form id, the import id, the user id: whatever caused the change.
  source_ref text,
  -- Personal data. Kept as evidence; never logged.
  ip inet, user_agent text,
  -- The public token that authorised the change; written once the public pages record it.
  token_jti text,
  created_at timestamptz not null default now(),
  foreign key (workspace_id, contact_id) references contacts (workspace_id, id) on delete cascade,
  foreign key (workspace_id, list_id) references lists (workspace_id, id) on delete cascade);

comment on table consent_log is
  'Append-only: rows are never updated or deleted by the application. Only the deletion of a parent contact, list or workspace removes them.';

create index consent_log_by_contact on consent_log (workspace_id, contact_id, created_at desc);
