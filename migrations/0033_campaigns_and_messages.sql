-- SPDX-License-Identifier: AGPL-3.0-only

-- Campaigns, their per-recipient messages, and the link from a control job to its campaign.
--
-- Every index is in this file: `sqlx::migrate!` wraps a migration in a transaction, so
-- `create index concurrently` is not available later (0026 section 6 has the long version).
--
-- Foreign keys, ruled on in ADR 0005 (the lock order):
--   * `workspaces -> campaigns`   cascade
--   * `campaigns  -> messages`    no action: a campaign with messages cannot be deleted by anything
--   * `campaigns  -> jobs`        cascade: a draft's test jobs go with it
-- `campaigns` is therefore a new foreign key parent, and the eleventh.

-- ---------------------------------------------------------------------------------------------
-- 1. `campaigns`
create table campaigns (
  id uuid primary key,
  workspace_id uuid not null references workspaces (id) on delete cascade,
  name text not null,
  state text not null default 'draft'
    check (state in ('draft', 'scheduled', 'sending', 'paused', 'sent', 'cancelled', 'failed')),
  -- The audience: the union of these lists, narrowed by `segment_id` when set. Neither is a
  -- foreign key. An array cannot be one, and a list or segment deleted after launch must not block
  -- or cascade into a campaign that has already frozen who it mails.
  list_ids uuid[] not null default '{}',
  segment_id uuid,
  -- The sender. Not foreign keys either: the launch re-checks the domain against the gate, and a
  -- deleted domain is a refusal with a reason, not a cascade.
  sending_domain_id uuid,
  from_local_part text,
  from_name text,
  reply_to text,
  -- One entry per content variant. A single variant is an ordinary send.
  variants jsonb not null default '[]' check (jsonb_typeof(variants) = 'array'),
  -- A/B: the share of the audience that gets the test, 10..100. Null when there is no test; 100 is
  -- a pure split with no winner phase.
  ab_test_percent smallint check (ab_test_percent between 10 and 100),
  -- The variant index an admin picked; phase 2 sends it to the remainder.
  winner smallint check (winner >= 0),
  schedule_mode text not null default 'instant' check (schedule_mode in ('instant', 'recipient_local')),
  schedule_zone text,
  -- Wall-clock time in `schedule_zone`, kept as typed so it can be shown back.
  schedule_local timestamp,
  -- The resolved instant. Null means now.
  send_at timestamptz,
  launch_job_id uuid,
  -- 1 is the test (or the whole audience); 2 is the remainder after a winner is picked.
  phase smallint not null default 1 check (phase in (1, 2)),
  -- Where audience resolution stopped, so a crashed launch resumes rather than restarts.
  resolve_cursor uuid,
  resolved boolean not null default false,
  -- The rendition every message is made from, compiled once at launch. Read only through
  -- `Frozen::from_column`, which refuses a shape the send path cannot use.
  frozen jsonb,
  frozen_at timestamptz,
  total bigint not null default 0,
  sent bigint not null default 0,
  failed bigint not null default 0,
  unknown bigint not null default 0,
  skipped bigint not null default 0,
  -- Why the worker moved the campaign, for the screen. System transitions write no audit row.
  failure_reason text,
  pause_reason text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  launched_at timestamptz,
  finished_at timestamptz,
  -- Not redundant with the primary key: Postgres requires a unique constraint on exactly these
  -- columns for a child's composite foreign key to point at.
  constraint campaigns_workspace_id_id_key unique (workspace_id, id)
);

-- The worker's "which campaigns are sending" question, answered without a scan of every draft and
-- every finished campaign.
create index campaigns_live on campaigns (state) where state in ('sending', 'paused');

comment on column campaigns.list_ids is
  'The audience lists. Not a foreign key; the audience is frozen at launch, so a later deletion of a list does not reach a launched campaign.';
comment on column campaigns.ab_test_percent is
  'Share of the audience that receives the A/B test, 10..100. Null: no test. 100: a pure split with no winner phase.';
