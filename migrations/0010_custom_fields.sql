-- SPDX-License-Identifier: AGPL-3.0-only

-- The typed schema a contact's `attributes` are validated against. `key` is the JSONB key every
-- contact's value lives under, so it never changes. Deleting a field sets `archived_at` and keeps
-- the values; only an explicit purge removes them. `unique (workspace_id, id)` is not redundant
-- with the primary key: later tables take composite foreign keys onto it, so a row in one
-- workspace cannot reference another workspace's field.
create table custom_fields (id uuid primary key,
  workspace_id uuid not null references workspaces(id) on delete cascade,
  key text not null, label text not null,
  kind text not null check (kind in ('text','number','date','datetime','boolean','select','multi_select')),
  options jsonb not null default '[]'::jsonb,
  archived_at timestamptz,
  created_at timestamptz not null default now(), updated_at timestamptz not null default now(),
  unique (workspace_id, key), unique (workspace_id, id));
