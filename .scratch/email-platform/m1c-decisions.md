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

**On a 3.8 GB WSL2 VM with stock Postgres config — not the reference box of `spec.md:448` — a 1M-row
import takes 323–468 s against the 120 s budget: between 2.7× and 3.9× over. The budget is missed
in every configuration measured, and the best one measured is still 2.69× over.**

That sentence carries its machine on purpose. `spec.md:448` opens §9 with *"The reference box is a
single VM with 8 vCPU and 32 GB RAM, running both `nm all` and Postgres"*, and that sentence scopes
every target below it including the `< 2 min` at `:459`. **Nothing below has been measured on the
reference box.** What follows is sound about this laptop and is evidence, not proof, about the
product.

The measurement is `tests/import_perf.rs`, which is `#[ignore]` by default. Nothing was tuned to
make a number look better; the configuration probe below was run *after* the figures were recorded,
and is reported as a separate arm rather than folded into the headline.

### The machine

| | |
|---|---|
| Host CPU | AMD Ryzen 7 250 (Radeon 780M), 8 cores / 16 threads, 3.30 GHz max |
| Host RAM | 31.3 GB |
| Host OS | Windows 11 Home Single Language 10.0.26200, Lenovo 83JG |
| Storage | Samsung MZAL81T0HFLB-00BL2, NVMe SSD, 954 GB (704 GB free at measurement) |
| Containers | Docker Desktop on WSL2. The VM is capped to **4 GB** by `~/.wslconfig` (`[wsl2] memory=4GB`, `autoMemoryReclaim=dropCache`). The container sees 16 CPUs and `MemTotal: 4007580 kB`, **shared with every other container on the host.** |
| Postgres | `postgres:16-alpine`, **PostgreSQL 16.15** on `x86_64-pc-linux-musl`. Data on a docker volume, i.e. the WSL2 ext4 filesystem inside the vhdx on the NVMe above. |
| Stock config | `shared_buffers = 128 MB`, `work_mem = 4 MB`, `maintenance_work_mem = 64 MB`, `max_wal_size = 1 GB`, `checkpoint_timeout = 5 min`, `effective_cache_size = 4 GB`, `random_page_cost = 4`, `max_parallel_workers = 8`, `fsync`/`synchronous_commit`/`full_page_writes` on, `data_checksums` off. |
| Load | Nothing else was building or testing. **Four containers unrelated to this project** were running on the host, and all four sat at 0.00% CPU for the whole measurement, as did this project's own idle dev server — five idle containers in total. The only non-idle ones were Postgres and the test. **Reproducing this needs the host otherwise idle, not these particular containers**, so they are not named here: a decision record committed to this repo is not the place to list what else is on somebody's laptop. |

### What was measured, and what it does not cover

**The two pipeline stages, called directly — not `POST /imports/{id}/start`.** The handler also does
the multipart upload, the authorisation, the advisory lock, the `imports` row updates and two audit
rows, and a single wall-clock figure over all of that cannot say which phase moved. The `COMMIT` is
timed separately below because it is the one part of the handler's extra database work that makes
the import durable.

**The direction of that exclusion matters and is favourable.** Every excluded item is *additive*:
the handler can only be slower than these figures, never faster. So these are a **lower bound**, the
headline is **optimistic**, and a "missed" verdict drawn from an optimistic figure is the safe
direction. One further difference: production wraps the upsert in a `SAVEPOINT`
(`src/contacts/import/routes.rs:1035`) and this test calls it on the outer transaction. That is a
subtransaction's worth of overhead, far below the run-to-run noise, and it is still a difference.

**The file is the shape an operator uploads**, six columns:
`email,first_name,last_name,timezone,tags,lists` — 1,000,000 rows, 65.3 MiB, every row carrying two
tags (`cohort-<i mod 50>` and `vip`) and the name of the list the import was also pointed at. So the
run writes **1,000,000 contacts, 1,000,000 list memberships and 2,000,000 contact tags**, and every
figure below is for that. A file of addresses and names alone writes none of the last two and would
be faster.

Three fixture properties that bound what the numbers mean:

- **Every row carries `vip`**, so 1,000,000 of the 2,000,000 `contact_tags` share one `tag_id` and
  therefore one index prefix in `contact_tags_by_tag (workspace_id, tag_id, contact_id)`. A file
  whose tags were evenly spread would scatter those inserts differently.
