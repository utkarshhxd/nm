-- SPDX-License-Identifier: AGPL-3.0-only

-- Which claim holds a message, and the index the sweep reads to fail a message no claim finished.

-- ---------------------------------------------------------------------------------------------
-- 1. `messages.claim_token`
-- `attempts` cannot say which claim holds a row: a release and a deferral each give an attempt
-- back, so the same value recurs across claims. Every claim mints a fresh token, and
-- `mark_submitting`, `record` and `release` match on it, so a stale holder or a replayed flush
-- touches nothing. Without it, a replayed `Retry` could move another worker's `submitting` row
-- back to `queued` while that row is at the relay, and the next claim would send it again.
alter table messages add column claim_token uuid;

comment on column messages.claim_token is
  'Minted by each claim; mark_submitting, record and release match on it. Null when no claim holds the row: never claimed, released, recorded or swept.';

-- ---------------------------------------------------------------------------------------------
-- 2. The stranded-row index
-- A `queued` row claimed `max_attempts` times with no outcome is never claimed again, and the
-- sweep fails it so its campaign can finish. The sweep runs every tick, and without this index
-- it would scan every queued row of every campaign to find a set that is almost always empty.
create index messages_exhausted on messages (run_after)
  where status = 'queued' and attempts >= max_attempts;
