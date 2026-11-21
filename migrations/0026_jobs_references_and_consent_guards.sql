-- SPDX-License-Identifier: AGPL-3.0-only

-- What `jobs` references, said in a way the database can see — and the guards that make
-- `consent_log`'s table comment true rather than aspirational.
--
-- All of it is free TODAY and not later: `jobs` is empty, so every constraint below validates in
-- no time and every index builds on nothing. `0023` makes this same argument for the columns it
-- put in up front; this migration is the rest of that list, found by M1d's whole-branch review
-- before the first consumer exists.

-- ---------------------------------------------------------------------------------------------
-- 1. `jobs.contact_id` and `jobs.list_id`: real columns, composite foreign keys.
--
-- They lived in `payload` as JSON strings, where no foreign key reaches them. Three things were
-- broken by that and are fixed here at once:
--
--  * **Erasure did not reach `jobs`.** `delete from contacts` is how this product erases a
--    person; `0020` cascades their consent rows for the stated reason that "a row here carries
--    their IP and user agent". A `confirmation_email` row carries their address in clear AND a
--    working confirm token, had only `workspace_id` to be held by, and nothing scheduled to
--    remove it. An erased person's row survived, runnable, and a worker written from `0023`
--    would have mailed them. The contact key cascades, so erasure now reaches this table the way
--    it reaches every other.
--  * **A cross-tenant row was merely unlikely.** `0023` tells M2 to read `contact_id` out of the
--    payload and says nothing about scoping that lookup by the row's workspace. Both keys below
--    are composite and carry `workspace_id`, exactly as `form_lists` and `consent_log` do, so a
--    job of one workspace naming a contact of another cannot be written at all. It is the
--    database's answer and not a handler's.
--  * **The resend throttle had no index.** See section 2.
--
-- Both columns are nullable, because `kind` is deliberately open (`0023`: "M2 adds kinds without
-- a migration here") and a later kind need not be about one contact. A composite foreign key with
-- a NULL column is not checked (MATCH SIMPLE), which is the same mechanism `consent_log` relies
-- on for its workspace-wide rows. `jobs_confirmation_email_shape` is what stops the one kind that
-- does need them from being written without them.
--
-- `list_id` cascades rather than setting null: a confirmation email for a list that no longer
-- exists has nothing to confirm, and `form_lists` already takes deleting a list as routine
-- housekeeping that must not be blocked.
alter table jobs
  add column contact_id uuid,
  add column list_id uuid,
  add constraint jobs_contact_fk foreign key (workspace_id, contact_id)
    references contacts (workspace_id, id) on delete cascade,
  add constraint jobs_list_fk foreign key (workspace_id, list_id)
    references lists (workspace_id, id) on delete cascade,
  -- Not redundant with the primary key: it is what lets a later table take a composite foreign
  -- key onto a job carrying `workspace_id`, as `form_lists` does onto `forms` in `0023`.
  add constraint jobs_workspace_id_id_key unique (workspace_id, id),
  -- The payload spec is prose and will stay prose for kinds this milestone cannot see. What is
  -- enforceable is the shape of the one kind M1d writes, and that is enforced here rather than
  -- by a check on `kind` itself: a closed value list would make every kind M2 adds a migration
  -- on this table, which `0023` deliberately decided against.
  add constraint jobs_confirmation_email_shape
    check (kind <> 'confirmation_email' or (contact_id is not null and list_id is not null)),
  add constraint jobs_payload_is_object check (jsonb_typeof(payload) = 'object'),
  -- `max_attempts` is where a worker gives up. Zero or less means "give up before trying", which
  -- no caller means and the claim predicate cannot express.
  add constraint jobs_max_attempts_positive check (max_attempts > 0),
  add constraint jobs_attempts_non_negative check (attempts >= 0);

-- ---------------------------------------------------------------------------------------------
-- 2. The resend throttle's index.
--
-- `src/public/forms.rs` asks, on every submission of a public form: "was there already a
-- `confirmation_email` for this (contact, list) in the last fifteen minutes?" That question is
-- the only thing standing between an unauthenticated route and an open mailer (`0025` says so in
-- those words), and until now the only index serving it was `jobs_pkey`. The plan was a scan of
-- every row enqueued **instance-wide** in the window, with workspace, kind and payload applied as
-- filters afterwards — so one tenant's campaign send set the cost of another tenant's signup
-- form, on a route a stranger may call sixty times a minute per IP. The 50,000-row measurement
-- quoted in that file was taken on M1d's own trickle, where fifteen minutes holds nothing.
--
-- With the columns above the question is an equality probe into this index. The cost no longer
-- depends on what the rest of the instance is sending. Partial on the one kind that is throttled,
-- so rows of every other kind M2 adds stay out of it entirely.
create index jobs_confirmation_resend
  on jobs (workspace_id, contact_id, list_id, created_at desc)
  where kind = 'confirmation_email';

-- ---------------------------------------------------------------------------------------------
-- 3. The claim protocol, in one place, as a constraint on the table rather than a paragraph.
--
-- `0023` declared its claim query a REQUIREMENT ON THE CONSUMER and `0025` then invalidated it
-- (the predicate needs `completed_at is null`, and success is an update, not a delete). `0023`
-- cannot be edited — its checksum is taken — and it carries no pointer forward, so the file named
-- "forms and jobs", the one that creates this table, is the one that states the protocol wrongly.
-- This comment is the authority. It is on the table itself, which is where `\d+ jobs` looks.
comment on table jobs is
$doc$The work queue. M1d writes rows; M2 owns the worker that drains them.

THIS COMMENT IS THE CLAIM PROTOCOL AND IT SUPERSEDES THE QUERY IN 0023_forms_and_jobs.sql.
That query predates `completed_at` (0025) and omits it, so a worker written from 0023 alone
re-claims every completed row the moment its lease expires, for ever. 0023 cannot be amended:
its checksum has been taken. Where the two disagree, this wins.

Claim with a single statement of this shape:

    update jobs set attempts = attempts + 1, run_after = now() + <lease>
    where id in (select id from jobs
                 where run_after <= now() and failed_at is null and completed_at is null
                   and attempts < max_attempts
                 order by run_after, id
                 for update skip locked
                 limit $batch)
    returning *;

Load-bearing, none of it optional:
  * `for update skip locked` — without it two pollers claim the same row and both run it.
  * `run_after` advanced in the SAME statement. It is the LEASE as well as the schedule; a claim
    that leaves `run_after` in the past re-offers the row before the first worker has finished.
  * `completed_at is null` — 0025.
  * `attempts < max_attempts` — a worker that dies after incrementing `attempts` and before
    writing `failed_at` otherwise leaves a row that is retried past its limit for ever. The
    column was decoration until this predicate; `check (max_attempts > 0)` is now on the table.

On success `update jobs set completed_at = now() where id = $1`. Never delete: the resend
throttle has no state of its own and counts these rows (0025). A delete inside the retention
window is refused by `jobs_throttle_retention` rather than silently turning a public route into
an open mailer.

On giving up, set `failed_at` and `last_error` and leave the row (0023).

REFERENCES ARE COLUMNS, NOT PAYLOAD. `contact_id` and `list_id` are real columns with composite
foreign keys carrying `workspace_id`. Read them from the columns; a lookup must still be scoped
by the row's `workspace_id`, and the foreign keys guarantee that scoping finds something. The
`confirmation_email` payload is now `{"email": "…", "token": "<public confirm token>"}` — the
address the form was submitted with, and the already-issued token string, carried verbatim
beside the token's own fields (src/public/token.rs documents why nothing travels inside it).

A STALE ROW FAILS; IT IS NOT SENT. `Purpose::Confirm` has a 14-day TTL stamped at enqueue, rows
are kept indefinitely, and nothing re-issues the token. A `confirmation_email` row whose
`created_at` is older than that TTL carries a link that is already dead and would answer
`link-expired`: fail it (`failed_at`, `last_error`) rather than mailing it.$doc$;

-- The retention rule `0025` could only ask for. A worker that deletes on success makes the
-- throttle's question answer "no" every time, and `0025` says what happens next: the route
-- becomes a mailer anybody can aim at any address, and **nothing goes red**. This is what turns
-- that into a constraint violation at the moment the mistake is made. It is the direct analogue
-- of `consent_log_append_only()`, which this milestone wrote for exactly this class of rule.
--
-- `pg_trigger_depth() > 1` admits the referential actions — the contact cascade that erasure
-- needs, the list and workspace cascades — exactly as `0020`'s trigger admits its `set null`.
--
-- One day, not fifteen minutes: `0025` names a day as the safe floor for housekeeping, and a
-- window stated here in terms of the throttle's current value would silently stop covering it the
-- day `RESEND_AFTER_MINUTES` grows. A day is a superset of every plausible value of it.
-- shortcut: the window is a literal, not derived from RESEND_AFTER_MINUTES; raise it here if that
-- constant ever exceeds a day.
--
-- Deliberately NOT `enable always`, unlike the `consent_log` triggers below. This rule protects an
-- operational decision made on the primary, and a logical subscriber applies deletes the primary
-- has already judged — refusing them there would stall replication to protect a copy. `consent_log`
-- is the opposite case: it is evidence, and losing a row on any machine is the loss.
create function jobs_throttle_retention() returns trigger language plpgsql as $$
begin
  if pg_trigger_depth() > 1 then
    return old;
  end if;
  if old.created_at > now() - interval '1 day' then
    raise exception
      'jobs rows are kept for the resend throttle: a row younger than a day may not be deleted';
  end if;
  return old;
end $$;

create trigger jobs_throttle_retention before delete on jobs
  for each row execute function jobs_throttle_retention();

-- ---------------------------------------------------------------------------------------------
-- 4. `consent_log`: the comment says more than the triggers enforced.
--
-- "Rows are never updated, and are removed only when the contact they belong to is erased."
-- The first half was enforced. The second was prose: any `delete from consent_log`, and any
-- `truncate`, succeeded. The admin UI repeats the claim to operators in so many words
-- (`web/src/routes/contact-detail.tsx`). Both halves are enforced from here.
--
-- The technique is `0020`'s, unchanged: `pg_trigger_depth()` tells a direct statement (1) from a
-- referential action (2 or more), which is how the contact cascade — the one legitimate way a
-- row leaves this table — is admitted while a hand-written delete is refused.
create function consent_log_no_destroy() returns trigger language plpgsql as $$
begin
  -- TRUNCATE fires a statement-level trigger with no OLD row, and there is no referential action
  -- that truncates: nothing may truncate this table.
  if tg_op = 'TRUNCATE' then
    raise exception 'consent_log is append-only: it is never truncated';
  end if;
  if pg_trigger_depth() > 1 then
    return old;
  end if;
  raise exception
    'consent_log is append-only: a row is removed only by erasing the contact it belongs to';
end $$;

create trigger consent_log_no_delete before delete on consent_log
  for each row execute function consent_log_no_destroy();

create trigger consent_log_no_truncate before truncate on consent_log
  for each statement execute function consent_log_no_destroy();

-- `enable always`, including on `0020`'s existing update trigger.
--
-- An ordinary trigger does not fire when `session_replication_role = 'replica'`. That is one
-- `SET` away for anyone who could run the write in the first place, and it is the mode a logical
-- replica's apply worker and `pg_restore --disable-triggers` run in. For a table whose whole
-- purpose is to be evidence years later, a guarantee with a one-line bypass is not a guarantee.
--
-- The cost, stated so the operator who meets it knows what they are looking at: on a logical
-- subscriber, the row changes that a list deletion or a contact erasure produces on the primary
-- arrive as ordinary statements at depth 1 and are refused by these triggers, which stalls
-- replication with the messages above. That is deliberate. Turning the guarantee off on that
-- subscriber is then `alter table consent_log disable trigger …` — a recorded act with a
-- reviewable diff, rather than a property the table quietly lost by being replicated.
alter table consent_log enable always trigger consent_log_no_update;
alter table consent_log enable always trigger consent_log_no_delete;
alter table consent_log enable always trigger consent_log_no_truncate;

comment on table consent_log is
  'Append-only. Rows are never updated and are never deleted by any statement; they are removed '
  'only by the cascade from erasing the contact they belong to. Enforced by '
  'consent_log_no_update, consent_log_no_delete and consent_log_no_truncate, all of them '
  '`enable always`, so the guarantee holds under session_replication_role = replica too.';

-- ---------------------------------------------------------------------------------------------
-- 5. `forms.fields` is an array, and the whole module is written around that.
alter table forms add constraint forms_fields_is_array
  check (jsonb_typeof(fields) = 'array');

-- ---------------------------------------------------------------------------------------------
-- 6. A correction `0022` cannot carry, because it has been applied and its checksum is taken.
--
-- `0022`'s comment argues that `drop constraint` / `add ... not valid` / `validate constraint`
-- moves the validation scan out of the `ACCESS EXCLUSIVE` lock. It does not, in this repository.
-- `src/db.rs` runs `sqlx::migrate!`, and sqlx 0.9.0 (`sqlx-core-0.9.0/src/migrate/source.rs:234`)
-- treats a migration as non-transactional only when the file *starts with* `-- no-transaction`.
-- Every migration here opens with the SPDX header, so every one of them runs inside a
-- transaction, and Postgres holds the lock until it commits. The two-step form buys nothing here;
-- `create index concurrently` cannot run at all. Using either takes a migration whose first bytes
-- are `-- no-transaction`, which collides with the header convention `scripts/check-spdx.sh`
-- enforces — a carve-out nobody has made. `issues/24` carries the long version, corrected.