comment on column campaigns.winner is
  'The variant index an admin picked after the test. Phase 2 sends it to the remainder.';
comment on column campaigns.schedule_local is
  'Wall-clock time in schedule_zone as the operator typed it. send_at is the resolved instant.';
comment on column campaigns.send_at is
  'The resolved send instant. Null means now. It is also the launch job''s run_after.';
comment on column campaigns.phase is
  '1: the test, or the whole audience when there is no winner phase. 2: the remainder, after a winner is picked.';
comment on column campaigns.resolve_cursor is
  'The last contact id resolved into messages, so a crashed launch resumes where it stopped.';
comment on column campaigns.frozen is
  'The compiled rendition, brand kit footer and unsubscribe link included, fixed at launch. Read only through Frozen::from_column.';
comment on column campaigns.pause_reason is
  'Set when the worker pauses the campaign; an admin pause leaves it null.';

-- ---------------------------------------------------------------------------------------------
-- 2. `messages`: one row per recipient, and the queue the send loop claims from.
create table messages (
  id uuid primary key,
  workspace_id uuid not null,
  campaign_id uuid not null,
  -- No foreign key, on purpose. Erasing a contact must not wait on, or cascade into, the send
  -- history, and a message to a contact since deleted is `skipped` at send.
  contact_id uuid not null,
  variant smallint not null default 0 check (variant >= 0),
  status text not null default 'queued'
    check (status in ('queued', 'submitting', 'sent', 'failed', 'unknown', 'skipped')),
  attempts int not null default 0 check (attempts >= 0),
  max_attempts int not null default 5 check (max_attempts > 0),
  run_after timestamptz not null default now(),
  relay_id uuid,
  response text,
  error text,
  created_at timestamptz not null default now(),
  sent_at timestamptz,
  -- Invariant 1: a contact is submitted at most once per campaign. The key sits on the row being
  -- claimed, so two workers and a crash cannot make a second one.
  constraint messages_campaign_contact_key unique (campaign_id, contact_id),
  -- No action: a campaign with messages cannot be deleted by anything, and a message cannot name
  -- another workspace's campaign.
  constraint messages_campaign_fk foreign key (workspace_id, campaign_id)
    references campaigns (workspace_id, id) on delete no action
);

-- The claim: per campaign, oldest due first.
create index messages_runnable on messages (campaign_id, run_after, id) where status = 'queued';
-- The sweep: `submitting` rows whose lease has run out.
create index messages_in_flight on messages (run_after) where status = 'submitting';

comment on column messages.run_after is
  'The lease. While queued it is when the message is next due; once submitting it is when the lease expires and the sweep records `unknown`.';
comment on column messages.attempts is
  'The claim increments it and fences the write-ahead update. A deferred relay spends none.';
comment on column messages.status is
  'submitting is written in a committed statement before SMTP and is never claimed. unknown is final and is never offered to another relay.';
comment on column messages.relay_id is
  'The relay that took the message. Not a foreign key: a deleted relay must not erase the record of where mail went.';

-- ---------------------------------------------------------------------------------------------
-- 3. `jobs.campaign_id`
-- Nullable, because most job kinds are not about a campaign. A composite foreign key with a NULL
-- column is not checked (MATCH SIMPLE), as with `list_id` in 0026; `jobs_campaign_shape` is what
-- stops the two kinds that need it from being written without it.
--
-- Cascade: deleting a draft takes its test jobs with it. `jobs_throttle_retention` admits that,
-- because it lets a referential action through (`pg_trigger_depth() > 1`).
alter table jobs
  add column campaign_id uuid,
  add constraint jobs_campaign_fk foreign key (workspace_id, campaign_id)
    references campaigns (workspace_id, id) on delete cascade,
  add constraint jobs_campaign_shape
    check (kind not in ('campaign_launch', 'campaign_test') or campaign_id is not null);

comment on column jobs.campaign_id is
  'The campaign a campaign_launch or campaign_test job is about; null for every other kind. Read it from here, not from the payload.';
