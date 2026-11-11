# M1a contact core: Postgres data model recommendation

Scope: `contacts`, `lists`, `list_memberships`, `tags`, `contact_tags`, `custom_fields`, `suppressions`, and the
"may we email this address" predicate. Postgres 16+, sqlx 0.9, UUIDv7 ids generated in Rust, no code and no
migrations written here. Style follows `migrations/0001`-`0009` (lowercase SQL, `text` + `check (... in (...))`
instead of enums, `timestamptz not null default now()`, a comment above each table for anything non-obvious).

## How this was checked

Nothing below about plans or sizes is from memory alone. I loaded a scratch database (since dropped) in the
dev container (`postgres:16-alpine`, PG 16.15) with 2.1M contacts, 2.1M list memberships and 55k suppressions
using the schema below, and ran the queries. The "at 10M" figures are extrapolations from that: btree and heap
sizes scale linearly; GIN size is given as a range because it depends on how repetitive real emails are. The
dev box is not the 8 vCPU / 32 GB reference box, so treat timings as order of magnitude.

| Measured at 2.1M contacts | Size / time |
|---|---|
| `contacts` heap (4 JSON attributes, ~212 B/row) | 446 MB |
| `contacts_workspace_id_email_key` | 233 MB |
| `contacts_workspace_id_id_key` | 100 MB |
| `contacts_pkey` | 63 MB |
| trigram GIN, high-entropy emails / low-entropy synthetic | 194 MB (2.0M rows) / 93 MB |
| `list_memberships` heap / pkey / `(contact_id)` | 219 MB / 137 MB (with `include`) / 63 MB |
| `suppressions` (54.6k rows) heap / unique / `(workspace_id,id)` / pkey | 7.7 MB / 5.2 MB / 3.5 MB / 2.2 MB |
| mailable count, list of 1.5M members, 1.43M mailable | 1.4 s warm, 2.8 s cold |
| send-time check of a 500-contact batch | 16 ms |
| trigram insert overhead | +2.8 s per 200k rows (3.7 s vs 0.9 s) |

---

## Answers

### 1. Email identity

**Decision: a plain `email text` column that holds the already-normalised address, with
`unique (workspace_id, email)`. No `citext`, no `lower()` index.**

- `normalize_email` (`src/users/mod.rs`) already trims and lowercases in Rust, and `users.email` already relies on
  that plus a plain `unique`. Contacts should do exactly the same. One normaliser, in one language, is the only
  thing that guarantees the same address is the same string everywhere: contacts, the suppression hash, the
  importer, the unsubscribe token.
- Rust and Postgres do not agree on what "lowercase" means for non-ASCII. Checked: Postgres `lower('İ')` is `i`
  and `lower('ΑΣ')` is `ασ` on the dev DB; Rust's `str::to_lowercase` (documented behaviour) gives `i` + U+0307
  and `ας` (final sigma). `citext` and `lower()` both use the database's collation, so two self-hosters with
  different `LC_CTYPE` get different uniqueness rules from the same data, and a `C`-locale database lowercases
  only ASCII. A stored normalised column is collation-independent: equality is byte equality.
- `citext` is a contrib extension. It is trusted since PG13, so a database owner can create it, but it must be
  installed in every self-hoster's server (on RHEL and Alpine it is a separate `-contrib` package), and sqlx
  has no first-class `query!` support for it (needs `as "email: String"` overrides or `PgCiText`).
- The unique index doubles as the lookup index: `where workspace_id = $1 and email = $2` and
  `on conflict (workspace_id, email)` both use `contacts_workspace_id_email_key` directly. With a `lower(email)`
  functional index every caller must remember to wrap the column, and `ON CONFLICT` inference and the bulk
  importer get fussier for no gain.
- The cost, same as `users`: the original casing is lost (`John@Acme.com` is stored `john@acme.com`). Every
  mainstream sender does this in practice.

**Gaps in `normalize_email` that matter more for contacts than for users (recommend fixing before M1a ships,
needs an owner decision):** it does not apply Unicode NFC, so `é` as one code point and `e`+U+0301 are two
different contacts; and it does not convert an IDN domain to punycode. Add NFC (the `unicode-normalization`
crate, MIT/Apache-2.0) to a shared normaliser; leave punycode to M2 unless a relay requires it. It also should
not do Gmail dot/plus folding: that merges distinct people. Because the suppression hash (Q6) is permanent,
settle the normaliser *before* any suppression row is written.

