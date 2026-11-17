# M1c decision record

Decisions and measurements from milestone M1c (the CSV importer: upload, sniff, map, the `COPY`
into staging, the set-based upsert, and the per-row error report) whose reasoning does not fit in a
code comment and is not recoverable from the diff.

Read `m0-decisions.md`, `m1a-decisions.md` and `m1b-decisions.md` first; everything in them still
applies.

---

## The 1M-row budget, measured

`spec.md:459` budgets **a 1M-row CSV import at under two minutes**. The M1b spec's §7.2 says why
the pipeline has the shape it has: *"Row-at-a-time `INSERT … ON CONFLICT` does not reach that on
any hardware worth buying."*

**The budget is missed on this machine, by a factor of about 2.7.** The measurement is
`tests/import_perf.rs`, which is `#[ignore]` by default.

### The machine

A number without its machine cannot be compared to the next one, so here is this one. **It is not
the reference box `spec.md` §9 names** (a single VM with 8 vCPU and 32 GB RAM running both `nm all`
and Postgres). It is a developer laptop, and the gap that matters most is memory: the whole Docker
VM has 3.8 GB, where the reference box gives Postgres alone a share of 32 GB.

| | |
|---|---|
| Host CPU | AMD Ryzen 7 250 (Radeon 780M), 8 cores / 16 threads, 3.30 GHz max |
| Host RAM | 31.3 GB |
| Host OS | Windows 11 Home Single Language 10.0.26200, Lenovo 83JG |
| Storage | Samsung MZAL81T0HFLB-00BL2, NVMe SSD, 954 GB (704 GB free at measurement) |
| Containers | Docker Desktop on WSL2. The VM is capped to **4 GB** by `~/.wslconfig` (`[wsl2] memory=4GB`, `autoMemoryReclaim=dropCache`). The container sees 16 CPUs and `MemTotal: 4007580 kB`, **shared with every other container on the host.** |
| Postgres | `postgres:16-alpine`, **PostgreSQL 16.15** on `x86_64-pc-linux-musl`. Data on the docker volume `nm_pgdata`, i.e. the WSL2 ext4 filesystem inside the vhdx on the NVMe above. |
| Postgres config | Stock. `shared_buffers = 128 MB`, `work_mem = 4 MB`, `maintenance_work_mem = 64 MB`, `max_wal_size = 1 GB`, `effective_cache_size = 4 GB`, `random_page_cost = 4`, `max_parallel_workers = 8`, `fsync = on`, `synchronous_commit = on`, `full_page_writes = on`, `data_checksums = off`. |
| Load | Nothing else was building or testing. The five other containers on the host (`oci-email-postgres`, `marketing-automation`, `email-marketing-saas-postgres-1`, its redis, and an idle `nm` dev server) were all at 0.00% CPU throughout. |

### What was measured, and what it does not cover

**The two pipeline stages, called directly — not `POST /imports/{id}/start`.** The handler also
does the multipart upload, the authorisation, the advisory lock, the `imports` row updates and two
audit rows, and a single wall-clock figure over all of that cannot say which phase moved. The
upload is a network and is charged to the client's link. The `COMMIT` is timed separately below,
because it is the one part of the handler's extra database work that makes the import durable.

**The file is the shape an operator actually uploads**, six columns:
`email,first_name,last_name,timezone,tags,lists` — 1,000,000 rows, 65.3 MiB, every row carrying
two tags out of a pool of 50 and the name of the list the import was also pointed at. The run
therefore writes **1,000,000 contacts, 1,000,000 list memberships and 2,000,000 contact tags**, and
the figures below are for that. A file of addresses and names alone writes none of the last two and
would be faster; the point of this shape is that it exercises all three phases.

`dedup = update existing` against an empty workspace, so: no row was an update, nothing collapsed,
nothing failed, `suppressions` was empty and no address was erased.

### The three numbers

Three runs of the **release** build, which is what ships. Wall clock as the test measures it:

| run | `COPY` | upsert (all of it) | `COMMIT` | pipeline |
|---|---|---|---|---|
| 1 | 3.881 s | 340.036 s | 0.035 s | **343.952 s** |
| 2 | 3.815 s | 324.123 s | 0.021 s | **327.959 s** |
| 3 | 3.750 s | 319.250 s | 0.032 s | **323.031 s** |
| median | 3.815 s | 324.123 s | 0.032 s | **327.959 s** |
| spread | 0.131 s (3.4%) | 20.8 s (6.4%) | — | 20.9 s (6.5%) |

**The runs agree.** The spread is 6.5% of the pipeline and the ordering is monotonically downward,
which is a warming cache, not disagreement. Nothing here needs more runs to be believed.

