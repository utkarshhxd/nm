-- SPDX-License-Identifier: AGPL-3.0-only

-- A selection snapshot is the frozen set a bulk action runs against (spec M1b section 8). The
-- predicate is evaluated once, here, and the matching contact ids are written down; the token a
-- client then carries names this snapshot and nothing about the predicate. Re-evaluating a
-- predicate when the action runs is what let a second editor's write change what the action hit.
--
-- `filter` keeps the predicate for the audit row (section 8.7). The token carries none, so without
-- this column a `bulk/delete` of 417,004 contacts would be recorded as a snapshot id that is
-- deleted an hour later — a worse record than M1a had, on the one action that cannot be undone.
--
-- `unlogged`, like the members below and for the same reason — but the parent has a second one of
-- its own. Postgres truncates an unlogged table on crash recovery, on standby promotion and on
-- restore from a base backup, and it would truncate only the members: the parent would survive
-- with `size = 417004` and nothing behind it. `resolve` and `require_live` read only the parent,
-- so both would pass, and the action would answer `200 {selected: 0, affected: 0}` over a grid
-- with no rows — the exact answer `selection.rs` argues must never be given, because "zero
-- contacts matched" is a different fact from "this selection is gone". Truncating both makes the
-- crash produce the `422 selection-expired` the members' comment already claims it does. The only
-- thing lost is `filter` for the audit row of a bulk action that, by construction, cannot happen.
create unlogged table selection_snapshots (
  id uuid primary key,                -- UUIDv7: carries its own issue time, see section 8.5
  workspace_id uuid not null references workspaces(id) on delete cascade,
  -- Exactly one of these is set: a selection is made by a signed-in user or by an API key, and
  -- a new selection supersedes only that same actor's previous one. One nullable `created_by`
  -- cannot carry a key, and NULL does not compare equal to NULL, so every key in a workspace
  -- would share one unsuperseded pile: at 10 selections a minute that is 600 live snapshots an
  -- hour against a reaper that clears 60.
  created_by uuid references users(id) on delete set null,
  created_by_key uuid references api_keys(id) on delete set null,
  filter jsonb not null,
  size bigint not null,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  unique (workspace_id, id)
);
-- The reaper's index: one expired snapshot per pass, oldest first (section 8.10).
create index selection_snapshots_expiry on selection_snapshots (expires_at);
-- A new selection supersedes the actor's previous live one in the workspace (section 8.4).
create index selection_snapshots_actor
  on selection_snapshots (workspace_id, created_by, created_by_key);

-- `unlogged` on purpose: this table is ephemeral, hourly and recreatable. Logged, it would push
-- ~158 MB per 1M-row selection through WAL and into every base backup. A crash truncates it and
-- its parent together, which produces the same "your selection expired, select again" path the
-- one-hour TTL already produces. The cost is that snapshots do not reach a physical standby.
--
-- It is also the first tenant table holding a `contact_id` without a composite foreign key to
-- `contacts`, against M1a's "unrepresentable, not merely unwritten" (section 8.6). The foreign key
-- is omitted deliberately: its cascade needs an index on `(workspace_id, contact_id)` that this
-- primary key cannot serve, so a `bulk/delete` of 1M contacts would do 1M index scans. A dangling
-- `contact_id` is harmless **because UUIDv7 is never reused** — that clause is load-bearing —
-- and because a bulk action narrows `contacts` by this table rather than reading ids out of it,
-- so a dangling row is a no-op rather than a foreign-key violation that kills the whole action.
create unlogged table selection_snapshot_members (
  workspace_id uuid not null,
  snapshot_id uuid not null,
  contact_id uuid not null,
  primary key (workspace_id, snapshot_id, contact_id),
  foreign key (workspace_id, snapshot_id)
    references selection_snapshots (workspace_id, id) on delete cascade
);