- **All 1,000,000 addresses are distinct**, so `collapsed = 0`, the `distinct on (email)` discards
  nothing, and `on conflict` never fires (`Conflicting Tuples: 0`). **The dedup path is exercised
  but never does any work.** A file with real duplicates is not measured here.
- `dedup = update existing` into an empty workspace with an empty `suppressions` table: nothing was
  updated, skipped, failed or erased.

### Every 1M-row run

All release builds. Seconds. Phases 1 and 2 are the test's own wall clock; phase 3 is server-side
(see "How the third number is taken").

| # | cluster | config | state | `COPY` | contacts | memberships + tags | `COMMIT` | **pipeline** | × budget |
|---|---|---|---|---|---|---|---|---|---|
| 1 | `nm-postgres-1` | stock | warm | 3.881 | 107.626 | 232.335 | 0.035 | **343.952** | 2.87 |
| 2 | `nm-postgres-1` | stock | warm | 3.815 | 106.919 | 217.014 | 0.021 | **327.959** | 2.73 |
| 3 | `nm-postgres-1` | stock | warm | 3.750 | 93.099 | 226.090 | 0.032 | **323.031** | 2.69 |
| 4 | probe, fresh | stock | cold | 4.844 | 169.930 | 292.608 | 0.043 | **467.554** | 3.90 |
| 5 | probe, fresh | **tuned** | cold | 4.401 | 115.728 | 262.226 | 0.032 | **382.458** | 3.19 |
| 6 | probe, fresh | **tuned** | cold | 4.273 | 99.285 | 264.503 | 0.030 | **368.168** | 3.07 |
| 7 | probe, same | **tuned** | warm | 4.647 | 106.925 | 262.101 | 0.059 | **373.788** | 3.11 |

Runs 1–3 are the headline set: three consecutive runs on the project's own Postgres. **Run 2 is the
median run and the only row that should ever be quoted as "the" figure**, because it is an actual
run:

> **run 2 (median pipeline): `COPY` 3.815 s · contacts 106.919 s · memberships and tags 217.014 s ·
> `COMMIT` 0.021 s · pipeline 327.959 s.**

A per-column median across runs 1–3 was quoted in an earlier draft of this file and was **wrong**:
it describes no run, and its cells sum to 336.9 s against a 327.96 s pipeline, which is a 103%
import. Quote run 2's row or quote all three.

### How the third number is taken

`write_memberships_and_tags` is private and runs inside `upsert_from_staging`, so the third phase
cannot be timed from outside the crate. The test sets `log_min_duration_statement = 0`, which makes
Postgres time each statement server-side, and the memberships figure is that statement's own
duration. **That figure is read out of the container's log, not printed by the test** — the exact
command is in the test's module doc comment, because a number nobody can reproduce is not a
measurement.

Per run the server-side statements sum to the client's wall clock **within 0.2 s**: 0.192 s for run
2, 0.076 s for run 1, 0.063 s for run 3. That agreement is the check that the split is real rather
than an attribution — so here are all seven statements of runs 1–3, in milliseconds, because the
sum does not come out without the small ones:

| run | `create temp table` | `copy` | error budget | contacts | memberships + tags | sweep | `COMMIT` | **server sum** | wall clock | gap |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 2.031 | 3877.774 | 0.026 | 107626.337 | 232334.636 | 0.825 | 34.587 | 343876.216 | 343952 | **0.076 s** |
| 2 | 2.105 | 3811.063 | 0.029 | 106919.283 | 217013.622 | 0.102 | 20.815 | 327767.019 | 327959 | **0.192 s** |
| 3 | 2.408 | 3746.382 | 0.046 | 93098.907 | 226090.204 | 0.077 | 31.356 | 322969.380 | 323032 | **0.063 s** |

The `create temporary table` column is 2–3 ms and is in the table because without it the sums are
short by that much and the agreement cannot be reproduced. An earlier draft dropped it.

### Which phase dominates, and the trap in saying so

**Memberships and tags is the largest phase in all seven runs.** The bands below are the full span
of the seven runs in the table above, computed run by run — not a rounded summary, and not a band
that quietly drops the runs at its edges:

