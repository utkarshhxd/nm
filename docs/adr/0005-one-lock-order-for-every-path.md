<!-- SPDX-License-Identifier: AGPL-3.0-only -->

# 0005 — One lock order, written down, for every path that takes more than one lock

Status: accepted, 2026-11-21. Supersedes the per-pair ordering arguments scattered through
`public::unsubscribe`, `public::forms` and `suppressions`, which remain as local explanations of
*why* a particular line is where it is but no longer as the definition.

## The problem this exists to stop

M1d shipped four ABBA deadlocks in three review rounds. Each round fixed the pair it was told
about and wrote a paragraph arguing that pair was now safe; each next round found another pair.
They were not four unlucky bugs. They were one missing artifact.

Every one of those paragraphs reasoned about **two** paths. There are nine paths that take more
than one of these locks, so there are thirty-six pairs, and each new writer re-derives its own
order from whichever neighbouring comment it happened to read. That is a defect generator, not a
defect: the next person to add a `consent_log` insert after a `list_memberships` update introduces
the same cycle again, and no test sees it, because every one of these is a race that the full
suite — green through all four — cannot reach.

## The order

Every transaction that takes more than one of these takes them in this order, and never out of it:

```
1. the address advisory lock      suppressions::lock_address / lock_address_for
2. lists rows                     lists::lock_lists (for key share)
2a. form_lists rows               (below lists; unordered against 3-6)
3. suppressions rows
4. contacts rows                  contacts::lock_contact (for key share)
5. list_memberships rows
6. consent_log rows
```

`form_lists` is `2a` rather than a number of its own because only one thing is forced about it: it
sits **below `lists`**, since `delete_list` holds the list row and then cascades into it.

**It is unordered against 3-6, and the reason is not the one first written here.** That said "no
path takes it together with any of them", which is false: `delete_list` takes all of them in one
transaction. The cascade on `lists` fires in constraint-OID order, read off the catalog —
`list_memberships`, `imports`, `consent_log`, `form_lists`, `jobs`, `suppressions` — with
`suppressions` additionally hand-pre-locked before the delete. The real reason the position is free
is that `delete_list` is the **only** path taking them together, so there is no second path to
invert against. A right ruling on a false reason is exactly what let `form_lists` through in the
first place, which is why this paragraph is longer than the ruling needs.

**And `set_lists` does not obey "below `lists`" for every row it takes.** It holds `lists` only for
the ids it keeps or adds, then deletes the link rows of the lists it **drops** without ever holding
those `lists` rows. What makes that safe is not the ordering but `form_lists`' primary key
`(form_id, list_id)`: `set_lists(F)` touches `{F} × lists(F)` and `delete_list(L)` touches
`forms(L) × {L}`, so the two intersect in **at most one row**, and a one-row intersection cannot
host an ABBA — whoever gets that row finishes. Both arrival orders were raced and both commit. The
same argument clears `delete_form` against `delete_list`.

**Whether that argument transfers is the thing to check for the next table of this shape**, and
there is one coming: the `PUT /contacts/{id}/tags` tripwire below has the identical
delete-and-reinsert shape over `contact_tags`, which also has a composite primary key. If the
intersection is again at most one row, the same reasoning applies; if a writer ever deletes by one
half of the key alone, it does not.

Within a class, by `id` ascending. `lists::lock_lists` sorts for you.

**`contacts` was missing until it had produced three more cycles**, and the shape of that miss is
worth more than the fix. Every `list_memberships` and `consent_log` row carries a foreign key to
`contacts` exactly as it does to `lists`, so every writer took the contact row *last*, at the end
of its statement, while `delete_contact`, `selection::delete` and `erase_contact` take it **first**
and then cascade into those same membership rows. It is the `lists` inversion again, on a class
nobody had named — and it was invisible because the tests that hold the `lists` class all wedge the
contact row with a transaction that never asks for anything back, which cannot cycle. The
antagonist was standing in the fixture.

**Why this order and not another.** Four of the six positions are forced, and the other two
follow:

- `lists` cannot move below `suppressions`, because the rename trigger
  (`0027_list_scope_suppressions.sql`) runs `lists` → `suppressions` inside one statement and
  nothing can get between them.
- `lists` cannot move below `list_memberships` either, because `delete_list` locks the list row it
  is deleting before anything cascades.
- `suppressions` must sit above `list_memberships`, because `contacts::routes::erase_contact` has
  to write the tombstone before it deletes the contact — the tombstone is what makes a concurrent
  create of that address impossible, and the contact delete is what cascades into the memberships.
- `contacts` must sit **below `suppressions`**, because `erase_contact` writes its tombstone and
  only then deletes the contact; and **above `list_memberships`**, because every path that deletes
  a contact takes the row and then cascades into the memberships. Nothing forces it against
  `lists`: no path takes both exclusively, so it sits after `lists` to keep one sequence rather
  than two.
- `consent_log` goes last because it is written last everywhere: it records what the membership
  write did, so it cannot precede it.
- The advisory lock goes first because it is the only one of the five that is not a row, it is
  what serialises two transactions *about one address* before either reads anything, and a path
  that took it later would already have branched on data it did not hold.

**The two places the natural order is the wrong way round, and what was done about them.** Every
writer of `list_memberships` or `consent_log` takes a `lists` row *implicitly*, through
`list_memberships_workspace_id_list_id_fkey` or `consent_log_workspace_id_list_id_fkey`, and a
foreign key check runs at the **end** of the statement that triggered it. So the natural order of
every membership writer is `list_memberships` → `lists`, which is backwards. Each such path now
takes the `lists` rows it is about to touch explicitly, first. The same applies to `delete_list`'s
cascade, whose trigger firing order reaches `suppressions` last: it now takes those rows up front.

## What this makes impossible

A path that obeys this cannot deadlock with any other path that obeys it, whatever either does —
not "cannot deadlock with the paths somebody checked". That is the difference between a rule and
thirty-six arguments, and it is why this is a document rather than another comment.

It does **not** make lock-free. Paths still wait for each other, and
`suppressions::unsubscribe_list` carries a note about the one wait that is long (a list deletion
holds its row across its whole cascade).

## Every path that takes more than one, and where it takes them

