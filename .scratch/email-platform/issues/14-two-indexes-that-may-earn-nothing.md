Status: ready-for-agent

# Two indexes that earn nothing, both now confirmed by `EXPLAIN`

Found in the M1b whole-branch review (D2-L1 and D2-L2, `final-d2-sql.md`) and **confirmed by
`EXPLAIN` in D5's pass** (`final-d5-report.md`), read from the top node on 50,000 segments and
50,000 snapshots in a migrated throwaway database. The plans below are **no longer hypotheses and
this issue does not retire**: both predictions were right, including the spelling D2 predicted for
the second. The output is under `## Comments`.

Both are still harmless at today's row counts, which is why this stays filed rather than fixed, and
both fixes are forward-only `drop index` migrations that change no result. One correction to what
the issue originally implied, measured in the same run and important enough to lead with: **the
supersede delete's cost is not the index at all** — 96% of it is the members foreign-key cascade.
See §2.

## 1 — `segments_keyset` duplicates what `unique (workspace_id, id)` already builds

`migrations/0016_segments.sql`:

```sql
unique (workspace_id, id)                                          -- line 18
create index segments_keyset on segments (workspace_id, id desc);  -- line 20
```

The unique constraint builds a btree on `(workspace_id, id)`. The only query that wants that
ordering is the keyset page (`.sqlx/query-095fcdad….json`: `where workspace_id = $1 … order by id
desc limit $3`), and with `workspace_id` pinned by equality a **backward** scan of the unique index
yields `id desc` directly. So `segments_keyset` appears to serve nothing the unique index does not,
and costs an index maintained on every segment write plus its storage.

It looks like `migrations/0011_contacts.sql:23` copied forward, where the same index is **not**
redundant: `contacts` has a bare `id uuid primary key` and no composite unique. `0016` added
`unique (workspace_id, id)` for the composite foreign key and kept the keyset index anyway.

Spec §7 carries the same pair, so the spec needs the same edit if this is actioned.

**Confirming run**, in a throwaway database with representative rows:

```sql
drop index segments_keyset;
explain (analyze, buffers) select id from segments
  where workspace_id = '…' order by id desc limit 26;
```

**Run, and confirmed.** It is still an `Index Scan Backward`, on `segments_workspace_id_id_key`,
with the same index condition and two more buffer hits:

```
with segments_keyset:  Limit → Index Scan using segments_keyset
                         Index Cond: (workspace_id = …)   Buffers: shared hit=29
after drop index:      Limit → Index Scan Backward using segments_workspace_id_id_key
                         Index Cond: (workspace_id = …)   Buffers: shared hit=31
```

Two buffers against an index maintained on every segment write. The index can go in a forward-only
migration.

## 2 — `selection_snapshots_actor`'s second and third columns can never be index quals

`migrations/0017_selection_snapshots.sql:30-31`, against `src/contacts/selection.rs:320-327`.

The index is `(workspace_id, created_by, created_by_key)` and the migration comment says its job is
the supersede delete. That delete is:

```sql
delete from selection_snapshots where workspace_id = $1
  and created_by is not distinct from $2::uuid
  and created_by_key is not distinct from $3::uuid
```

`IS NOT DISTINCT FROM` builds a `DistinctExpr`, and Postgres's `match_clause_to_indexcol` matches
only `OpExpr` / `ScalarArrayOpExpr` / `NullTest` / `RowCompareExpr`. That it is not an indexable
qual is a long-standing, documented limitation. So only `workspace_id = $1` is expected to reach
the index, and the two actor columns to be applied as a filter over every row of the workspace —
making this index do the job a single-column `(workspace_id)` index would, which the existing
`unique (workspace_id, id)` already does.

This is not a performance problem today: §8.4 bounds a workspace to roughly one live snapshot per
actor. It is recorded because the migration comment asserts the index serves the actor match, so
whoever later finds the supersede slow will tune the wrong thing.

**Confirming run:**

```sql
explain (analyze) delete from selection_snapshots where workspace_id = '…'
  and created_by is not distinct from null::uuid
  and created_by_key is not distinct from '…'::uuid;
```

**Run, and confirmed, including the spelling.** Exactly `Index Cond: (workspace_id = …)` with the
other two columns under `Filter:` as a `DistinctExpr`:

```
ModifyTable (Delete) on selection_snapshots
 └ Bitmap Heap Scan
     Recheck Cond: (workspace_id = …)
     Filter: ((NOT (created_by IS DISTINCT FROM NULL::uuid))
              AND (NOT (created_by_key IS DISTINCT FROM NULL::uuid)))
     └ Bitmap Index Scan on selection_snapshots_actor
         Index Cond: (workspace_id = …)   Plan Rows: 25068   Actual Rows: 25000
```

One fact D2 could not have: the planner cannot estimate it either — `Plan Rows: 1` against
`Actual Rows: 25000` on the `ModifyTable`, a 25,000× miss on the delete's input.

If it ever matters, the indexable spelling is `(created_by = $2 or ($2 is null and created_by is
null))`, or a generated identity column standing for "which actor".

### The supersede delete's real cost is the members cascade, not this index

This is the correction that matters most here, because an optimisation aimed at the wrong 3% is
worse than none: it looks done. From the same run:

```
Triggers: RI_ConstraintTrigger … selection_snapshot_members_workspace_id_snapshot_id_fkey
          Time: 315.644   Calls: 25000
Execution Time: 326.751
```

**315.6 ms of 326.8 ms — 96% — is the foreign-key cascade onto the members table.** The index is
not what anyone tuning a slow supersede should touch, and neither is the `DistinctExpr` above.
Harmless today, because §8.4 bounds a workspace to roughly one live snapshot per actor, so the real
call count is one rather than 25,000.

`migrations/0017_selection_snapshots.sql:38-40` is where that belongs, beside the actor index's own
comment — but **0017 is applied**, and editing an applied migration changes its checksum and makes
every database already at that version refuse it ("previously applied but has been modified"). That
has already cost this project time twice. So it is recorded here instead, and goes into the
migration only in the same forward-only change that drops the index.

## What it costs either way

Both are forward-only `drop index` migrations plus, for the second, a comment that stops claiming
something the plan does not do and names the cascade instead. Neither changes a result. The cost of
leaving them is one index maintained per segment write, one per snapshot write, and a comment that
misdirects the next person to look at this.

## Comments

**2026-11-16 — confirmed by `EXPLAIN`, status `needs-triage` → `ready-for-agent`.** Both plans
above were run by D5 (`final-d5-report.md`, "D2-L1 and D2-L2") on 50,000 segments and 50,000
snapshots in a throwaway database, read from the top node, and both of D2's predictions held. The
triage question this issue was filed with — *are the plans what reading says they are?* — is
answered, so it is no longer `needs-triage`; what remains is one forward-only migration and a
comment, which is fully specified above. **It does not retire**: nothing in either plan weakens the
finding, and the second one is stronger than filed, because the planner misestimates the delete's
input by 25,000× as well.

The one thing the issue got wrong is corrected in §2: it implied the supersede's cost lives in the
index, and 96% of it is the members FK cascade.