| phase | min | max |
|---|---|---|
| `COPY` | 1.04% (run 4) | 1.24% (run 7) |
| contacts upsert | **26.97% (run 6)** | **36.34% (run 4)** |
| memberships and tags | **62.58% (run 4)** | **71.84% (run 6)** |

So: `COPY` **1.0–1.2%**, contacts **27.0–36.3%**, memberships and tags **62.6–71.8%**. The erasure
sweep and the `COMMIT` are noise: **0.077–0.825 ms** for the sweep and **20.8–34.6 ms** for the
`COMMIT`, across runs 1–3, which are the runs whose per-statement figures this file prints. (The
`COMMIT` wall clock for all seven is in the table above and spans 21–59 ms; the sweep is not a
column there, so its band is deliberately only the three runs shown, rather than a band quoting
runs the reader cannot check.)

(An earlier draft of this section said "63% to 72%" and "29–36%". The first contradicted its own
parenthetical — run 4 is 62.6% — and the second excluded three of the seven runs. Nothing it
concluded changes; it was the same fault as the median row above, a figure in prose disagreeing
with the table beside it, and every band in this file has now been recomputed from the seven runs.)

**But per written row it is the cheaper of the two statements, and saying "memberships dominate"
without that sends the next optimisation at the wrong one.**

| statement | rows written | seconds (range) | **µs per written row** |
|---|---|---|---|
| contacts upsert | 1,000,000 | 93.1 – 169.9 | **93 – 170** |
| memberships and tags | 3,000,000 | 217.0 – 292.6 | **72 – 98** |

It dominates by **row count**, not by inefficiency: it writes three rows for every one the contacts
statement writes, and it writes each of them more cheaply by a factor that runs **1.13× to 1.74×
across the seven runs, median 1.32×** — a single "1.3×" is that median and not the spread. This is the M1b
supersede-cascade lesson recurring one level down — there, 315.6 ms of 326.8 ms went to an FK
cascade and not to the index everyone suspected; here, the phase with the big share is the
efficient one. **An optimisation that makes the memberships statement faster per row is working on
the better of the two; the one that pays is writing fewer rows, or making the row cheaper in both.**

**And naming the phase is not the same as the budget being reachable by fixing it.** If the
memberships statement cost nothing at all, run 2 would still be ~111 s against 120 s — inside by
7%, which is not a margin. Both write phases have to come down.

### Where the time goes: measured, not inferred

From `auto_explain` with `log_analyze`, `log_buffers` and `log_triggers`, on a **200,000-row**
release run (the test turns this on under `NM_PERF_EXPLAIN` and never during a timing run, because
instrumenting every node changes the figures it is meant to produce). Absolute times are inflated by
the instrumentation; the shares are the finding.

**Referential integrity is 42.7% of the upsert, and it is measured per constraint.** An earlier
draft of this file inferred ~16 s by subtracting plan nodes from the statement duration. That
subtraction was not sound — a `ModifyTable` node's `actual time` already includes its children, so
summing top-level totals double-counts — and `auto_explain.log_triggers` measures the thing
directly, with none of the nested-statement flood that makes `log_nested_statements` unusable here:

```
-- the contacts statement, 19788.484 ms total
Trigger for constraint contacts_workspace_id_fkey on contacts: time=3540.799 calls=200000

-- the memberships and tags statement, 46952.583 ms total
Trigger for constraint list_memberships_workspace_id_contact_id_fkey …: time=4655.114 calls=200000
Trigger for constraint list_memberships_workspace_id_list_id_fkey    …: time=3758.179 calls=200000
Trigger for constraint tags_workspace_id_fkey on tags                 : time=1.501    calls=51
Trigger for constraint contact_tags_workspace_id_contact_id_fkey      …: time=9497.601 calls=400000
Trigger for constraint contact_tags_workspace_id_tag_id_fkey          …: time=7073.429 calls=400000
```

- Memberships statement: **24,985.8 ms of 46,952.6 ms — 53.2%** is RI triggers, across 1,200,051
  calls. The four high-volume constraints run **17.7–23.7 µs per call** (17.68, 18.79, 23.28,
  23.74). The fifth, `tags_workspace_id_fkey`, is 29.4 µs — but that is 1.501 ms over **51 calls**,
  and it is inside the 1,200,051 while being outside the band, so both facts are worth saying.
  **Why it is higher is not established here.** The likeliest explanation is the one-off cost of
  planning the RI check on its first call, amortised over 51 rows instead of 400,000; timer
  granularity at that count is a second candidate. An earlier draft asserted the second as though
  it had been read off something, which it had not. Nothing depends on which it is: 1.5 ms of a
  46,953 ms statement changes no conclusion either way.
