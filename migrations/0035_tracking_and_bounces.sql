-- SPDX-License-Identifier: AGPL-3.0-only

-- M2d: open and click tracking, and the send-time hard bounce.
--
-- Every index is in this file, for the reason `0033` gives: `sqlx::migrate!` wraps a migration in
-- a transaction, so `create index concurrently` is not available later.
--
-- Re-adding the status check scans `messages` under an exclusive lock, and the `consent_log` index
-- blocks its writes while it builds. Both tables are small at launch.
--
-- No foreign key is added (ADR 0005): the graph `0033` ruled on is unchanged.

-- ---------------------------------------------------------------------------------------------
-- 1. `messages`: the `bounced` status, and the open and click marks.
alter table messages drop constraint messages_status_check;
alter table messages add constraint messages_status_check
  check (status in ('queued', 'submitting', 'sent', 'failed', 'bounced', 'unknown', 'skipped'));

alter table messages
  add column opened_at timestamptz,
  add column opens int not null default 0,
  add column clicked_at timestamptz,
  add column clicks int not null default 0;

comment on column messages.status is
  'submitting is written in a committed statement before SMTP and is never claimed. unknown is final and is never offered to another relay. bounced is final: a permanent refusal naming the recipient, recorded in the same transaction as the suppression of the address.';

-- ---------------------------------------------------------------------------------------------
-- 2. `campaigns.bounced`, beside the other outcome counters.
alter table campaigns add column bounced bigint not null default 0;

-- ---------------------------------------------------------------------------------------------
-- 3. `message_clicks`: one row per message and link that was clicked.
create table message_clicks (
  message_id uuid,
  link int,
  workspace_id uuid not null,
  campaign_id uuid not null,
  variant smallint not null,
  clicks int not null default 1,
  first_at timestamptz not null default now(),
  last_at timestamptz not null default now(),
  primary key (message_id, link)
);

create index message_clicks_by_campaign on message_clicks (campaign_id);

comment on table message_clicks is
  'No foreign keys, on purpose (spec M2d 4.7.2). A row is written only from a messages row in the same statement, and nothing deletes messages. A key to messages would make it a twelfth parent, and a key to campaigns a third child edge, in the graph ADR 0005 rules on.';

-- ---------------------------------------------------------------------------------------------
-- 4. `consent_log`: the report counts a campaign's unsubscribes by `source_ref`.
create index consent_log_by_source_ref on consent_log (workspace_id, source_ref)
  where source_ref is not null;
