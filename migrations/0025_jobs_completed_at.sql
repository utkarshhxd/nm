-- SPDX-License-Identifier: AGPL-3.0-only

-- A job that has been done is KEPT and marked, never deleted. `0023_forms_and_jobs.sql` wrote the
-- claim protocol as a requirement on the consumer and said what happens to a row that gives up
-- (`failed_at`, kept, so "this confirmation was never sent" has an answer months later). It said
-- nothing about a row that succeeded, and the plain reading of "M2 owns the worker that drains
-- them" is that a drained row goes away.
--
-- It must not, and this is the column that says so.
--
-- WHY, in one sentence: the hosted signup form's resend throttle has no state of its own and reads
-- these rows. `src/public/forms.rs` re-enqueues a `confirmation_email` for a membership that is
-- still `pending` — that is how somebody who lost their confirmation email gets another — and the
-- only thing stopping an unauthenticated route from sending one on every submission is "was there
-- already a `confirmation_email` row for this (contact, list) in the last fifteen minutes". A
-- worker that deletes on success makes that question answer "no" every time. The route becomes a
-- mailer that anybody can aim at any address, and **nothing goes red**: no test fails, no
-- constraint is violated, and the first sign is somebody's inbox.
--
-- So, as a requirement on whoever drains this queue, in the same voice as `0023`'s claim protocol:
--
--   * On success, `update jobs set completed_at = now() where id = $1`. Do NOT `delete`.
--   * The claim's predicate gains `completed_at is null`, beside `failed_at is null`:
--
--         where run_after <= now() and failed_at is null and completed_at is null
--         order by run_after, id
--         for update skip locked
--         limit $batch
--
--     Without that a completed row's lease expires and it is claimed again, for ever.
--   * Deleting old rows in bulk is housekeeping, not draining, and it may only remove rows older
--     than the longest throttle window that reads them. Fifteen minutes today
--     (`RESEND_AFTER_MINUTES`); a day is a safe floor if one is ever wanted.
--
-- `0023` is not edited to say this: it has already been applied, and a third checksum change would
-- refuse `sqlx migrate run` on every database that has run it.
alter table jobs add column completed_at timestamptz;

-- The claim index, re-made to carry the amended predicate. Partial on both terminal states, so a
-- completed row leaves the index the moment it is marked rather than being read on every poll for
-- the rest of the table's life. Free to do now: `jobs` has no consumer yet, so it holds only rows
-- M1d has written.
drop index jobs_runnable;
create index jobs_runnable on jobs (run_after, id)
  where failed_at is null and completed_at is null;
