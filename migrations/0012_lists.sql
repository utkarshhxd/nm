-- SPDX-License-Identifier: AGPL-3.0-only

-- The named sets of contacts a campaign is sent to, and each contact's subscription status on each
-- one. This is where "unsubscribed from this list" lives: a contact has no status of its own.
-- `unique (workspace_id, id)` is not redundant with the primary key: `list_memberships` takes a
-- composite foreign key onto it, and later tables will too.
create table lists (id uuid primary key,
  workspace_id uuid not null references workspaces(id) on delete cascade,
  name text not null, description text not null default '',
  double_optin boolean not null default true,
  -- Written now, read by nothing in M1a: the M1c preference center lists the `public` lists as
  -- its "topics", and this way it needs no migration.
  visibility text not null default 'private' check (visibility in ('public','private')),
  -- Set by an importer so a re-run finds the same list; null for a list made in the UI. Unused
  -- until then. Postgres treats nulls as distinct, so any number of UI lists may have none.
  source_ref text,
  created_at timestamptz not null default now(), updated_at timestamptz not null default now(),
  unique (workspace_id, name), unique (workspace_id, source_ref), unique (workspace_id, id));

-- A row is kept after an unsubscribe: that someone left is the fact worth keeping, and the send-time
-- check reads it. `pending` is written only on a `double_optin` list and is never mailable, so a
-- status means one thing wherever it is read, with no second lookup of the list's setting.
-- BOTH foreign keys are composite and carry `workspace_id`: a membership joining a contact of one
-- workspace to a list of another cannot be written at all. The database refuses it, not a handler.
create table list_memberships (workspace_id uuid not null,
  contact_id uuid not null, list_id uuid not null,
  status text not null check (status in ('pending','subscribed','unsubscribed')),
  subscribed_at timestamptz, unsubscribed_at timestamptz,
  created_at timestamptz not null default now(),
  primary key (contact_id, list_id),
  foreign key (workspace_id, contact_id) references contacts (workspace_id, id) on delete cascade,
  foreign key (workspace_id, list_id) references lists (workspace_id, id) on delete cascade);

create index list_memberships_by_list on list_memberships (workspace_id, list_id, status, contact_id);
