Status: needs-triage

# Two indexes that may be earning nothing, both needing `EXPLAIN` to confirm

Found in the M1b whole-branch review (D2-L1 and D2-L2, `final-d2-sql.md`). Recorded rather than
fixed: the redundancy is confirmed by reading, the **plans** are hypotheses, and both are harmless
at today's row counts. Nothing here should be changed without the runs below.

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

Expectation: still an `Index Scan Backward`, on `segments_workspace_id_id_key`. If it is, the index
can go in a forward-only migration.

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

Expectation: `Index Cond: (workspace_id = …)` with the other two under `Filter:`.

If it ever matters, the indexable spelling is `(created_by = $2 or ($2 is null and created_by is
null))`, or a generated identity column standing for "which actor".

## What it costs either way

Both are forward-only `drop index` migrations plus, for the second, a comment that stops claiming
something the plan does not do. Neither changes a result. The cost of leaving them is one index
maintained per segment write, one per snapshot write, and a comment that misdirects the next
person to look at this.

## Comments