DB-side backstop: only `char_length(email) between 3 and 254` (same count as the Rust check: characters, not
bytes). I deliberately add no `check (email = lower(email))`: it would reject valid Rust output for the cases
above.

### 2. Contact status

Three independent things forbid mailing. Each gets exactly one home, so no fact is stored twice:

| Fact | Where | Values |
|---|---|---|
| "This person told us to stop all email from this workspace" | `contacts.status` | `active`, `opted_out` |
| "This person is, or is not, on this list" | `list_memberships.status` (absence of a row = never a member) | `unconfirmed`, `subscribed`, `unsubscribed` |
| "This address must not be mailed" (hard bounce, complaint, soft-bounce limit, manual, GDPR, instance-wide) | `suppressions` row | n/a (reason column) |

State meanings:

- `contacts.status = 'active'`: no workspace-wide opt-out. Says nothing about lists or suppressions.
- `contacts.status = 'opted_out'`: workspace-wide opt-out. Reversible only by an explicit re-subscribe that the
  consent log records. Import never touches this column on an existing contact.
- `list_memberships.status = 'unconfirmed'`: double opt-in pending. Not mailable.
- `list_memberships.status = 'subscribed'`: mailable for that list, subject to the other two.
- `list_memberships.status = 'unsubscribed'`: left that list. Row is kept (not deleted) so a re-import cannot
  silently resubscribe.
- a `suppressions` row: bounced, complained, manually blocked, erased, or instance-blocked. It is keyed by
  address, not by contact, so it survives the contact row being deleted (GDPR) and applies if the address is
  re-imported.