| path | order it takes |
|---|---|
| `public::unsubscribe::save` (preference centre) | A, L (every candidate list), S, contacts, M, C |
| `public::unsubscribe::one_click` | A, L (the contact's lists), S, contacts, M, C |
| `public::forms::apply` | A, L (the form's lists), S (read), contacts, M, C |
| `public::confirm` | L, contacts, M, C |
| `contacts::routes::erase_contact` | A, S, contacts, M, C (cascade). Takes no `lists` row. |
| `contacts::routes::delete_contact` | contacts, M, C (cascade). No tombstone, so no S. |
| `contacts::selection::delete` | contacts, M, C (cascade), over a selection |
| `contacts::import::staging::upsert_from_staging` | L (the job's list and every list the file names), contacts (the upsert, exclusively), M, C |
| `contacts::selection::subscribe` / `unsubscribe` | L, contacts (the `sel` CTE), M, C |
| `lists::add_member` / `unsubscribe_member` | L, contacts, M, C |
| `lists::routes::delete_list` | L, S, M, C (cascade) |
| `lists::routes::update_list` (rename) | L, S (trigger) |
| `forms::routes::create_form` / `update_form` | the `forms` row, L (`set_lists`), `form_lists` |
| `contacts::import::routes::start_import` | L (held, not merely read), then `imports` |
| `contacts::selection::tag` | the `tags` row (`for key share`), `contact_tags` |
| `suppressions::routes::create_suppression` / `lift_suppression` | S, C (through `record_workspace_optout`) |

**Every row below has been read against the code it names.** `relay::bucket`, `sending_domains`
and `worker` all shipped on this branch, so the caveat this section carried through M2a — that
most of these paths did not exist yet — is gone, and with it the licence to treat a row as a
guess.

**What replaces it is a standing obligation, because a snapshot goes stale and this section has
done so twice.** The table is meant to list *every* path that writes one of the enumerated tables,
so: **a change that adds a writer adds its row, and a change to what a writer locks changes its
row — in that same change, not afterwards.** A row with no writer and a writer with no row are
both defects in this document, and the second is the one that produces a deadlock. If a row and
the code disagree, the code is what runs: fix the row, and say in the commit which one you read.

| path (M2a) | order it takes |
|---|---|
| `relay::routes::create_relay` / `update_relay` | the `relays` row alone. A rename writes `name`, which `relays_workspace_name_key` makes a key column, so it takes `for update` and **not** `for no key update` |
| `relay::routes::delete_relay` | the `relays` row, then `relay_buckets` (cascade) |
| `relay::bucket::take` | `relay_buckets`, then `relays` (`for key share`) — **on its insert arm only**. The update arm writes `tokens` and `refilled_at`, so no referential trigger fires and it takes nothing on `relays` |
| `sending_domains::routes::*` and the DNS checker's write-back | the `sending_domains` row alone; nothing references it |
| `worker::claim` / `complete` / `fail` / `defer` | `jobs` rows alone, `for update skip locked`. `jobs` is a child of `lists` and `contacts` and is a parent of nothing |
| `worker::release` | the `jobs` row alone, by primary key; the update's own row lock and no clause beyond it. It undoes the claim's `attempts` increment and pushes `run_after` for a job the tick chose not to start; it names no parent, and `jobs` is a parent of nothing |
| `worker::confirmation::handle` | the `jobs` rows above for what it records, and **`relay::bucket::take`'s order** on the failover arm, where it takes a token for a relay the tick did not pay for. It reads `suppressions`, `sending_domains` and `relays` and writes none of them. Since M2c R6 its submission is `worker::deliver::submit`, which on a deferral (`455`, or no connection) also runs `relay::bucket::drain`: one statement, the update arm alone |
| `relay::bucket::drain` (M2c R6) | the `relay_buckets` row alone, **the update arm only**: it writes `tokens` and `refilled_at`, neither a key column, so no referential trigger fires and it takes nothing on `relays`. One statement on the pool, never a transaction. A relay with no bucket row matches nothing. `tests/lock_order.rs` (`a_bucket_drain_takes_no_lock_on_its_relay`) holds it against a wedged `relays` row, with a first take's insert arm on a second wedged relay as the control that shows the wedge blocks |
| `worker::test_send::handle` (M2c R6) | as `worker::confirmation::handle`: the `jobs` row it records, and `bucket::take`'s order on the failover arm, and `drain`'s on a deferral. It reads `jobs.campaign_id`, `suppressions`, `sending_domains` and `relays` and writes none of them |

**M2b's writers landed with R7**, which is the task that turned the ruling below from a claim
about statement shapes into a claim about code. Each row is one locking statement; the reads
beside them (`custom_fields`, `brand_kits`, the row being patched) are plain selects with no `for`
clause and lock nothing.

| path (M2b) | order it takes |
|---|---|
| `template::routes::create_template` | the new `templates` row, then `workspaces` (`for key share`) through `templates_workspace_id_fkey`'s referential trigger, at the end of that one statement. Reads `custom_fields` and `brand_kits` first, both unlocked |
| `template::routes::update_template` | the `templates` row alone. The `set` names `name`, `tree`, `source`, `compiled` and `updated_at` and never `workspace_id`, so no referential trigger fires and it takes nothing on `workspaces`. It reads the row first with a plain `select` — **no `for update`** — because it only needs the kind and whatever the patch leaves alone |
| `template::routes::delete_template` | the `templates` row alone. The child side of a foreign key has no delete-time check, and nothing references `templates` |
| `template::routes::preview_template` | **nothing.** It writes no row: an unsaved document is compiled in memory; a stored rendition made stale by a brand-kit or renderer change (`Stale`, `MarkerFormat`) is recompiled in memory too (M2c R2, issue 49) and the result is never written back; one whose fields drifted, or that cannot be read, is refused (`409 template-stale`) |
| `brand::routes::save_brand_kit` | the `brand_kits` row, and `workspaces` (`for key share`) **on the upsert's insert arm only**. The update arm's `set` names `logo_url`, `palette`, `font_stack`, `footer` and `updated_at` — never `workspace_id` — so it is the same short-circuit as `relay_buckets`' update arm |

Each is `one statement on one row by primary key` plus the audit write, which takes nothing beyond
a `RowExclusiveLock` on `audit_log` (the `delete_relay` reasoning). So each stays a terminal node
in the wait-for graph, which is what the ruling below rests on. `templates` and `brand_kits` are
still not lock classes and still not parents.

**M2c's campaign handlers (R3).** The order is `messages`, then `campaigns`, then `jobs`. None of
these handlers touches `messages` (a draft has none), so each is `campaigns` and then, where a job
is queued, `jobs`. Every one authorises, validates and runs `mrml` BEFORE its transaction takes a
row; the reads beside the writes (`lists`, `segments`, `sending_domains`, `templates`,
`custom_fields`, `brand_kits`) are plain selects and lock nothing. The worker's paths (launch,
claim, flush, sweep) have rows of their own in the second table below, and take their rows in
this same order.

| path (M2c R3) | order it takes |
|---|---|
| `campaign::routes::create_campaign` | the new `campaigns` row, then `workspaces` (`for key share`) through `campaigns_workspace_id_fkey`, at the end of that one statement |
| `campaign::routes::update_campaign` | the `campaigns` row alone. The `set` never names `workspace_id`, so no referential trigger fires; `where state = 'draft'` makes a lost race a `409`. The read before it is a plain `select`, no `for update` |
| `campaign::routes::pause_campaign` / `unschedule_campaign` / `cancel_campaign` | the `campaigns` row alone, by workspace and primary key and a state guard, held to commit through the audit insert and the plain read-back that follow it in the same transaction (R7 added that clause: the row said "alone" and the code is three statements). `audit_log` has no foreign keys, so the hold asks for nothing and the path stays a leaf. No key column is written |
| `campaign::routes::resume_campaign` | the `campaigns` row alone. A re-freeze writes `frozen` and `frozen_at` in the same statement, both after `prepare` has run outside any transaction; no key column is written. Then `queue::finish_if_done` on the same row (R6: a campaign whose last messages were recorded while it was paused finishes here). When the audience is not fully resolved (`resolved = false`) it then repoints `launch_job_id` (`campaigns` again) and inserts a `campaign_launch` job (`jobs`, whose foreign key re-takes the campaign's `for key share`): `campaigns`, then `jobs`, as `choose_winner` does |
| `campaign::routes::delete_campaign` | the `campaigns` row, then `jobs` (cascade, a draft's `campaign_test` jobs). `where state = 'draft'` cannot match a campaign with messages (`messages -> campaigns` is `no action`) or one that has launched, so this is the only deleter and `campaigns` stays out of the lock classes. A deleted draft's launch job (it may have been scheduled and unscheduled) goes the same way |
| `campaign::routes::schedule_campaign` | the `campaigns` row (update, guarded by `state = 'draft'`), **then** the new `jobs` row, whose foreign key takes `for key share` on that same campaign and on `workspaces` at the end of its statement. `campaigns` before `jobs`, and the campaign row is already held, so the key share is a re-take, not a second lock |
| `campaign::routes::choose_winner` | the same two, in the same order: the `campaigns` update (`state in ('sending','paused') and phase = 1 and resolved and ab_test_percent < 100`), then the phase-2 `jobs` insert |
| `campaign::routes::test_campaign` | `jobs` inserts only, one per address, each taking `for key share` on the campaign and `workspaces` at the end of its statement. It writes no `campaigns` row |
| `campaign::routes::preview_campaign` / `estimate_campaign` | **nothing.** The estimate reads `mailable_list_members` under `set local statement_timeout` in a transaction it rolls back |

**M2c's order on its own tables is `messages`, then `campaigns`, then `jobs`** (spec §4.11). The
message queue (R4) writes these. **No UPDATE below writes `campaign_id` or `workspace_id`**, so no
referential trigger fires on one and none takes `for key share` on `campaigns`; an update of
`campaigns` counters or state is `for no key update`, which a child insert's `for key share` does
not wait on. The one statement below that does write them is an INSERT, the launch batch's into
`messages`, and its row says so. (This paragraph said "no statement" until R7 read it against the
launch row beside it and found the two disagreeing; the row was right.)

| path (M2c) | order it takes |
|---|---|
| `campaign::queue::claim` | `messages` rows alone, `for update skip locked`, one statement. Writes `attempts`, `run_after` and `claim_token`. Its join on `campaigns` (`state = 'sending'`) and its `exists` probe on `messages` (which campaigns have a message due, the ones the batch is split over) are plain reads with no locking clause |
| `campaign::queue::mark_submitting` | the `messages` row alone, by primary key and `claim_token`. Writes `status` and `run_after` |
| `campaign::queue::release` | `messages` rows alone, by primary key and `claim_token`, `status = 'queued'` only. Writes `attempts`, `run_after`, `error`, `claim_token` |
| `campaign::queue::record` | `messages` (one `update … from unnest`, matched on `claim_token`), then `campaigns` by primary key, one row per statement in ascending id order: the counters, then `finish_if_done` on the row it already holds. One transaction. `tests/lock_order.rs` races it against `system_pause` and the sweep (`an_outcome_flush_and_a_pause_do_not_deadlock`, `an_outcome_flush_and_a_sweep_of_the_same_message_count_it_once`) |
| `campaign::queue::record` (M2d) | `A` once per bounced address in ascending lock-key order, and the `suppressions` insert for each (`suppressions::record_bounces`), then `messages`, then `campaigns` as before. One transaction. A flush with no bounce takes no `A` and no `S`. Raced by `two_flushes_bouncing_the_same_addresses_in_opposite_orders_both_commit` and `a_bounce_flush_and_a_one_click_for_the_same_address_both_commit` |
| `campaign::routes::report_campaign` (M2d) | **nothing.** Three plain reads (`messages`, `message_clicks`, `consent_log`) and a fourth of `campaigns.frozen` for the link table, after the `load` that is the 404; no transaction |
| `public::track::open` (M2d) | the `messages` row alone, one statement on the pool. Writes `opens` and `opened_at`, no key column. A leaf |
| `public::track::click` (M2d) | one statement on the pool: the `messages` row (`clicks`, `clicked_at`, `opened_at`, no key column), then the `message_clicks` row, which has no foreign key and which nothing references. The lookup before it is a plain read. A leaf |
| `campaign::queue::sweep` | `messages` in two statements (expired `submitting` rows to `unknown`, exhausted `queued` rows to `failed`), each `for update skip locked` so it never waits on a row a flush is recording; then `campaigns` as `record`. One transaction. Held by the same flush race, and by `two_sweeps_at_once_fail_each_exhausted_message_once` for the second statement |
| `campaign::launch::sweep` (R6) | for each launch job whose worker died on its last attempt, the last-attempt give-up below: `campaigns` (`failed`, or a system pause, fenced on `launch_job_id`), then `jobs` (`failed_at`), one transaction. The select that finds them is a plain read |
| `campaign::send::system_pause` (R6) | the `campaigns` row alone, by primary key and a state guard (`sending`, `paused`). Writes `state`, `pause_reason`, `updated_at`; no key column. Raced against a flush in both orders (`an_outcome_flush_and_a_pause_do_not_deadlock`), the one pair that really runs together: a tick that pauses a campaign while its flusher is recording |
| `campaign::send::CampaignWorker::tick` (R6) | **no transaction of its own.** One statement at a time, each in the order its own row gives: `queue::sweep`, `queue::claim`, `queue::mark_submitting`, `queue::release` and `queue::record` (the flusher), `worker::debit` and `deliver::submit` (`bucket::take`), `bucket::drain`, `system_pause`. It reads `campaigns` (`frozen`), `custom_fields`, `sending_domains`, `relays` and `mailable_list_members` with plain selects. Nothing is held across an SMTP session |
| `campaign::launch` (a batch of `resolve_batch`, and each loop of `handle`) | `messages` (the insert-select, whose foreign key takes `for key share` on the campaign at the end of that statement), then `campaigns` by primary key and `launch_job_id` (cursor and `total`; no key column). The last batch of a slice adds, in the same transaction: `campaigns` (`resolved`, then `finish_if_done`, or `launch_job_id` for the hand-off), **then** `jobs` (insert the successor, complete this job). `messages`, `campaigns`, `jobs`. The freeze (`scheduled -> sending`) and a refusal (`failed`, or a system pause) are `campaigns` then `jobs` and touch no `messages`; so is the last-attempt give-up, which ends the campaign and fails the job in one transaction. The job's own `payload` (the phase's segment snapshot) is written in a statement of its own, before any transaction opens. `prepare` and its `mrml` run before any of these transactions open. `tests/lock_order.rs` races a batch against an operator's pause in both orders (`a_launch_batch_and_a_pause_do_not_deadlock`); the hand-off, the freeze and the refusals have no race test of their own, and are `campaigns` then `jobs` for the reason given under "M2c: why `campaigns` is not a class" below: a `jobs` insert takes only `for key share` on the campaign, which no campaign update waits on |

## The rule for new code

If you write `list_memberships` or `consent_log`, call `lists::lock_lists` with the list ids you
are about to name and `contacts::lock_contact` with the contact, in that order, first. If you also
touch `suppressions`, do that between the two. If the write is about one address, take
`suppressions::lock_address_for` before any of it.

**Use what `lock_lists` returns.** A caller that inserts must narrow itself to the ids that came
back; one that only updates may discard it, because a vanished list matches nothing and it answers
`404` anyway. Getting that wrong is a foreign key violation reported as a `500`.

If a path cannot obey this, that is worth saying out loud rather than working around — put it in
this table with the reason, and expect the next deadlock to be there.

## The method, and the closed enumeration it produced

This section used to be a short list of tables somebody had thought about, and it was wrong twice:
it omitted `contacts`, and then it said in so many words that `form_lists` was covered when it was
not. Both were right answers reached by the wrong route — "which tables feel relevant" — so here is
the route that is not guesswork.

**A class exists wherever one path writes a child table directly and another path reaches the same
rows through a parent's `on delete` action.** The cascading path takes the parent first; the direct
writer takes the child first and the parent last, through its own foreign key at the end of its
statement. That is the inversion, every time. So the enumeration is over **foreign key parents**,
because that is what bounds the set of cascades.

**This is the query that produces the set, and the next milestone runs this one rather than
inventing its own.** It is derived from the catalog, not from the migrations and not from this
document — reading the document is how it was wrong twice.

```sql
select confrelid::regclass::text as parent,
       count(*) as fks,
       string_agg(distinct conrelid::regclass::text || ' (' || confdeltype::text || ')', ', ')
         as children
from pg_constraint
where contype = 'f' and connamespace = 'public'::regnamespace
group by 1
order by 1;
```

`confdeltype` is the `on delete` action and is the half that matters: `c` cascades, `n` nulls the
column, `a` and `r` refuse. Against a database migrated from nothing:

```
bash scripts/dev.sh bash -c 'export DATABASE_URL=postgres://nm:nm@postgres:5432/nm_enum; \
  sqlx database create && sqlx migrate run --source ./migrations'
docker compose -f docker-compose.dev.yml -p nm exec -T postgres psql -U nm -d nm_enum -c "<query>"
```

`psql` is in the `postgres` service, not in the dev container, which is why the second line does
not go through `scripts/dev.sh`. As of `0033` it answers — and it still did when M2c R7 re-ran it
against a database migrated from nothing to `0034`, which adds a column and no edge, and when M2d
re-ran it against a database migrated from nothing to `0035`: unchanged, because `message_clicks`
has no foreign key (spec M2d §4.7.2; `pg_constraint` shows none to or from it):

```
      parent         | fks | children
---------------------+-----+---------------------------------------------------------------
 api_keys            |   1 | selection_snapshots (n)
 campaigns           |   2 | jobs (c), messages (a)
 contacts            |   4 | consent_log (c), contact_tags (c), jobs (c), list_memberships (c)
 forms               |   1 | form_lists (c)
 imports             |   1 | import_errors (c)
 lists               |   6 | consent_log (n), form_lists (c), imports (n), jobs (c),
                     |     | list_memberships (c), suppressions (n)
 relays              |   1 | relay_buckets (c)
 selection_snapshots |   1 | selection_snapshot_members (c)
 tags                |   1 | contact_tags (c)
 users               |  12 | api_keys (n), auth_challenges (c), imports (n), invites (n),
                     |     | memberships (c), passkeys (c), recovery_codes (c), segments (n),
                     |     | selection_snapshots (n), sessions (c), suppressions (n)
 workspaces          |  18 | api_keys (c), brand_kits (c), campaigns (c), contacts (c),
                     |     | custom_fields (c), forms (c), imports (c), invites (c), jobs (c),
                     |     | lists (c), memberships (c), relays (c), segments (c),
                     |     | selection_snapshots (c), sending_domains (c), suppressions (c),
                     |     | tags (c), templates (c)
```

**Eleven parents; ten are ruled on and the eleventh, `campaigns`, is ruled in M2c R3 and re-ruled,
with the code read against it, in R7 (its own section below).** `0028` added
exactly one, `relays`; `0032` added **none** — it added two edges, both out of `workspaces`, and
two tables nothing references. `0033` added `campaigns`, with two children (`messages (a)`,
`jobs (c)`) and a third edge out of `workspaces`; `messages` is a table nothing references, and
`jobs` already was one. The ruling rests on `delete_campaign`'s statement (`where state = 'draft'`,
which cannot match a campaign that has messages or one that has launched; a draft may still own an orphaned launch job from a schedule that was undone, and the cascade removes it), and that statement now
exists: see the M2c table below.
`sending_domains`, `templates` and `brand_kits` are listed below the ten although they are not
parents, because each was a table asked about and the answer is load-bearing.
`tests/lock_order.rs` asserts that every **edge** of this graph —
each `(parent, child, on delete action)` triple, not merely the set of parents — is unchanged, so
a migration that adds a twelfth parent, a second child to an existing one, or that turns a
`cascade` into a `set null`, fails there rather than being noticed by somebody.

| parent | ruling | why |
|---|---|---|
| `lists` | **class 2** | cascades into `form_lists`, `list_memberships`, `consent_log`, `suppressions`, `jobs`, `imports`; several paths write those directly |
| `contacts` | **class 4** | cascades into `list_memberships`, `consent_log`, `contact_tags`, `jobs`; every membership writer takes it last through a foreign key |
| `forms` | not a class | its only child is `form_lists`, and every exclusive taker of a `forms` row takes that row first |
| `tags` | not a class **today** — see the tripwire below | its only child is `contact_tags`, and every writer either inserts `on conflict do nothing` (which retains no lock on a conflicting row) or deletes and asks for nothing back |
| `imports` | not a class | its only child is `import_errors`, written only by the run already holding the `imports` row |
| `selection_snapshots` | not a class | its only child's rows are inserted once, by the transaction that minted the parent |
| `api_keys` | not a class | one child, `selection_snapshots`, and no exclusive taker exists (below) |
| `campaigns` | **not a class** — ruled in M2c R3, re-run and held by tests in R7 | two children, `messages` (`no action`) and `jobs` (`cascade`); a new parent as of `0033`. Its only deleter is `delete_campaign` with `where state = 'draft'`, which cannot match a campaign that has messages or one that has launched (a draft may keep an orphaned launch job from a schedule that was undone; the cascade removes it), and every child insert takes `for key share` once at the end of one statement. See the M2c table below |
| `workspaces` | not a class | eighteen children since `0033` — it was seventeen after `0032`, fifteen before it, and thirteen before `0028` — and no exclusive taker (below) |
| `users` | not a class | twelve foreign keys over eleven children (`suppressions` twice, created-by and lifted-by) and no exclusive taker (below) |
| `relays` | **not a class** — the evidence is four tests, below | its only child is `relay_buckets`, and the one path that writes that child directly is `bucket::take`, whose two arms are ruled separately: the insert arm inverts but cannot cycle, and the update arm takes nothing on the parent at all |
| `sending_domains` | not a parent at all | nothing references it, so no cascade reaches rows through it and the inversion this enumeration is over has nowhere to form. It is a child of `workspaces` and nothing else |
| `templates` | not a parent at all — **today**, see the tripwire in the M2b section | `pg_constraint` shows one foreign key on it, `templates_workspace_id_fkey` to `workspaces` (`c`), and none pointing at it. It carries `templates_workspace_id_id_key` precisely so that a later table can, which is the day this row is re-ruled |
| `brand_kits` | not a parent at all, and cannot become one cheaply | one foreign key, to `workspaces` (`c`), and its primary key *is* `workspace_id`, so the only thing that could reference it is a table that already references `workspaces` for the same row |

**Two measured properties this rests on, worth re-checking rather than re-deriving.** The "no
cycle" result for `selection::tag` depends on Postgres granting a compatible `FOR KEY SHARE` past
an already-queued `FOR UPDATE` waiter — measured on 16.15 as granted in 1.1 ms while the exclusive
waiter went on waiting 2.4 s. That is an implementation property of `heap_lock_tuple`, **not a
documented guarantee**, so it is worth re-measuring on a major-version upgrade. And `start_import`'s
`lock_lists` does **not** hold the list for the length of a run: the `update imports … list_id`
below it already took that same row through its own foreign key and held it to commit, so the
change moves the acquisition point by microseconds and closes a window, rather than extending a
hold.

**"No exclusive taker" is the load-bearing phrase for `api_keys`, `workspaces` and `users`**, and it rests on a Postgres
rule rather than on a count: an `UPDATE` that changes no column of a candidate key takes
`FOR NO KEY UPDATE`, which does **not** conflict with the `FOR KEY SHARE` a foreign key check
takes. Every update to these three touches non-key columns only, and `src/` contains no
`delete from workspaces`, `delete from users` or `delete from api_keys`. If a delete appears, or an
update starts changing a key column, they become classes the same day.

`audit_log` is outside all of this: it has no foreign keys, nothing deletes from it, and it is
append-only.

### Why `relays` is not a class, and the four tests that are the evidence

**Half the code this section rules on still does not exist.** `src/relay/` now holds the CRUD
handlers, and `delete_relay` has been checked against this section (see the symmetric gap below).
But `take` has not been written: what is described below is the plan's wording for Task 8,
reconstructed as the `DEBIT` constant in `tests/lock_order.rs`. The same caveat is on the path
table above, repeated here because this is the section somebody reads in a hurry. **If Task 8
writes `take` differently, the tripwire list at the end of this section is the part to re-check
first.**

`relays` has the shape of a class and was expected to be one. One child, `on delete cascade`, a
deleting path that takes the parent first, and a direct child writer — `bucket::take`, run on every
worker tick — that takes the child first and the parent last through its own foreign key. That is
the inversion in the paragraph above this table, word for word. It is still not a class, and the
reason is in the statement rather than in the shape, so it is written down here with what measured
it. A ruling with no evidence is what this section exists to prevent.

`take` is one `insert … on conflict (relay_id) do update`, and **its two arms take different
locks**:

- **The insert arm** — a relay's first tick, when no bucket row exists — writes the foreign key
  columns, so the referential check takes `for key share` on the `relays` row at the end of the
  statement. Child first, parent last: the inversion.
- **The update arm** — every tick after the first — writes `tokens` and `refilled_at` only.
  Neither is part of the foreign key, so Postgres fires no referential trigger and the statement
  takes **nothing at all** on `relays`. The path that would invert simply does not take the second
  lock. `a_debit_of_an_existing_bucket_takes_no_lock_on_its_relay` holds the `relays` row
  `for update` and shows the debit completing anyway, with a control on the same wedge proving the
  wedge is real: the `for key share` the *insert* arm would take does block.

#### Why the insert arm cannot be half of a cycle, and the two orders that were raced

The first version of this section gave the right ruling for a reason that is not in the causal
chain — "a right ruling on a false reason", which is what this document says above let `form_lists`
through in the first place. It said the deletion
"has nothing to wait on" because the debit's bucket row is uncommitted and invisible to the
cascade. That mechanism is real, but **both arrival orders were then raced against a live `0028`
database and neither of them exercises it**:

| order | what happens | outcome |
|---|---|---|
| **A** — the deletion's statement completes first | `delete from relays` locks the parent **and** runs its cascade scan inside one statement, and it did both before the debit inserted anything. The cascade found no bucket row because none existed yet, not because it could not see one. The debit then queues on the parent, the deletion commits | **`23503`** on `relay_buckets_relay_fk` |
| **B** — the debit's statement completes first | the debit holds its new bucket row *and* the `for key share` its insert arm took. The deletion blocks on the **parent** row — measured at 2.51 s in the review's harness, which is that harness's hold time and not a property of anything — then the debit commits, the deletion takes the relay, and its cascade now sees a committed child row and takes that too | **no error at all**, both commit |

So the honest statement of the ruling is not about invisibility. It is this: **once its one
statement is done, the insert arm asks for nothing further.** Note what that does *not* say: the
deletion certainly can and does queue behind it — on the parent row, for the 2.51 s of order B
above. Queuing behind the debit is fine. What matters is that the debit is not itself queued behind
anything, so the wait is one-directional. A cycle needs every node to be both a holder and a
waiter. In order B the debit is a
terminal node in the wait-for graph — it has the parent lock and wants nothing — so the deletion
waits and that is the end of it. In order A the debit is the only waiter. Neither order has two
waiters, so neither can cycle.

The invisibility argument survives, but only in the job it is actually needed for: ruling out the
third interleaving, the one neither test can stage, where the deletion has locked the parent and is
mid-cascade while the debit has inserted and is asking for the parent. That one would cycle if the
cascade could see the debit's row. It cannot, so it does not. It is written here rather than in a
test comment because **no test measured it**, and a comment claiming otherwise is how this went
wrong the first time.

`a_relay_deletion_and_a_first_debit_do_not_deadlock` is order A and asserts the exact SQLSTATE and
the constraint name; `a_first_debit_then_a_relay_deletion_both_commit` is order B and asserts that
the deletion blocks, that both commit, and that the bucket row went with its relay. Both use
`settle`, so both fail rather than pass if the interleaving never forms. **One order is not an
answer here** — they differ in outcome *and* in mechanism, and a reader given only one of them
draws the wrong conclusion about why.

**The `23503` of order A is a real error path and nothing owns it today.** `take`'s signature is
`sqlx::Result<bool>`, so the violation reaches its caller as an `Err`, and the caller is Task 11's
tick. A tick that treats it as fatal turns an operator deleting a relay into a failed batch; it
should read as *this relay is gone* and fall to the next in failover order, which is the behaviour
`enabled_relays` already orders for. **Task 11 owns this**, and its plan section says so in those
terms — recorded in both places because a ruling whose only carrier is this document is a ruling
that depends on someone thinking to read it.

**The symmetric gap, and what checking it actually showed.** Tripwire 3 fires when the *debit* side
grows past one statement; the deletion side needs the same test, because the argument above rests
on `delete_relay` not being a second waiter either.

`delete_relay` now exists and **is three statements** — `authorize`, the delete, then the audit
write — so the obvious reading is that the ruling has already broken. It has not, and the reason is
worth stating precisely, because the obvious reading was mine and it was wrong:

- `authorize` is a bare `select` on `memberships`. A plain read takes no row lock.
- `record` inserts into `audit_log`, and **`audit_log` has no foreign keys at all** (`0001`). An
  insert into a table with no parents takes `RowExclusiveLock` on that table and asks for nothing
  from any other. Verified against `pg_constraint`, which returns only `audit_log_pkey`, and
  measured: the whole three-statement path holds nothing on `workspaces`, `users` or `api_keys`,
  while an `insert into relays` in the same probe does take `RowShare` on `workspaces` — the
  control that shows the probe would have seen such a lock had one been taken.

So the deletion is still terminal. **The right test is not "is it one statement" but "does it take
a row lock on, or wait on, anything else"** — and `delete_relay` does neither. Every audited writer
in this codebase has exactly this shape, so this reasoning covers all of them at once rather than
relay-by-relay, and `tests/lock_order.rs`'s graph-closure test fails on any migration that adds a
foreign key to `audit_log`. The gap is therefore mechanically watched, not merely written down.

Last, the `40P01` half from the other side: can a deletion wait forever on a bucket row a debit
holds? It cannot, for the same reason — the holder is asking for nothing back, which is the
argument this document already makes for `tags`' unlocked writers.
`a_relay_deletion_waits_for_a_debit_rather_than_deadlocking` runs exactly that, with a *committed*
bucket row held mid-transaction and a relay deletion queued behind the cascade, and the deletion
waits and then commits.

**Measured, and worth not re-deriving: an `UPDATE` that writes a foreign key column back to the
value it already held takes no lock on the parent either.** Postgres's referential trigger
short-circuits on unchanged keys, which was found by trying to break the update-arm test with
`set workspace_id = $2` and watching it stay green. So the ruling survives a future `take` that
restates `workspace_id` redundantly. It does **not** survive one that changes it.

**The tripwire, written as the thing to check rather than as a prediction.** `relays` becomes a
class the day any of these is true, and each is a one-line check against the schema or `src/`:

- a second child is added to `relays`, so that two paths can reach its rows by different routes.
  This one is enforced: `the_foreign_key_graph_is_the_one_the_adr_rules_on` fails on the migration
  that adds it;
- any path **updates** an existing `relay_buckets` row's `workspace_id` or `relay_id`, which makes
  the update arm take the parent and closes the cycle the insert arm cannot;
- **any transaction holds a `relay_buckets` row across more than one statement.** The whole ruling
  above is about a statement, and it stops being about a statement the moment the statement stops
  being the transaction. That is not only a new writer — a reset, a backfill, an operator "clear
  the bucket" endpoint — it is also `take` *itself* called inside a longer one, which is exactly
  what Task 11's tick would do if it ran `begin; take(…); …send…; complete(…); commit`. Then the
  insert arm's `for key share` on the relay is held across the sends, an operator's relay deletion
  blocks for the length of a tick rather than for one statement, and whatever else the tick takes
  joins the cycle candidates. **So the tick passes the pool to `take`, not a transaction**, and if
  it ever cannot, this section is re-derived rather than re-read.

`sending_domains` has no tripwire of this kind because it has no children; it has the other one.
The day anything references it — a campaign naming the domain it sends as is the obvious candidate
— it becomes a parent, the enumeration is ten plus one, and it is ruled on then. `0028` already
carries the `unique (workspace_id, id)` such a reference would point at, so the intent is on the
record even though the reference is not.

### M2b: why `templates` and `brand_kits` are not a lock class

**Read this section for what it says about code, which is almost nothing, because there is almost
no code.** Tasks R1-R7 added `0032`, the block tree, the MJML emitter and the renderers, and **not
one statement that writes `templates` or `brand_kits`**: when this was first written `src/`
contained no `insert into`, `update` or `delete from` for either, and the only writers were the
raw SQL in `tests/templates.rs`. **R7 has since added the handlers**, and their rows are in the
path table above; each was read against the statement it names. The ruling below is on the
**schema** and on the **statement shape every such handler has**, held by a test on that shape as
well as by the rows.

**The enumeration, against a database migrated from nothing to `0032`** (`pg_constraint`, not the
migration text — the migration is what was *written*, the catalog is what *runs*):

- `workspaces` gained two children, `brand_kits (c)` and `templates (c)`. Its fan-out is now
  seventeen. **No new parent**: the ten above are still the ten.
- Every constraint that mentions `templates` or `brand_kits`, in either column: both tables'
  primary keys, `templates_workspace_id_fkey`, `brand_kits_workspace_id_fkey`,
  `templates_workspace_id_id_key` (a unique key, not a reference), and five `check`s (three on
  `templates`, two on `brand_kits`). **Nothing references either table.** There are no user
  triggers on either, so no statement on them runs a second statement behind the first.
- `audit_log` still has only `audit_log_pkey`. A handler's audit write therefore takes nothing
  beyond a `RowExclusiveLock` on `audit_log` itself, which is the reasoning `delete_relay` was
  cleared by and is not repeated here.

**The ruling.** Neither is a lock class, for the reason the method's own definition gives: a class
exists where one path writes a child directly *and another reaches the same rows through a
parent's `on delete` action*, and the only parent either table has is `workspaces`, which **nothing
deletes** (`src/` has no `delete from workspaces`, re-run for this ruling) and whose rows are only
ever updated in non-key columns. A cascade that never fires cannot be the second path.

**What a write to either table locks**, which is the half the test holds:

| statement | takes on `workspaces` | why |
|---|---|---|
| insert a template | `for key share`, at the end of the statement | `templates_workspace_id_fkey`, checked by the referential trigger |
| insert a brand kit, or the insert arm of an upsert | `for key share`, likewise | `brand_kits_workspace_id_fkey` |
| update a template (name, tree, source, compiled) | **nothing** | writes no foreign key column, so no referential trigger fires |
| the update arm of a `brand_kits` upsert on `workspace_id` | **nothing** | `set` names `logo_url`, `palette`, `font_stack`, `footer`, `updated_at` — never `workspace_id`, so the same short-circuit as `relay_buckets`' update arm |
| delete a template or a brand kit | **nothing** | the child side of a foreign key has no delete-time check |

A `for key share` is compatible with `for no key update`, which is what every `UPDATE` of a
non-key `workspaces` column takes, so none of these waits on an ordinary workspace edit; and
because nothing takes a `workspaces` row exclusively, none waits on anything at all. Every one is
**one statement on one row by primary key**, which makes each a terminal node in the wait-for
graph: it holds what it holds and asks for nothing further. A cycle needs every node to be both a
holder and a waiter, so there is none to form.
`a_template_or_brand_kit_write_takes_at_most_a_key_share_on_its_workspace` measures exactly this
table, with the control the relay tests use: the same insert is raced first against an exclusive
hold on the workspace row and must block, so the wedge is shown to be real, and only then are the
updates, the delete and the upsert's update arm shown to pass it.

**The tripwires, written as things to check.** Each is a one-line check; the first and the last
are enforced:

- **Anything starts referencing `templates`.** The obvious candidate is a campaign naming its
  body, and `templates_workspace_id_id_key` was added so that such a reference can be composite.
  `the_foreign_key_graph_is_the_one_the_adr_rules_on` fails on the migration that adds it, and
  `templates` is then an eleventh parent to rule on **before** the migration merges. Rule it with
  `relays` as the model: a delete that takes the template row and then checks (or cascades into)
  the children, against a child writer that takes the child first and the template last. If the
  reference is `on delete restrict` or `no action`, nothing cascades but the referential check
  still looks at the child; if it is `cascade`, it is the `lists` shape.
- **Any handler holds a `templates` or `brand_kits` row across a second statement that locks
  something else** — a template save that also takes a `lists`, `contacts` or `suppressions` row,
  say. The ruling is about one statement; the audit write that follows a handler's statement is
  not such a second lock, for the `audit_log` reason above. A handler that grows a second one
  adds its row to the path table in that same change and is placed in the order above.
- **A handler writes `workspace_id`.** There is no reason to, since a template never changes
  workspace, and it is exactly the statement that would turn the "takes nothing" rows above into
  "takes `for key share`" ones.
- **A `delete from workspaces` appears anywhere.** That is the one change that makes `workspaces` a
  class for **seventeen** tables at once instead of fifteen. `nothing_in_src_deletes_a_workspace`
  fails on it, within the blind spots listed under "Known and open" below.

### M2c: why `campaigns` is not a class

**The enumeration was re-run for this ruling**, with the query above, against a database migrated
from nothing to `0034`: eleven parents, the same edges `the_foreign_key_graph_is_the_one_the_adr_rules_on`
pins. `campaigns` has two children, `jobs` (`jobs_campaign_fk`, `cascade`) and `messages`
(`messages_campaign_fk`, `no action`). `messages` has **no** foreign key to `workspaces` or to
`contacts` — its one constraint to a parent is the composite one onto `campaigns` — so the launch
batch's insert takes a `for key share` on the campaign row and on nothing else. The only trigger on
`jobs`, `jobs_throttle_retention`, admits a delete that arrives through a referential action
(`pg_trigger_depth() > 1`), which is how a draft's test jobs go with it.

**The ruling.** A class exists where one path writes a child directly and another reaches the same
rows through the parent's `on delete` action. `campaigns` has the second path in name only:

- **Its only deleter is `delete_campaign`, and it matches drafts.** `where state = 'draft'` cannot
  match a campaign that has launched, and a campaign that has messages has launched; were it ever
  to, `messages_campaign_fk` is `no action`, so the delete would be refused rather than cascade. The
  one thing it does cascade into is `jobs`, and `jobs` is a parent of nothing.
- **Every child insert asks for nothing after its statement.** A `campaign_test` job or a launch job
  takes `for key share` on the campaign at the end of its insert and then is done with it. The
  launch batch is the one path that asks twice — the `messages` insert's key share, then the
  `campaigns` update of the cursor and total — and both asks are for the same row, so it cannot be
  half of a cycle on it. The update is `for no key update` and a child's `for key share` does not
  wait on that, so an ordinary writer of a counter never makes a child insert wait either.
- **No update of a child writes `campaign_id` or `workspace_id`**, so none of them takes the
  parent. `record`, `sweep`, `claim`, `mark_submitting` and `release` write `messages` rows alone
  until `record` and `sweep` reach for the campaign's counters, last, in ascending id order.

**What the tests can and cannot show, said plainly.** Every other writer of a campaign row —
`pause`, `cancel`, `unschedule`, `resume`, `system_pause`, the refusals — is a leaf or takes
`campaigns` and then `jobs`: it holds the campaign and asks for nothing that a `messages` writer
holds, which is the whole content of "messages, then campaigns, then jobs". A cycle needs two paths
that each hold something the other wants, and a leaf is never one. So the race tests below cannot
be an ABBA with the code as it stands, and they are not pretending to be one: they hold the three
things that can still go wrong. A **deadlock**, red-first by a pause that also locks the campaign's
messages after the campaign row — which does cycle with a flush, in the one arrival order where the
pause is first — and that mutation is why both orders are run. A **wait that should not exist**,
which is the sweep's `skip locked` (a sweep that queued behind a flush would be the second node of
that cycle). And an **outcome counted twice or a pause overwritten**, which are the fence on
`record`'s `where` and the batch's update never naming `state`. The four campaign tests below
each wedge the campaign row `for update`, standing for the one exclusive taker a campaign could
have; the drain test wedges its `relays` row instead.

- `a_launch_batch_and_a_pause_do_not_deadlock` — a batch against the operator's pause route, both
  orders; the pause survives and the batch keeps its row and its total.
- `an_outcome_flush_and_a_pause_do_not_deadlock` — `record` against `system_pause`, both orders;
  the outcome is counted once and the finish check reads the state the pause left.
- `an_outcome_flush_and_a_sweep_of_the_same_message_count_it_once` — `record` against `sweep` on one
  row, both orders (R4's concern 6).
- `two_sweeps_at_once_fail_each_exhausted_message_once` — the exhausted-message statement, run twice
  concurrently (R4's re-review minor).
- `a_bucket_drain_takes_no_lock_on_its_relay` — the drain's update arm, with a first take's insert
  arm as the control.

**The tripwire, written as the thing to check.** `campaigns` becomes a class **the day a sent
campaign is deletable** — a retention job, or an operator's "delete this campaign and its mail". A
delete would take the campaign row `for update` and then reach `messages`, which is the order this
document forbids (`messages`, then `campaigns`) and the `lists` shape exactly: the flush holds a
message and wants the campaign, the delete holds the campaign and wants the message. The fix then
is the one `delete_list` made: take the `messages` rows (or refuse to delete while any exist)
before the campaign row, and rule on `no action` versus `cascade` on the way. `delete from
campaigns` outside `delete_campaign` is the grep that fires it; like `users` and `api_keys` it is a
grep somebody ran once, not a test. It was last run in R7 and found one deleter.

**What was found re-reading the M2c rows against their code**, because the standing obligation is
that a row and its code agree and say which was read. Two disagreements, both fixed above and in
this commit: the paragraph that opens the M2c table said no statement writes `campaign_id` or
`workspace_id`, which the launch batch's insert does, and the row for `pause` / `unschedule` /
`cancel` said "alone" for what is three statements in one transaction. The code was right in both.
Every other row — create, update, delete, schedule, resume, winner, test and preview/estimate; claim,
mark, release, record, sweep; the launch's batch, hand-off, freeze, refusal and the orphan sweep;
`system_pause`, the tick, `drain`, the test-send handler — was read against the statement it
names and agrees with it.

### M2d: the two families

This document now orders two families. The **address family** is the order at the top, `A, L, S,
contacts, M, C`. The **campaign family** is M2c's `messages`, then `campaigns`, then `jobs`. **The
address family comes before the campaign family** (spec M2d §4.5.7): `A, L, S, contacts, M, C`,
then `messages, campaigns, jobs`.

**Every path that takes both already does so.** The public form (`public::forms::apply`) inserts
its confirmation jobs after its consent rows, and erasure reaches `jobs` through the cascade from
`contacts`. Both reach `jobs` last. No path takes a campaign-family row and then an address-family
one.

**The bounce flush is the first path to take `A` and `S` and then `messages`.** `record` takes `A`
once per bounced address, in ascending lock-key order, with each address's `suppressions` insert
after its lock. Then it writes `messages`, then `campaigns`, in one transaction (the M2d `record`
row above). It is also the first path to take `A` for many addresses in one transaction, the case
`lock_address`'s SHORTCUT names. The ascending order is what keeps two flushes that share
addresses from cycling, and a flush bounds how many it holds. A second multi-address writer that
locks out of key order deadlocks with this one.

**Against a one-click on the same address**, both take `A` first and then write that address's
`suppressions` row, so they queue on `A`. The second finds the first's row and keeps it
(issues/27). A flush that wrote its suppression before taking `A` would cycle with a one-click
granted `A` first, on the uncommitted row.

- `two_flushes_bouncing_the_same_addresses_in_opposite_orders_both_commit`: a third transaction
  holds X's lock, and the flushes bounce `[X, Y]` and `[Y, X]`. Red-first by dropping the sort.
- `a_bounce_flush_and_a_one_click_for_the_same_address_both_commit`: both arrival orders.
  Red-first by inserting the suppression before taking `A`.

### The tripwire on `tags`, and what to check rather than predict

`tags` is quiet because of the shape of its writers, not because of anything structural — the
mechanism is live. A writer that **deletes a contact's `contact_tags` rows and re-inserts them**
deadlocks against `delete_tag` exactly as `set_lists` did against `delete_list`, and that is the
obvious next endpoint: a `PUT /contacts/{id}/tags` replacing the set. It has the same shape
`set_lists` already has.

**So: if that endpoint is written, `tags` becomes class 2b and it takes `for key share` on the tag
rows first.** This is written here as the thing to check when it merges, not as a prediction.

Today's writers of `contact_tags` are `tags::apply`, `tags::remove`, `selection::tag`,
`selection::untag` and the importer's `write_memberships_and_tags` — it is **not** "only the
importer", which is what this section used to say. The ruling survives on `on conflict do nothing`,
not on the writer count. And `jobs` is not a parent at all: nothing references it.

### Known and open: the importer's tag race (`issues/39`)

`write_memberships_and_tags` still raises `23503` on `contact_tags_workspace_id_tag_id_fkey` when a
tag is deleted while it runs, failing the whole import. Its `known` CTE reads `tags` under the
statement's snapshot while the end-of-statement referential check uses a newer one, so a tag it
*found* can be gone by the time the check runs; a tag it *created* in the same statement cannot,
being uncommitted. `selection::tag`'s pre-lock does not transfer, because **the importer resolves
tags by name, not by id**, and the whole thing is one statement, so the lock cannot be hoisted
without restructuring it. Reproduced, pre-existing, and recorded here so the next person meets it
as a known gap rather than as a new finding.

**The honest warning, since this document has now been wrong twice.** The claim above that a path
obeying this cannot deadlock with another that obeys it is only as good as the enumeration. It said
that while `contacts` was missing and while `form_lists` was listed as covered. What is different
now is that the enumeration is **closed and mechanical**: eleven parents, eleven ruled on (`campaigns` in M2c R3), each with a
reason that can be re-checked against the schema rather than against somebody's memory. If a
twelfth parent appears, it is a new row in that table — and since `0028` that no longer depends on
anybody remembering, because `the_foreign_key_graph_is_the_one_the_adr_rules_on` in
`tests/lock_order.rs` fails on the migration that adds one. If a class appears that is not
reachable from any of the eleven, the method itself is unsound and that is a bigger conversation than
another round. `issues/38` is the enforcement story this test is the first instalment of.

**Be exact about how much of this is enforced, because "no longer depends on anybody remembering"
is the kind of sentence that gets believed further than it is true.** What that test pins is the
*shape of the schema*: every foreign key edge and its `on delete` action. What it does not pin is
anything about the code.

- **Enforced.** A new parent, a new child, a removed edge, a changed cascade action.
- **Not enforced, and this is the bigger half.** Two of the ten rulings — `api_keys` and `users`
  (`workspaces` is now held by `nothing_in_src_deletes_a_workspace`) — rest on "`src/` contains
  no `delete from` for this table", which is a grep somebody ran once. Nothing fails if a delete
  is added. They become classes the day one is,
  and the only thing standing between here and there is a reviewer noticing. The same is true of
  every statement-shape claim in the `relays` section above: no test sees a `take` that grows a
  second statement.

### Known and open, carried rather than fixed

- **The `delete from` grep is enforced for `workspaces` only.** `nothing_in_src_deletes_a_workspace`
  scans `src/` for a `delete from` or `truncate` of `workspaces`, tolerating case, whitespace and
  newlines, `\n` / `\t` escapes, string continuations, `public.` qualifiers, quoted identifiers
  (bare or `\"`-escaped) and a `truncate` that names it anywhere in a list (`only` before a name,
  `*` after it). Whole-line `//` comments are skipped. Its matcher is held by
  `the_workspace_deletion_matcher_sees_what_it_is_for`, whose samples are written in the source
  form the matcher reads, escapes and all. **Its blind spots:** a table name not spelled out in
  `src/` (`format!("delete from {t}")`, `concat!`); anything outside `src/`; `drop table`; an
  SQL comment between tokens (`delete /* c */ from workspaces`); a spaced qualifier
  (`"public" . "workspaces"`); and a line *inside* a multi-line or raw string whose first
  non-whitespace is `//`, which the comment skip blanks along with real comments. The last three
  are not chased: nobody writes them — `//` is not SQL — and a matcher that tried would be
  fragile guesswork. **And the false-positive side:** a `/* */` block, a
  comment trailing code and a string that merely mentions the statement still trip it, so the
  failure prints the file, the line and the text that matched.
  `users` and `api_keys` are still a grep somebody ran once; the same test with another table name
  closes them.
- **The importer's tag race**, `issues/39`, two sections above. Still open, still reproduced,
  still raising `23503` on `contact_tags_workspace_id_tag_id_fkey` when a tag is deleted
  mid-import, and still failing the whole import when it does.