**Verdict: the budget is MISSED. 327.96 s against 120 s — over by 207.96 s, or 2.73×.**

The third phase cannot be timed from outside, because `write_memberships_and_tags` is private and
runs inside `upsert_from_staging`. It is split out here by `log_min_duration_statement = 0`, which
makes Postgres time each statement server-side; the test turns it on, and these are the figures it
logged. Milliseconds, as logged:

| run | `create temporary table` | `COPY` | error budget | **contacts upsert** | **memberships and tags** | erasure sweep | `COMMIT` |
|---|---|---|---|---|---|---|---|
| 1 | 2.031 | 3877.774 | 0.026 | 107626.337 | 232334.636 | 0.825 | 34.587 |
| 2 | 2.105 | 3811.063 | 0.029 | 106919.283 | 217013.622 | 0.102 | 20.815 |
| 3 | 2.408 | 3746.382 | 0.046 | 93098.907 | 226090.204 | 0.077 | 31.356 |
| median | 2.105 | **3811** | 0.029 | **106919** | **226090** | 0.102 | 31.356 |

So the three phases the budget is actually spent on, as a share of the median run:

1. **`COPY` into staging — 3.8 s, 1.2%.** Parse, normalise, HMAC, mint a v7, validate and render
   1,000,000 rows, and stream them in: 263,000 rows/sec.
2. **The upsert into `contacts` — 106.9 s, 32.6%.**
3. **Memberships and tags — 217.0 s, 66.2%.**

The erasure sweep and the `COMMIT` are noise: 0.1 ms and 31 ms.

### Which phase dominates, before anything is changed

**Memberships and tags.** It is two thirds of the whole import and about twice the contacts upsert.
This is the M1b supersede-cascade lesson again — there, 315.6 ms of 326.8 ms went to an FK cascade
and not to the index everyone suspected. Here the phase everybody would reach for first is the
contacts upsert, and it is the smaller half.

**Naming the phase is not the same as the budget being reachable by fixing it.** If the memberships
statement cost nothing at all, the pipeline would still be about 111 s against a 120 s budget on
this box — inside it by 7%, which is not a margin. Both write phases have to come down.

### Where the time goes inside each statement

From `auto_explain` with `log_analyze` (the test turns this on under `NM_PERF_EXPLAIN`, and never
during a timing run, because instrumenting every node changes the figures it is meant to produce).
These plans are from a **200,000-row** instrumented run, so the absolute times are inflated and only
the shares are the finding.

**The contacts upsert: 92.8% of it is the `INSERT` itself.**

```
Result  (actual time=9204.315..9204.326 rows=1 loops=1)
  CTE picked
    ->  Unique  (actual time=105.879..150.173 rows=200000)
          ->  Sort  (actual time=105.877..133.245 rows=200000)
                Sort Key: …email, …row_number DESC
                Sort Method: external merge  Disk: 36856kB
                ->  Seq Scan on import_staging_…  (actual time=0.008..21.684 rows=200000)
  CTE live
    ->  Hash Right Anti Join  (actual time=81.642..158.708 rows=200000)
          ->  Seq Scan on suppressions  (actual time=0.026..0.026 rows=0)
  CTE erased
    ->  Hash Anti Join  (actual time=347.611..347.613 rows=0)
  ->  Insert on contacts  (actual time=0.767..8542.110 rows=200000)
        Conflict Resolution: UPDATE
        Conflict Arbiter Indexes: contacts_workspace_id_email_key
```

`contacts` carries seven indexes, one of them a GIN trigram index
(`contacts_search` on `email || ' ' || first_name || ' ' || last_name`). That index maintenance,
plus the heap and the WAL for it, is the 8.5 s.

**Memberships and tags: the three `INSERT`s, and then an equal amount of time in no plan node at
all.** The statement took 33,580 ms; its plan nodes sum to roughly 17,200 ms:

```
->  Insert on list_memberships  (actual time=6.262..4290.752 rows=200000)
->  Insert on contact_tags     (actual time=423.260..9928.541 rows=400000)
->  Insert on tags             (actual time=284.029..285.476 rows=51)
->  HashAggregate (the `wanted` union)  (actual time=997.129..1105.254 rows=200000)
      Batches: 5  Memory Usage: 8241kB  Disk Usage: 15488kB
->  Aggregate (the `declined` count)    (actual time=1040.627..1040.629 rows=1)
->  Aggregate (the `suppressed` count)  (actual time=15.327..15.328 rows=1)
```

