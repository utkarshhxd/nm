-- SPDX-License-Identifier: AGPL-3.0-only

-- A segment is a named, saved predicate over contacts: it stores rules, never a membership list.
-- `rules` is the validated rule document (`segments::rules`), replaced wholesale on every edit;
-- `version` moves only when that document actually changes. `unique (workspace_id, id)` is not
-- redundant with the primary key: later tables take composite foreign keys onto it.
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
