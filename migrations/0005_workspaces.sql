-- SPDX-License-Identifier: AGPL-3.0-only

create table workspaces (id uuid primary key, slug text not null unique, name text not null,
  timezone text not null default 'UTC', created_at timestamptz not null default now(), updated_at timestamptz not null default now());
create table memberships (workspace_id uuid not null references workspaces(id) on delete cascade,
  user_id uuid not null references users(id) on delete cascade,
  role text not null check (role in ('viewer','editor','admin','owner')),
  created_at timestamptz not null default now(), primary key (workspace_id, user_id));
create index memberships_user on memberships (user_id);