**The missing ~16 s is referential integrity.** `list_memberships` has a foreign key to
`contacts(workspace_id, id)` and to `lists`; `contact_tags` has one to `contacts` and to `tags`.
Postgres fires an RI check **once per inserted referencing row**, as an after-row trigger that
`auto_explain` does not attribute to any node. Turning on `auto_explain.log_nested_statements`
makes them visible, and they arrive in a flood — enough to rotate the four plans that were wanted
out of the container's log, which is why the test does not set it. What survived that rotation was
44,506 `SELECT 1 FROM ONLY "public"."contacts" x WHERE "workspace_id" … FOR KEY SHARE` plans and
44,507 of the same against `lists`, all from this one statement.

So the set-based design did not remove the per-row cost that §7.2 wanted gone — for the
memberships and the tags it moved it inside the server, where it is one RI probe and one index
insert per row rather than one client round trip per row. That is a very large win (three million
rows written in 217 s is not reachable row-at-a-time from Rust), and it is still not two minutes.

### `spec.md` §7.2 is right about the half it names and silent about the half that costs

The `COPY` half is **spectacularly** inside budget: 3.8 s for a million rows, 1.2% of the import,
and it carries all the per-row Rust work there is — `normalize_email`, `suppression_hmac`,
`Uuid::now_v7`, validation and rendering. Nothing in this milestone's Rust is near the budget.

The budget is spent entirely on the write side, in index maintenance and RI checks that are per-row
whatever shape the statement around them has. **A future optimisation aimed at the `COPY`, at the
parser, or at the Rust is aimed at 1.2% of the problem.**

Worth saying: the same run in a **debug** build takes 377.7 s, of which the `COPY` is 20.7 s rather
than 3.8 s. The SQL phases are within noise of the release figures, as they must be. Anybody
repeating this has to say which build they used, or the `COPY` number is off by 5.4×.

### Three things the plans say, recorded and not acted on

1. **The staging table has no index, and the plan does want one.** Both big statements re-derive
   `picked` as `select distinct on (email) * from <staging> order by email, row_number desc`, and
   both get `Seq Scan → Sort (Sort Method: external merge, Disk: 36856kB / 29248kB at 200k rows) →
   Unique`. At a million rows that is roughly 180 MB spilled to disk, twice per import. An index on
   `(email, row_number desc)` would remove both sorts. **It is not the fix**: those sorts are about
   150 ms of a 12,000 ms statement, 1–2%, and building the index costs its own sort. Recorded
   because the brief asked whether the plan wants one, and the plan does.

2. **The second `distinct on (email)` pass and the second anti-join against `suppressions` are not
   the problem.** This is the question nobody had measured. Together, re-deriving `picked` and
   `live` in the memberships statement costs about 165 ms + 26 ms at 200,000 rows, against a 33,580
   ms statement — under 1%. Both stay. The thing to note is that this was measured with an **empty**
   `suppressions` table; see below.

3. **The anti-join's predicate has no index that can serve it, and that is untested at scale.**
   `where reason = 'gdpr_erasure' and email_hmac = s.email_hmac and (workspace_id = $1 or
   workspace_id is null)` carries no `lifted_at is null`, and **both** indexes on
   `suppressions.email_hmac` (`suppressions_lookup_hmac`, `suppressions_live`) are partial on
   `lifted_at is null`, which this predicate does not imply. The planner therefore cannot use
   either, and the plans above show `Seq Scan on suppressions`. With zero suppression rows it costs
   nothing, which is exactly why this run does not test it. **A workspace with a large
   `suppressions` table is unmeasured, and this is where it would hurt.**

### What was not measured

- **`POST /imports/{id}/start`.** The multipart upload, the advisory lock, the rate limit
  (`IMPORT_PER_MINUTE = 60`), the row cap (`MAX_IMPORT_ROWS = 1_000_000`) and the audit rows are
  all outside these figures, by the choice argued above.
- **The `fail on duplicate` pre-flight**, whose `order by s.row_number limit 1` sorts its whole
  match set over an unindexed staging table to return one row. This run is `update existing`, so
  the pre-flight never ran. It is a known shape worth a measurement of its own.
- **A populated `suppressions` table**, per point 3 above.
- **A file without tags or lists.** It would be faster, and the plans above already attribute the
  memberships phase to its three `INSERT`s and their RI checks, so a second file shape would
  confirm rather than reveal.
- **The reference box.** Everything here is a 3.8 GB VM with 128 MB of `shared_buffers`. More
  memory would keep the external merge sorts and the `HashAggregate` spill in RAM and would widen
  the page cache under the seven `contacts` indexes. **It would not remove an RI probe or an index
  insert per row**, which is where the budget goes, so the reference box should be expected to be
  faster and not to be 2.7× faster. That remains a prediction until somebody runs it there.
