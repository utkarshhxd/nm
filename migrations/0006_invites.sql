-- SPDX-License-Identifier: AGPL-3.0-only

-- `token_hash` is sha256 of the invite token: the raw token is shown once, at creation, and never stored.
create table invites (id uuid primary key, workspace_id uuid not null references workspaces(id) on delete cascade,
  email text not null, role text not null check (role in ('viewer','editor','admin','owner')),
  token_hash bytea not null unique, invited_by uuid references users(id) on delete set null,
  expires_at timestamptz not null, accepted_at timestamptz, revoked_at timestamptz, created_at timestamptz not null default now());
create index invites_ws on invites (workspace_id);