- Contacts statement: **3,540.8 ms of 19,788.5 ms — 17.9%**, which also answers a thing the earlier
  draft missed entirely: `contacts` itself has a foreign key to `workspaces`, so the contacts
  statement pays a per-row RI check too.
- Across both: **28,526.6 ms of 66,741.1 ms, 42.7% of the upsert.**

**That 42.7% is measured at 200,000 rows under instrumentation, and is quoted as a property of
"the upsert" on the assumption it holds at a million.** The assumption is reasonable and it is not
tested: scaling the two statements from this instrumented 200k run to the uninstrumented 1M run 2
gives 5.4× for contacts (19.8 s → 106.9 s) and 4.6× for memberships (47.0 s → 217.0 s) across a 5×
row count, which is roughly linear and conflates instrumentation overhead with scaling. **Nobody
has run `log_triggers` at 1M**, so the share could move; nothing here depends on which way.

**So the set-based design did not remove the per-row cost that §7.2 wanted gone. It moved it inside
the server**, where it is one RI probe plus one index insert per row rather than one client round
trip per row. That is a very large win — three million rows written in 217 s is not reachable
row-at-a-time from Rust — and it is still not two minutes.

The rest of the memberships statement is its three inserts: `Insert on contact_tags` 13,777 ms
(400k rows), `Insert on list_memberships` 6,723 ms (200k rows), `Insert on tags` 285 ms (51 rows —
50 `cohort-N` plus `vip`, which is the fixture's distinct tag count). These node totals **include
their children and must not be summed against the statement**; they are listed to rank the inserts,
not to decompose the time.

**The load is not cache-starved. It is CPU and write-back bound.** This is what `log_buffers`
settles:

| node | `shared hit` | `shared read` | `dirtied` | `written` |
|---|---|---|---|---|
| `Insert on contacts` (200k) | 4,879,190 | **6** | 14,440 | 14,436 |
| `Insert on list_memberships` (200k) | 2,130,358 | **4** | 7,200 | 9,022 |
| `Insert on contact_tags` (400k) | 4,339,992 | **2** | 11,704 | 15,558 |

**Twelve shared reads across 11.3 million buffer accesses.** Essentially nothing came off disk, so
"too little cache to read with" is not the explanation. What *is* visible is the other side: the
backends wrote 39,016 buffers (305 MB) themselves, which is a backend evicting its own dirty pages
because 128 MB of `shared_buffers` cannot hold the working set. **That working set is eleven
indexes** — seven on `contacts`, two on `list_memberships`, two on `contact_tags` — or fourteen
counting the three on `tags`, which this file's fixture barely touches (51 rows). That is a
write-back problem, and it is what the configuration probe went after.

### The configuration probe: worth 18%, and it is not the explanation

The earlier draft of this file *argued* that more memory would not help, on the grounds that it
"would not remove an RI probe or an index insert per row". That argument was wrong-headed: nobody
claims the operations vanish — the question is their **unit cost**, which cache residency and
checkpoint policy set. And the stock config table above names the likeliest culprit without
discussing it: `max_wal_size = 1 GB` with `full_page_writes = on`. So it was measured.

**A paired A/B, back to back, on a throwaway container (`nm-postgres-1` was not touched): same
image, same host, same fresh-volume lifecycle, only four settings differ.**

| | stock (run 4) | tuned (run 5) | change |
|---|---|---|---|
| `shared_buffers` | 128 MB | **1 GB** | |
| `work_mem` | 4 MB | **64 MB** | |
| `max_wal_size` | 1 GB | **16 GB** | |
| `checkpoint_timeout` | 5 min | **30 min** | |
| **pipeline** | **467.554 s** | **382.458 s** | **−85.1 s, −18.2%** |
| contacts | 169.930 s | 115.728 s | −31.9% |
| memberships + tags | 292.608 s | 262.226 s | −10.4% |
| checkpoints during the run | **7, all `reason: wal`** | **0** | |
| WAL since cluster init, read after the run (a proxy — see below) | **4,125 MB** | **3,106 MB** | **−24.7%** |

**How much of a sample that is, said plainly.** The A/B delta is **one stock run against one tuned
run** — runs 4 and 5. The tuned arm has three runs (5, 6, 7) and replicates at 374.8 s ± 2%; **the
stock arm has none to replicate against.** And this file's own between-container uncertainty is
±20% (below), which is **larger than the 18.2% the probe measures**. The pairing is the defence —
back to back, same image, same host, same fresh-volume lifecycle, four settings apart, so the
unattributed between-container term should be common to both arms and cancel — but a paired design
is an argument that the noise cancelled, not a measurement that it did. **Treat 18.2% as the right
order of magnitude and not as a figure to two significant digits.** The conclusion does not rest on
it: the gap needs 63–74%, and no reading of one run against one run closes that.

**Where the checkpoint and WAL figures come from.** The same rule applies to these as to the
memberships number — a figure nobody can reproduce is not a measurement — so here are the commands
and what they printed. The probe container has since been deleted, so these are quoted from the
capture rather than re-extractable; the commands are recorded so that the next probe produces
comparable ones.

```text
$ docker logs nm-postgres-probe 2>&1 | grep -E "checkpoint starting" \
    | sed -E 's/.*checkpoint starting: /  /' | sort | uniq -c
# stock arm                        # tuned arm
      1   immediate force wait           1   immediate force wait
      1   shutdown immediate             1   shutdown immediate
      7   wal                            (no `wal` line at all)

$ docker logs nm-postgres-probe 2>&1 | grep -E "checkpoint complete" | tail -4
# stock arm. `tail -4` prints the LAST FOUR of the nine, so these are not a time-bounded
# subset of the seven `wal` ones -- they are simply the last to complete, and the fourth
# (omitted here) is the `shutdown immediate` that wrote 2 buffers. Of the seven `wal`
# checkpoints, only these three were captured; the earlier four scrolled past `tail -4`.
16:09:36 … wrote 7201 buffers (44.0%); … write=41.091 s,  sync=1.872 s, total=43.666 s
16:10:11 … wrote 4346 buffers (26.5%); … write=29.850 s,  sync=0.136 s, total=30.377 s
16:13:07 … wrote 5859 buffers (35.8%); … write=172.321 s, sync=0.001 s, total=172.629 s

$ docker exec nm-postgres-probe psql -U nm -d nm -tAc \
    "select pg_size_pretty(pg_current_wal_lsn() - '0/0'::pg_lsn)"
4125 MB   # stock arm
3106 MB   # tuned arm
```

**That WAL figure is WAL since the cluster was initialised, not since the import** — there is no
cheap per-statement WAL counter, and this is the proxy. **It was read once per arm, after the test
process exited and before the container was removed; there is no before-reading to subtract.** It
is a fair proxy only because each arm ran on a cluster created minutes earlier whose only other
writers were `initdb`, the migrations and one `#[sqlx::test]` database; the import is the
overwhelming majority of it. It is still a proxy, and the −24.7% it reports is the difference
between two proxies.

The mechanism is visible and real. At `max_wal_size = 1 GB` an import that writes about 4.1 GB of
WAL forces seven checkpoints; each one makes the next touch of every page write a full 8 kB image,
which brings the next checkpoint sooner. Raising the ceiling removed all seven and took about a
quarter of the WAL volume with it.

**It is still a design finding and not a config finding.** The tuning buys 18.2%; the budget needs
**63–74%**. The tuned arm is 3.19× over, and runs 6 and 7 at 368.2 s and 373.8 s put the tuned mean
at 374.8 s.

### The honest uncertainty: it is systematic, not random

Three consecutive runs agreeing to 6.5% says very little, and the earlier draft over-read it.
Monotone order across three points happens in a third of random orderings, and if a warming cache
*were* the cause, the series would not have converged and all three runs would overstate the steady
state — the trend was used to dismiss disagreement and never followed to its own conclusion.

What the probe showed is worse and more useful: **the same stock configuration on the same laptop
differs by 36–45% between containers.** The fresh container's 467.554 s against each of the three
runs on `nm-postgres-1` is +35.9% (run 1, 343.952 s), +42.6% (run 2, 327.959 s) and **+44.7%**
(run 3, 323.031 s). **The band is quoted whole on purpose**: this is the file's headline
uncertainty figure, and quoting a headline uncertainty at its most favourable pairing is the one
framing it must not have.

A second probe tested whether that was cache warmth, by running the tuned config twice in one
container, cold then warm: **368.2 s then 373.8 s. Warmth is worth nothing measurable.** So the
36–45% is something else — a different volume, a different cluster age, host thermal state after an
hour of continuous load — and **it is not attributed.**

The conclusion to draw is about the *kind* of error, not its size. Within one container and one
session the figure is repeatable: runs 6 and 7 are 1.5% apart, and runs 1–3 are 6.5% apart — so
"repeatable" means single figures of per cent, and the 6.5% that an earlier draft called close
agreement is the loose end of that, not the tight one. **Between containers it moves by 36–45%, in
a direction nothing here controls.** Any single number from this file is good to roughly ±20%, and
the binary question survives that easily: **worst observed 3.90×, best observed 2.69×, and the
budget needs 2.69× to become 1.0×.**

### What `spec.md` §7.2 gets right, and what it is silent about

The `COPY` half is **spectacularly** inside budget: **3.75–4.84 s for a million rows**, 1.0–1.2% of
the import, and it carries all the per-row Rust there is — `normalize_email`, `suppression_hmac`,
`Uuid::now_v7`, validation and rendering — at **206,000 to 267,000 rows/sec** across the seven runs
(267k in run 3, 206k in run 4). Nothing in this milestone's Rust is anywhere near the budget.
§7.2's claim about row-at-a-time inserts is almost certainly right, and this measurement neither
proves nor needs it.

What §7.2 is silent about is that the write side has a per-row cost of its own which being
set-based does not remove. **An optimisation aimed at the `COPY`, at the parser, or at the Rust is
aimed at 1.2% of the problem.**

**The debug build, with its phases, so the claim can be checked rather than taken.** One run, on
`nm-postgres-1`, the same cluster as runs 1–3:

| build | `COPY` | contacts | memberships + tags | `COMMIT` | pipeline |
|---|---|---|---|---|---|
| debug (n=1) | **20.650** | 125.901 | 231.048 | 0.022 | **377.726** |
| release runs 1–3, same cluster | 3.750 – 3.881 | 93.099 – 107.626 | 217.014 – 232.335 | 0.021 – 0.035 | 323.031 – 343.952 |

The `COPY` moves by **5.4×** (20.650 / 3.815), which is the whole point: it is the only phase with
per-row Rust in it. The SQL phases do **not** move the same way — but "within noise" would be too
strong for the contacts figure, and an earlier draft said it anyway. **125.901 s is 17% above the
highest of the three release runs on that cluster** (107.626 s); it sits inside the full seven-run
release range (93.1–169.9 s), and memberships at 231.048 s is within 0.6% of run 1's 232.335 s.
With n=1 in debug, the fair statement is that the SQL phases are not distinguishable from the
release spread, and the `COPY` plainly is. **Anybody repeating this has to say which build they
used, or the `COPY` number is off by 5.4×.**

### Findings recorded and not acted on

   **Both findings below take every number from one run** — the 200,000-row instrumented release
   run that also produced the trigger and buffer figures above. An earlier draft divided node
   timings from a *superseded* 200k run by statement durations from this one, which is a ratio of
   two different runs wearing one percentage sign. Nothing it concluded moves; the arithmetic is
   now traceable to a single plan.

1. **The staging table has no index, and the plan does want one.** Both big statements re-derive
   `picked` as `select distinct on (email) * from <staging> order by email, row_number desc`, and
   both get `Seq Scan → Sort (Sort Method: external merge) → Unique`:

   | statement | `Unique` ends at | spill | statement duration | share |
   |---|---|---|---|---|
   | contacts upsert | 173.046 ms | `Disk: 36856kB` | 19,788.484 ms | **0.87%** |
   | memberships and tags | 206.793 ms | `Disk: 29248kB` | 46,952.583 ms | **0.44%** |

   At 200,000 rows that is **36.0 MiB and 28.6 MiB** spilled (`Disk:` reports kibibytes: 36,856
   and 29,248 of them); **scaling linearly to a million rows gives roughly 180 MiB and 143 MiB,
   once each per import — an extrapolation, not a measurement.** An index on `(email, row_number desc)` would remove both sorts. **It is not the
   fix**: under 1% of either statement, and building the index costs its own sort. Recorded
   because the question was asked and the plan answers it.

2. **The second `distinct on (email)` pass and the second anti-join against `suppressions` are not
   the problem.** This is the question nobody had measured. In the memberships statement, from the
   same plan as finding 1: the `picked` `Unique` ends at **206.793 ms** and the `live`
   `Nested Loop Anti Join` wrapping it ends at **236.608 ms**, so the second pass and the second
   anti-join together cost **236.6 ms of a 46,952.583 ms statement — 0.50%**, of which the
   anti-join itself is the 29.8 ms difference. Both stay. Measured with an **empty**
   `suppressions` table; see the next point.

3. **The erasure anti-join has no index that can serve it, and one word would fix that.** The
   predicate is

   ```sql
   where reason = 'gdpr_erasure' and email_hmac = s.email_hmac
     and (workspace_id = $1 or workspace_id is null)
   ```

   and **neither** index on `suppressions.email_hmac` can serve it, for two independent reasons:

   - `suppressions_lookup_hmac (email_hmac) WHERE lifted_at IS NULL` is partial on a condition this
     predicate does not state and so cannot imply.
   - `suppressions_live (workspace_id, scope, email_hmac, domain) WHERE lifted_at IS NULL` has
     `email_hmac` **third**, behind a `scope` the predicate never constrains and a `workspace_id`
     it only reaches through an `OR` — so it could not serve this lookup even if it were not
     partial.

   The plans confirm it: `Seq Scan on suppressions`. With zero suppression rows that costs nothing,
   which is exactly why this run does not test it. **A workspace with a large `suppressions` table
   is unmeasured, and this is where it would hurt.**

   **And the cheap fix is available**, which is why this is worth more than a note. A
   `gdpr_erasure` suppression **cannot be lifted**: `src/suppressions/mod.rs:420-422` refuses it
   with `erasure-not-liftable`, and that is the only statement in the tree that ever sets
   `lifted_at`. So for these rows `lifted_at is null` is **always true**, and adding it to the
   anti-join is semantically a no-op that makes `suppressions_lookup_hmac` usable. Not done here —
   this milestone's task was to measure, not to change `src/` — but it is a one-line change with a
   measured reason.

   (The same reasoning disposes of a correctness question raised in review: the predicate's
   omission of `lifted_at is null` does **not** mean a lifted tombstone still blocks re-creation,
   because a `gdpr_erasure` tombstone can never become lifted. The importer's predicate matches
   `suppressions::is_erased` at `src/suppressions/mod.rs:221-223` exactly, which is the right
   invariant. Nothing to file.)

4. **`work_mem = 4 MB` keeps the GIN index in small merges.** `contacts_search` is a GIN trigram
   index, and its `fastupdate` pending list is flushed against `work_mem`. The probe raised
   `work_mem` to 64 MB as part of a bundle, so its individual contribution to the 18.2% is **not
   isolated**. Named because it is the next thing to vary, not because it is measured.

### What was not measured

- **`POST /imports/{id}/start`.** Excluded by the choice argued above; the bias is additive, so
  these figures are a lower bound.
- **The reference box of `spec.md:448`.** Everything here is a 3.8 GB VM. The probe shows the
  configuration axis is worth about 18% on this hardware; what 8 vCPU and 32 GB are worth is
  **unknown within a factor of several, and the configuration alone could account for a large part
  of it.** The earlier draft asserted a direction here on an argument that did not survive contact
  with the probe; it is replaced by this sentence, and the honest answer is that nobody has run it
  there.
- **The `fail on duplicate` pre-flight**, whose `order by s.row_number limit 1` sorts its whole
  match set over an unindexed staging table to return one row. This run is `update existing`, so
  the pre-flight never ran.
- **A populated `suppressions` table**, per finding 3.
- **A file with real duplicate addresses**, so the `on conflict … do update` arm and `collapsed`
  are exercised at zero.
- **A file without tags or lists.** It would be faster; the trigger figures already attribute the
  memberships phase, so a second file shape would confirm rather than reveal.
