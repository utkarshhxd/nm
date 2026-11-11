-- SPDX-License-Identifier: AGPL-3.0-only

-- `prefix` locates the key; `secret_hash` is sha256 of the secret half. The whole key is shown once, at creation, and never stored.
create table api_keys (id uuid primary key, workspace_id uuid not null references workspaces(id) on delete cascade,
  name text not null, prefix text not null unique, secret_hash bytea not null,
  role text not null check (role in ('viewer','editor','admin')),
  created_by uuid references users(id) on delete set null, created_at timestamptz not null default now(),
  last_used_at timestamptz, revoked_at timestamptz);
create index api_keys_ws on api_keys (workspace_id);
