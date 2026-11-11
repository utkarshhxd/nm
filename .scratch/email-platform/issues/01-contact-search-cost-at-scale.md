Status: needs-triage

# Contact search cost at ten million rows

Found in M1a task 4 (`GET /contacts`, commit `11778f3`). Two separate cost problems in the
search path. Neither is measured yet.

## This is a cost problem, not a tenancy breach

No other workspace's data can reach a response. The `workspace_id = $1` predicate in
`contacts::list` is unconditional and is applied before every other predicate. A test in
`tests/contacts_list.rs` (`paging_never_repeats_or_skips_a_row`) seeds a contact in a second
workspace and asserts it is absent from every page. Do not read problem 1 as a data leak.

## Problem 1: `contacts_search` has no `workspace_id`

The trigram GIN index is on `(email || ' ' || first_name || ' ' || last_name)` alone. EXPLAIN shows a
`Bitmap Index Scan on contacts_search` with the workspace as a heap `Filter` afterwards. At scale
the bitmap hands up every matching heap row of every tenant, and the filter discards the other
tenants' rows. A term common across the table costs work proportional to all tenants, not to the
caller's workspace.

## Problem 2: with `q` set, the keyset order is not served by an index

The trigram bitmap scan cannot return rows in `(email, id)` or `id` order. So when `q` is set,
Postgres sorts the whole match set, then applies `limit`, on every page. `sort=email&q=<a common
domain>` at ten million rows sorts millions of rows per request, and again for each following
page. The keyset predicate does not help here: it narrows what is returned, not what is sorted.

## Proposed fix (one fix for both)

Enable `btree_gin` and replace the search index with a composite GIN index on
`(workspace_id, <the search expression> gin_trgm_ops)`. That scopes the scan to one workspace
(problem 1) and shrinks the sorted set to that workspace's matches. Whether it is enough for
problem 2, or whether it needs more (for example a lower bound on result size, or a different
plan for very common terms), is for the load test to say. It needs a new migration. Migration
`0011` is applied everywhere and must not be edited.

## Why this is not fixed yet

The 150 ms p95 target at ten million rows has not been measured by anyone. The only check so far
is task 4's EXPLAIN on 200k rows in a single workspace, which shows index use and says nothing
about multi-tenant cost or sort cost. The fix belongs with a 10M-row, multi-workspace load test
that can measure it, not a guess made now. The load test should cover: `q` with a rare term, `q`
with a very common term, each with `sort=created` and `sort=email`, deep pages, and many
workspaces of mixed size.

## Label

`needs-triage`: the direction is clear but the fix is not fully specified until measured. Move to
`ready-for-agent` once the load test exists.

## Comments
