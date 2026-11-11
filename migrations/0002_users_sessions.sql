-- SPDX-License-Identifier: AGPL-3.0-only

-- `email` is stored trimmed and lowercased (users::normalize_email); the unique constraint relies on it.
-- `password_hash` is null for a user who has not set a password yet.
create table users (id uuid primary key, email text not null unique, display_name text not null default '',
  password_hash text, is_instance_admin boolean not null default false,
  created_at timestamptz not null default now(), updated_at timestamptz not null default now());
-- `id_hash` is sha256 of the cookie value: the raw session id is never stored.
create table sessions (id_hash bytea primary key, user_id uuid not null references users(id) on delete cascade,
  csrf_token text not null, mfa_pending boolean not null default false, mfa_failures int not null default 0,
  created_at timestamptz not null default now(), last_seen_at timestamptz not null default now(),
  expires_at timestamptz not null, ip inet, user_agent text);
create index sessions_user on sessions (user_id);