Things deliberately **not** contact states: `bounced`, `complained`, `unsubscribed`, `unconfirmed`, `erased`.
Bounce and complaint are address facts (suppressions); unsubscribe and confirmation are per list; an erased
contact has no row. `contacts.status` has no timestamp column: when and why it changed is the consent log's job,
and a second column would be a second place to disagree. `list_memberships` has one `status_changed_at` rather
than `subscribed_at` + `unsubscribed_at`, for the same reason (the spec's "with timestamps").

**Spec wording to amend:** invariant 2 lists "unsubscribed" among suppressed addresses, and §10 says "unsubscribes
and bounces become suppressions". Under the per-list decision an unsubscribe from list A must not block list B,
so the importer must map Interspire's per-list unsubscribe to `list_memberships.status = 'unsubscribed'`, and
only bounces to `suppressions`. The invariant-2 test must cover all three sources.

### 3. The "may we email this address" predicate

**Decision: two plain (non-materialised) views over one `stable` SQL helper, defined once in a migration. Not a
scalar function, not an inlined CTE copy per caller.**

```sql
-- sha256 of the UTF-8 bytes of the normalised address. STABLE, not IMMUTABLE, on purpose: convert_to is stable,
-- and an IMMUTABLE wrapper around it is NOT inlined (measured: 2.8 s vs 1.4 s on the same count). Never use it in
-- an index or generated column.
create function email_hash(text) returns bytea language sql stable parallel safe
  as $$ select sha256(convert_to($1, 'UTF8')) $$;

-- Contacts that may receive marketing mail at all: no workspace-wide opt-out, not suppressed (workspace or instance).
-- Two NOT EXISTS, not one with `workspace_id = c.workspace_id or workspace_id is null`: the OR form could not use
-- the unique index and hashed the whole suppressions table for a 500-row batch (62 ms vs 16 ms measured).
create view sendable_contacts as
select c.workspace_id, c.id as contact_id, c.email
from contacts c
where c.status = 'active'
  and not exists (select 1 from suppressions s
                  where s.workspace_id = c.workspace_id and s.email_hash = email_hash(c.email))
  and not exists (select 1 from suppressions s
                  where s.workspace_id is null and s.email_hash = email_hash(c.email));

-- Contacts that may receive marketing mail for one list.
create view mailable_list_members as
select lm.workspace_id, lm.list_id, lm.contact_id, sc.email
from list_memberships lm
join sendable_contacts sc on sc.workspace_id = lm.workspace_id and sc.contact_id = lm.contact_id
where lm.status = 'subscribed';
```

Usage (callers add predicates; the rule never gets re-typed):

```sql
-- M1a mailable count
select count(*) from mailable_list_members where workspace_id = $1 and list_id = $2;
-- M2 launch: resolve in id-range chunks (UUIDv7 ids are ordered), insert into messages
select contact_id, email from mailable_list_members
 where workspace_id = $1 and list_id = $2 and contact_id > $3 order by contact_id limit 50000;
-- M2 send-time re-check, one lookup per batch: whoever is missing from the result is skipped
select contact_id from mailable_list_members
 where workspace_id = $1 and list_id = $2 and contact_id = any ($3);
```

Why a view:

- A scalar `is_mailable(contact)` function containing an `EXISTS` cannot be inlined by the planner (SQL function
  inlining refuses sublinks), so it would run as a per-row subplan: tens of seconds at millions of rows and it
  blocks the anti-join. A set-returning function can inline, but a view gives the same single definition with
  less ceremony, and it composes with `= any($3)`.
- Views are flattened, so the caller's filters push down. Measured plans: a 500-id batch is a nested-loop with
  index probes on `contacts_workspace_id_id_key` and the suppression unique index (16 ms). The bulk count is a
  hash anti-join against the (small) suppression set, parallel, 1.4 s warm for 1.43M mailable of 2.1M.
- An inlined CTE per caller is the thing to avoid: the count, the launch and the send-time check would each carry
  a copy of the predicate, and invariant 2 is exactly the rule that must not drift.
- Per recipient the hash is computed on the fly (sha256 of ~25 bytes, sub-microsecond). I did not store it on
  `contacts`: that would be +~0.35 GB at 10M and a second copy of the email.

At 5M recipients extrapolate the bulk path to roughly 5-10 s warm (the two 5M-row joins dominate), well inside the
5-minute launch target. It is **not** a UI-latency query: show a cached or asynchronously computed number for
per-list mailable counts. Send-time throughput is a non-issue: 1,500/s is ~3 batches/s of 500 at ~16 ms each.

Not covered by M1a, flagged for M2: (a) campaigns to *several* lists want "subscribed in any targeted list",
a `distinct` over the same view; (b) segment campaigns use `sendable_contacts` and need a product decision on
whether they also require a subscribed membership (consent basis); (c) transactional sends should check only
suppressions (spec: hard bounce still applies), so they probe `suppressions` directly and do not use these views.

### 4. Indexes

At 10M contacts, ~1.5 memberships per contact (15M rows), ~2 tags per contact (20M rows, assumption), 1M
suppressions (generous).

| Index | Serves | Est. size at 10M |
|---|---|---|
| `contacts_pkey (id)` | FK target, `/contacts/{id}` | 0.30 GB (measured 63 MB / 2.1M) |
| `contacts (workspace_id, id)` unique | cursor pagination (`where workspace_id=$1 and id < $2 order by id desc`, scanned backward), composite-FK target | 0.48 GB |
| `contacts (workspace_id, email)` unique | exact lookup, `ON CONFLICT`, uniqueness | 1.1 GB |
| `contacts_search` GIN trigram on `email \|\| ' ' \|\| first_name \|\| ' ' \|\| last_name` | search by email or name, prefix or substring | 0.5-1.0 GB (range from the two measurements) |
| `contacts (workspace_id, id) where status <> 'active'` | "opted out" filter. Without it the filter walks all 10M rows to find a few | < 10 MB |
| `list_memberships_pkey (list_id, contact_id)` | filter by list with cursor on contact id, uniqueness, `ON CONFLICT`, list-delete cascade | ~0.7 GB (137 MB/2.1M, less the dropped `include`) |
| `list_memberships (contact_id)` | contact detail "lists", **FK cascade when a contact is deleted** (GDPR), send-time batch (`contact_id = any($3)`) | 0.45 GB |
| `list_memberships (list_id, contact_id) where status <> 'subscribed'` | Unsubscribed / Unconfirmed tabs | < 50 MB |
| `contact_tags_pkey (tag_id, contact_id)` | filter by tag, cursor on contact id, uniqueness | ~1.0 GB |
| `contact_tags (contact_id, tag_id)` | tags of one contact, FK cascade | ~1.0 GB |
| `suppressions_pkey (id)` | FK-less identity | 0.04 GB per 1M |
| `suppressions (workspace_id, email_hash)` unique nulls not distinct | the per-recipient lookup (both the workspace probe and the `workspace_id is null` instance probe), upsert key | 0.1 GB per 1M |
| `suppressions (workspace_id, id)` | suppression list page, cursor by id | 0.06 GB per 1M |

Total on `contacts`: heap ~2.1 GB, indexes ~2.4-2.9 GB. Everything fits in cache on the reference box.

Notes:

- No separate indexes for the mailable count: it is a join over the PK of `list_memberships`, the unique index of
  `contacts`, and the suppression unique index. I did **not** add `include (status)` to the membership PK; I
  did not verify the planner would use it index-only and it would make every status change a non-HOT update.
- The `desc` that M0 puts on `audit_log_ws_id` is cosmetic: a btree is scanned backward at no cost, so the
  `unique (workspace_id, id)` serves `order by id desc` as is.
- Counting `contact_tags` and `list_memberships` per list or tag exactly is O(rows); same advice as Q3: cache it.

**GIN on `contacts.attributes` at M1a: against.**

1. Nothing queries it. The segment engine is the next milestone and has not chosen its query shapes.
2. It would be the most expensive index on the table. `jsonb_ops`/`jsonb_path_ops` GIN on a 10M-row column with a
   handful of keys is typically multi-GB and the slowest GIN to maintain; it would eat most of the 2-minute
   import budget (the trigram GIN alone costs ~14 s per 1M rows, measured).
3. It kills HOT updates. Any update to `attributes` changes an indexed column, so every index on the row is
   rewritten. Without it, flow-driven attribute updates stay HOT.
4. It would not even serve the likely rules. GIN containment (`@>`) serves equality on a key. "signup date before
   X" and "lifetime value over N" are range and typed comparisons it cannot serve; those need per-field
   expression btrees such as `((attributes->>'plan'))` or `(((attributes->>'ltv')::numeric))`, which the segment
   engine can create from `custom_fields` definitions.
5. Deferring is free: `create index concurrently` on an existing table needs no rewrite and no downtime.

Add it, or better the expression indexes, when the segment engine's EXPLAIN plans show the need.

### 5. Search

**Decision: `pg_trgm` GIN on one concatenated expression. Not full-text, not a prefix-only btree.**

```sql
create index contacts_search on contacts using gin ((email || ' ' || first_name || ' ' || last_name) gin_trgm_ops);
-- query: ... where workspace_id = $1 and (email || ' ' || first_name || ' ' || last_name) ilike $2 order by id desc limit 50
```

- Full-text search tokenises words and stems; it cannot match a substring of an email and handles addresses
  badly. Wrong tool.
- A prefix btree (`text_pattern_ops`) is the cheapest option and would be my fallback, but it cannot do the
  "acme" in `john@acme.com` substring the spec asks for, and it needs a second index next to the unique one.
- Trigram handles prefix, substring and name in one index, case-insensitively (`ilike`, pg_trgm folds case).
  Exact-address lookup never touches it (that is the unique btree). The API should require at least 3
  characters (trigrams need 3; below that the query degrades to a scan) and must escape `%`, `_` and `\` in
  the user's text before building the pattern.
- The expression must be repeated verbatim in the query for the planner to use the index. Keep it in one data
  access function. `first_name`/`last_name` are `not null default ''` so the concatenation never yields null.
- One combined index instead of separate email and name indexes: one bitmap scan, "john acme" style queries work,
  one structure to maintain.

Measured at 2M rows (high-entropy emails, p = single run on the dev box): selective substring 1-2 ms; 3-char
prefix matching ~2.6k candidates 30 ms; dense matches (`gmail`, 30% of rows) the planner abandons the GIN and
walks the id index backward, 7-100 ms. The tenant filter is not in the GIN (that would need `btree_gin`, a
second extension) so the index is global across workspaces; fine for the self-hosted case, and a large
multi-tenant install still filters by the rare trigram first.

**Extension requirement:** `pg_trgm` is a contrib module, trusted since PG13 (so the database owner can
`create extension` without superuser). It ships in the official `postgres` Docker image (checked: the 16-alpine
image offers it), in Debian/Ubuntu's main `postgresql-16` package, and on the managed clouds; on RHEL/PGDG and
Alpine-from-packages it is `postgresql16-contrib`. Make it a hard requirement: the migration runs
`create extension if not exists pg_trgm;` and fails loudly with a message. Do **not** make the index
conditional on the extension being present: that gives different schemas on different installs. Document it in
the install guide and have `nm migrate` print a readable error. M0 migrations use no extensions, so this is the
first one; `citext` and `btree_gin` are not needed.

**Uncertain, and what would settle it:** mid-density queries at 10M rows, e.g. a 3-character term matching
~0.5% of contacts (50k rows). The GIN is not selective enough to be instant and a backward id walk is not dense
enough to be quick; I expect 100-250 ms and have not measured it. Settle it with a 10M-row synthetic benchmark
(realistic domain skew, mixed name lengths) before M1a is signed off. If p95 exceeds 150 ms, the fallback is
dropping substring support and using a `text_pattern_ops` prefix index on `(workspace_id, email)`.

### 6. Suppressions

**Decision: one table, `workspace_id` nullable (null = instance-wide), looked up by `email_hash`, with the
plaintext address stored alongside it (nullable).**

- One table over two: the send-time check needs both scopes for the same hash; GDPR, the importer and the
  bounce intake write the same row shape; and `unique nulls not distinct (workspace_id, email_hash)` (PG15+,
  fine for PG16+) gives one row per address per scope, with `ON CONFLICT (workspace_id, email_hash)` working
  for the instance scope too (tested). The risk with a nullable tenant column is a tenant query leaking
  instance rows. Mitigation: tenant endpoints only ever write `workspace_id = $1` (equality, never
  `or workspace_id is null`), and the only place that mentions the null scope is `sendable_contacts` plus the
  instance-admin endpoints. This is the one table that deliberately breaks "every tenant table has a non-null
  `workspace_id`"; note it in the migration comment like `audit_log`'s no-FK note.
- Lookup is by hash, always: fixed 32 bytes (a compact index), uniform for live and erased rows, and a batch
  check is `email_hash = any($1)`. Measured 16 ms for a 500-contact batch including list-membership and contact
  probes; ~3 batches/s at 1,500 emails/s is negligible. No cache is needed. If a cache is ever added, it must
  be invalidated on the same transaction as the write (an unsubscribe landing mid-campaign must take effect).
- Store both. The hash is the key and the only thing kept after GDPR delete. The plaintext (`email`) lets the UI
  show "bob@x.com is blocked because of a hard bounce on 3 March" and lets an admin export the list. It is
  nulled on erasure, including on a pre-existing bounce row: the erase procedure must `update suppressions set
  email = null where email_hash = $1`. A check makes the GDPR case structurally safe:
  `check (reason <> 'gdpr_erasure' or email is null)`.
- One row per (scope, address); the `reason` is the first cause. A later complaint on an already hard-bounced
  address is `on conflict do nothing`; the history is in `events` and the audit log, not here.

**One-way door, decide now: the hash function.** I recommend unsalted `sha256` over the UTF-8 bytes of the
normalised address, via `email_hash()` above, because it can be computed identically in SQL and Rust (the view,
the importer's staging step and any backfill need it in SQL) and matches how M0 hashes tokens. The cost is that
emails are low-entropy: an attacker with a database dump can confirm that a *known* address was erased. The
stronger option is HMAC-SHA256 keyed from the master key; it cannot be computed in SQL without `pgcrypto`, so
`contacts` would have to carry a stored hash (+~0.35 GB at 10M), and rotating the key is impossible for erased
rows because their plaintext is gone. Whichever is chosen is permanent. I have not seen the legal position the
spec's owner takes on pseudonymised hashes; if "hashed" must mean "not reversible by dictionary", choose HMAC and
accept the stored column.

GDPR delete is: in one transaction, upsert the workspace-scoped suppression (`reason = 'gdpr_erasure'`,
`email = null`, or null the plaintext of the existing row), then `delete from contacts` (cascades memberships
and tags). `consent_log` (IP, timestamps) and later `messages`/`events` must not carry an email or IP that
survives this; have them reference `contact_id` only and be erased or anonymised in the same procedure.

### 7. Volume and partitioning

**Premature. None of these tables should be partitioned in M1a.** At 10M contacts: `contacts` heap ~2.1 GB,
`list_memberships` ~1.6 GB, `contact_tags` ~1.6 GB, suppressions ~0.2 GB. Postgres is comfortable with 100M-row
tables; these all fit in the reference box's 32 GB cache with room to spare. Partitioning would also cost real
things here: every unique constraint must contain the partition key (so `contacts_pkey (id)` and the composite-FK
target would both change), pruning does not help a single large workspace, and the instance-wide suppression
scope fits awkwardly. Revisit only if a single table passes ~100M rows or its working set stops fitting in memory.

What to do instead (cheap, in the `create table` statements):

- `autovacuum_vacuum_scale_factor = 0.02` and `autovacuum_analyze_scale_factor = 0.01` on `contacts`,
  `list_memberships`, `contact_tags`. The default 0.2 means two million dead rows at 10M before vacuum runs.
- `fillfactor = 90` on `contacts` so attribute and `updated_at` updates can stay HOT. This is a judgement call;
  I did not measure it.
- Deleting a list with millions of memberships via `on delete cascade` is one huge transaction. The API should
  delete memberships in id-ranged batches first (a job in M1b), then the list.

### 8. Import (`COPY` into staging, then `INSERT ... ON CONFLICT`)

What M1a must get right so M1b needs no migration:

1. **A real unique key on the natural identity of every upserted thing**, as a constraint or unique index over
   plain columns: `contacts unique (workspace_id, email)`; `list_memberships primary key (list_id, contact_id)`;
   `contact_tags primary key (tag_id, contact_id)`; `tags unique (workspace_id, name)`;
   `custom_fields unique (workspace_id, key)`; `suppressions unique nulls not distinct (workspace_id,
   email_hash)`; and `lists unique (workspace_id, source_ref) where source_ref is not null` (the stable key an
   Interspire list id maps to, so a re-run finds the same NM list instead of creating a second one or relying on
   a renameable name). Without these, `ON CONFLICT` has no target and a re-run duplicates.
2. **Email is stored normalised**, so the staging table is normalised once in Rust and conflicts are exact byte
   matches. The suppression hash is computed in SQL from the same string with `email_hash()`, so the importer
   needs no key material.
3. **Ids carry no default.** PG16 has no `uuidv7()` (it arrived in 18), so the importer assigns UUIDv7 ids in
   Rust and COPYs them in; on conflict the existing id wins. This matches M0 (`id uuid primary key`, no default).
4. **No triggers on these tables.** Row triggers (for `updated_at`, counters) would defeat bulk speed. The app
   sets `updated_at` explicitly, as M0 does.
5. **CHECK constraints are the only validation inside the statement and a failing row aborts the whole
   `INSERT ... SELECT`.** Keep them few and cheap, and validate in Rust while streaming into staging, diverting
   rejects to the report the spec requires. Dedupe the staging table on `(workspace_id, email)` first:
   `ON CONFLICT DO UPDATE` cannot touch the same row twice in one statement.
6. **Re-run safety is an upsert policy, which the schema must allow:**
   - contacts: `on conflict (workspace_id, email) do update set first_name, last_name, attributes = contacts.attributes || excluded.attributes ... where (contacts.first_name, contacts.last_name, contacts.attributes) is distinct from (...)`. The `where` makes an unchanged re-run write nothing (no WAL, no bloat). `status` is never in the `set` list, so a re-import cannot un-opt-out anyone.
   - memberships: `on conflict (list_id, contact_id) do update set status = 'unsubscribed' where list_memberships.status <> 'unsubscribed'` for imported unsubscribes (only ever moves towards restriction), `do nothing` for imported subscribes, so a re-run cannot resubscribe someone who unsubscribed after the first run.
   - suppressions: `on conflict (workspace_id, email_hash) do nothing`.
7. **Composite FKs are checked per row** (two probes per membership). Measured cost is small, but it is the
   reason the unique `(workspace_id, id)` on `contacts`/`lists`/`tags` exists; do not drop it.
8. **The trigram GIN is the dominant write cost:** +2.8 s per 200k rows measured, so about +14 s per 1M rows on
   top of heap and the other three btrees. That should still leave a 1M import under 2 minutes, but it is the
   first thing to check in the import benchmark; raising `gin_pending_list_limit` on `contacts_search` is the
   knob if it does not. Loading in 50k-row statements keeps the pending list and WAL bounded.

---

## Recommended schema

Suggested files `0010_contacts.sql` and `0011_suppressions.sql`; numbering and split are the builder's call.
The suppression table must exist before the views, so the views go last.

```sql
-- SPDX-License-Identifier: AGPL-3.0-only

-- Trigram search (contacts_search). contrib, trusted since PG13: the database owner can create it.
create extension if not exists pg_trgm;

-- `source_ref` is the id of this list in the system it was imported from (Interspire list id), so a re-run
-- of `nm import` finds the same list. `double_opt_in` decides whether a new member starts `unconfirmed`.
create table lists (id uuid primary key, workspace_id uuid not null references workspaces(id) on delete cascade,
  name text not null, double_opt_in boolean not null default true, source_ref text,
  created_at timestamptz not null default now(), updated_at timestamptz not null default now(),
  unique (workspace_id, name), unique (workspace_id, id));
create unique index lists_ws_source_ref on lists (workspace_id, source_ref) where source_ref is not null;

create table tags (id uuid primary key, workspace_id uuid not null references workspaces(id) on delete cascade,
  name text not null, created_at timestamptz not null default now(),
  unique (workspace_id, name), unique (workspace_id, id));

-- Typed definitions for the keys of contacts.attributes. `key` is the JSON key. The type of a field does not
-- change once created (the API rejects it): existing values would need converting. Values are stored as JSON
-- number / ISO-8601 date string 'YYYY-MM-DD' / JSON boolean / JSON string.
create table custom_fields (id uuid primary key, workspace_id uuid not null references workspaces(id) on delete cascade,
  key text not null check (key ~ '^[a-z][a-z0-9_]{0,62}$'), label text not null,
  type text not null check (type in ('text','number','date','boolean')),
  created_at timestamptz not null default now(),
  unique (workspace_id, key));

-- `email` is stored normalised (users::normalize_email plus NFC) and never lowercased in SQL: the unique key
-- relies on it, and no citext / lower() index is used. `status` is the workspace-wide opt-out only: per-list
-- state lives in list_memberships, and bounces, complaints and GDPR blocks live in suppressions, so none of
-- those is repeated here. `first_name`/`last_name` are built-in columns, not custom fields, so personalisation
-- and search do not need JSON. `attributes` holds custom-field values keyed by custom_fields.key.
-- unique (workspace_id, id) is the target of the composite foreign keys below and the cursor-pagination index.
create table contacts (id uuid primary key, workspace_id uuid not null references workspaces(id) on delete cascade,
  email text not null check (char_length(email) between 3 and 254),
  first_name text not null default '', last_name text not null default '',
  status text not null default 'active' check (status in ('active','opted_out')),
  timezone text, attributes jsonb not null default '{}' check (jsonb_typeof(attributes) = 'object'),
  created_at timestamptz not null default now(), updated_at timestamptz not null default now(),
  unique (workspace_id, email), unique (workspace_id, id))
  with (fillfactor = 90, autovacuum_vacuum_scale_factor = 0.02, autovacuum_analyze_scale_factor = 0.01);
-- Search by email or name, prefix or substring. The query must use the same expression verbatim.
create index contacts_search on contacts using gin ((email || ' ' || first_name || ' ' || last_name) gin_trgm_ops);
-- The "opted out" filter; opted-out contacts are rare, so this is tiny.
create index contacts_ws_not_active on contacts (workspace_id, id) where status <> 'active';

-- Absence of a row means never a member. A row is kept after unsubscribe so an import cannot resubscribe.
-- Composite foreign keys make a cross-workspace membership unrepresentable.
-- (contact_id) serves the contact's list view, the GDPR cascade and the send-time batch check.
create table list_memberships (workspace_id uuid not null, list_id uuid not null, contact_id uuid not null,
  status text not null check (status in ('unconfirmed','subscribed','unsubscribed')),
  status_changed_at timestamptz not null default now(), created_at timestamptz not null default now(),
  primary key (list_id, contact_id),
  foreign key (workspace_id, list_id) references lists (workspace_id, id) on delete cascade,
  foreign key (workspace_id, contact_id) references contacts (workspace_id, id) on delete cascade)
  with (autovacuum_vacuum_scale_factor = 0.02, autovacuum_analyze_scale_factor = 0.01);
create index list_memberships_contact on list_memberships (contact_id);
-- Unsubscribed / unconfirmed tabs; the bulk of rows are subscribed, so this is small.
create index list_memberships_not_subscribed on list_memberships (list_id, contact_id) where status <> 'subscribed';

create table contact_tags (workspace_id uuid not null, tag_id uuid not null, contact_id uuid not null,
  created_at timestamptz not null default now(),
  primary key (tag_id, contact_id),
  foreign key (workspace_id, tag_id) references tags (workspace_id, id) on delete cascade,
  foreign key (workspace_id, contact_id) references contacts (workspace_id, id) on delete cascade)
  with (autovacuum_vacuum_scale_factor = 0.02, autovacuum_analyze_scale_factor = 0.01);
create index contact_tags_contact on contact_tags (contact_id, tag_id);
```

```sql
-- SPDX-License-Identifier: AGPL-3.0-only

-- `workspace_id` is null for an instance-wide entry (instance admins only; tenant endpoints always filter
-- `workspace_id = $1`). That makes this the one tenant-scoped table without a non-null workspace_id, on purpose.
-- `email_hash` is sha256 of the UTF-8 bytes of the normalised address (email_hash()); it is the lookup key and
-- the only thing kept after a GDPR erasure. `email` is the same address in clear for display and export, and is
-- null for `gdpr_erasure` rows and nulled on any existing row when the contact is erased. One row per address per
-- scope: `reason` is the first cause; later ones are `on conflict do nothing`.
-- The unique index is (workspace_id, email_hash) so both the workspace probe and the `workspace_id is null`
-- instance probe are index conditions; it needs `nulls not distinct`, hence Postgres 15+.
create table suppressions (id uuid primary key, workspace_id uuid references workspaces(id) on delete cascade,
  email_hash bytea not null check (octet_length(email_hash) = 32), email text,
  reason text not null check (reason in ('hard_bounce','soft_bounce','complaint','manual','imported','gdpr_erasure')),
  source text not null default '', created_at timestamptz not null default now(),
  check (reason <> 'gdpr_erasure' or email is null),
  unique nulls not distinct (workspace_id, email_hash));
-- The suppression list page, cursor by id.
create index suppressions_ws_id on suppressions (workspace_id, id);

-- (email_hash(), sendable_contacts and mailable_list_members: exactly as in answer 3.)
```

## Rejected alternatives

- **`citext`.** Extension to install; collation-dependent case rules that differ between installs and from Rust;
  awkward with sqlx macros; saves nothing over normalising in the one place we already do.
- **`unique (workspace_id, lower(email))`.** Same lowercasing disagreement (`İ`, final sigma), and every query
  and `ON CONFLICT` must wrap the column to use the index.
- **Contact status with `bounced` / `complained` / `unsubscribed` / `unconfirmed` / `erased`.** Each would
  duplicate a fact owned by `suppressions` or `list_memberships`, so two tables could disagree about the same
  person. Bounce and complaint are about the address and must outlive the contact row.
- **`contacts.opted_out_at` plus `status`, or `subscribed_at` plus `unsubscribed_at` plus `status`.** Two columns
  for one fact.
- **A scalar `is_mailable()` SQL function.** Contains an `EXISTS`, so it is not inlined and runs per row.
- **Making the SQL helper `IMMUTABLE`.** `convert_to` is stable, so the wrapper is not inlined (measured 2x
  slower) and would be a lie if ever used in an index.
- **One `NOT EXISTS` with `workspace_id = c.workspace_id or workspace_id is null`.** Cannot use the unique index;
  for a 500-contact batch the planner hashed the whole suppression table.
- **A stored `mailable` boolean on `contacts` or `list_memberships`.** Three writers (unsubscribe, bounce
  intake, GDPR) would all have to maintain it transactionally; a missed update emails a suppressed address.
- **Two suppression tables.** Doubles the send-time probe code and the importer paths for no isolation the
  view does not already give. Revisit if the null-scope row ever leaks through a tenant query.
- **GIN on `attributes` now.** See Q4: no consumer, costly writes, defeats HOT, wrong shape for range rules.
- **EAV for custom fields.** Already rejected by the owner; noted for completeness.
- **Full-text search (`tsvector`) for contact search.** Cannot do substring; poor tokenisation of addresses.
- **A prefix-only `text_pattern_ops` btree as the primary search.** Cheaper to write but cannot match "acme" in
  `john@acme.com`; kept as the fallback if the trigram benchmark fails (Q5).
- **`primary key (workspace_id, id)` instead of `primary key (id)` plus `unique (workspace_id, id)`.** Saves
  ~0.3 GB per 10M contacts and is arguably cleaner, but departs from M0's `id uuid primary key` convention.
  Reasonable if the extra index ever hurts.
- **A generated stored `email_hash` column on `contacts`.** Impossible with `sha256(convert_to(...))`
  (`convert_to` is not immutable) and +0.35 GB; only worth it if the hash switches to HMAC.
- **Partitioning any of these tables.** See Q7.
- **`list_memberships_pkey ... include (status)`.** Unproven index-only benefit, makes status changes non-HOT.

## Open items for the owner

1. HMAC-SHA256 versus plain sha256 for `email_hash` (one-way door, Q6).
2. Add NFC normalisation to the contact normaliser before the first suppression row exists (Q1).
3. Built-in `first_name` / `last_name` columns are my assumption; the spec lists only `email`, `status`,
   `attributes`, `timezone`. If names must live in `attributes`, the search expression changes to
   `email || ' ' || coalesce(attributes->>'first_name','') ...` and loses HOT on name edits.
4. Spec §8.2 and §10 wording versus per-list unsubscribe (Q2).
5. Segment-campaign consent basis: must a segment recipient also be subscribed to some list (Q3)?
6. Run the 10M-row benchmark for mid-density trigram queries and the 1M-row import before relying on the
   150 ms and 2-minute targets (Q5, Q8). My numbers are extrapolated from 2.1M rows on a laptop container.
