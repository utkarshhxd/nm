-- SPDX-License-Identifier: AGPL-3.0-only

-- The people a workspace sends to. There is deliberately NO status column (no `subscribed`,
-- `active`, ...): a workspace-wide opt-out is a `suppressions` row keyed by address, so it survives
-- both GDPR erasure (which deletes this row) and a re-import (which would clear a status column).
-- `email` is stored already normalised by `users::normalize_email`; Rust is the one authority, so
-- no citext and no `lower()` index. `unique (workspace_id, id)` is not redundant with the primary
-- key: later tables take composite foreign keys onto it, so a row in one workspace cannot
-- reference a contact in another. No GIN index on `attributes`: nothing queries it until the
-- segment engine, and one would defeat HOT updates on a column every update touches.
create extension if not exists pg_trgm;

create table contacts (id uuid primary key,
  workspace_id uuid not null references workspaces(id) on delete cascade,
  email text not null,
  email_domain text not null generated always as (substring(email from position('@' in email) + 1)) stored,
  first_name text not null default '', last_name text not null default '',
  attributes jsonb not null default '{}'::jsonb,
  timezone text,
  created_at timestamptz not null default now(), updated_at timestamptz not null default now(),
  unique (workspace_id, email), unique (workspace_id, id));

create index contacts_keyset_created on contacts (workspace_id, id desc);
create index contacts_keyset_email on contacts (workspace_id, email, id);
create index contacts_domain on contacts (workspace_id, email_domain);
create index contacts_search on contacts using gin ((email || ' ' || first_name || ' ' || last_name) gin_trgm_ops);
