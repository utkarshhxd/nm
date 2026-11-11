-- SPDX-License-Identifier: AGPL-3.0-only

-- No foreign keys on purpose: audit rows must outlive deleted users, API keys and workspaces.
create table audit_log (
  id uuid primary key, workspace_id uuid, actor_user_id uuid, actor_api_key_id uuid,
  action text not null, target_type text, target_id text, ip inet, user_agent text,
  metadata jsonb not null default '{}', created_at timestamptz not null default now());
create index audit_log_ws_id on audit_log (workspace_id, id desc);
