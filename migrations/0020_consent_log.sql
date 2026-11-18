-- SPDX-License-Identifier: AGPL-3.0-only

-- One row per consent change: the answer, years later, to "prove this person agreed". Written in
-- the same transaction as the membership change it records, so neither can outlive the other.
-- BOTH foreign keys are composite and carry `workspace_id`, as `list_memberships` does: a row
-- joining a contact of one workspace to a list of another cannot be written at all. The database
-- refuses it, not a handler. `list_id` is null for a workspace-wide suppression; a null column
-- skips its foreign key, so the contact key alone still holds that row to its workspace.
--
-- The two keys differ on delete, on purpose. Do not make them match.
--  * `contact_id` cascades. `delete from contacts` is how this product erases a person, and a
--    row here carries their IP and user agent: keeping it after erasure would defeat the erasure.
--    The record goes with the person.
--  * `list_id` sets null. Deleting a list is routine housekeeping and must not destroy the proof
--    that contacts who still exist consented. The row keeps its action, source, IP, user agent
--    and time; only the pointer to the vanished list goes. The column list `(list_id)` is
--    required: the bare form would also null `workspace_id`, which is `not null` (Postgres 15+).
-- No `updated_at`: a row is never changed.
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
  foreign key (workspace_id, list_id) references lists (workspace_id, id) on delete set null (list_id));

comment on table consent_log is
  'Rows are never updated, and are removed only when the contact they belong to is erased.';

create index consent_log_by_contact on consent_log (workspace_id, contact_id, created_at desc);

-- Enforces the comment. DELETE is left alone: the contact cascade is how erasure happens. The one
-- update allowed is the `on delete set null (list_id)` above. Postgres runs that as an UPDATE
-- that fires this trigger, so a blanket refusal would make every list deletion fail. It is told
-- apart from a hand-written `update ... set list_id = null` by `pg_trigger_depth()`: 1 for a
-- statement, 2 for the referential action (measured on 16.15). That matters because `list_id` is
-- evidence, and null already means "workspace-wide": nulling it by hand would silently turn a
-- list-scoped row into a workspace-wide one. The `to_jsonb` comparison makes the allowed update
-- change nothing else, and covers a column added later without anyone editing this function.
-- Limit: an UPDATE issued from inside another trigger also runs at depth 2 and is not told apart.
create function consent_log_append_only() returns trigger language plpgsql as $$
begin
  if pg_trigger_depth() > 1 and old.list_id is not null and new.list_id is null
     and to_jsonb(new) - 'list_id' = to_jsonb(old) - 'list_id' then
    return new;
  end if;
  raise exception 'consent_log is append-only: rows are never updated';
end $$;

create trigger consent_log_no_update before update on consent_log
  for each row execute function consent_log_append_only();
