Status: needs-triage

# Cursor paging re-walks every earlier page inside the index

Found while measuring `0024`'s index during M1d Task 11. **Not introduced by that route** — it is
the shape every paged route in this tree uses, and it is recorded here so the next person to touch
the pagination helper knows.

## The mechanism

The cursor predicate is written as:

```sql
($3::uuid is null or c.id < $3)
```

That `or` makes the whole expression non-sargable: Postgres cannot turn it into an index
condition, because the first branch does not reference the indexed column. So the index is still
used for ordering and for the equality columns, but the cursor becomes a **filter applied after
rows are read**. Page N therefore walks the entries for pages 1 through N−1 inside the index and
discards them.

Each page is still an index scan, so the cost is small and bounded by the rows that *match the
equality columns* rather than by the table. For the consent route that bound is one contact's own
history. For a route whose equality columns are less selective, the bound is larger.

Confirmed in the same `explain (analyze, buffers)` session that measured `0024`: the index
condition covers `(workspace_id, contact_id)` and the cursor appears separately rather than as
part of the index condition.

## Where it applies

Every route using the shared cursor helper, including `list_forms`, `list_contact_lists` and the
new consent history route. This is a property of the helper, not of any one caller.

## What a fix looks like

Split the query rather than branching inside it: a first-page statement with no cursor predicate,
and a subsequent-page statement whose predicate is a bare `c.id < $3`. The second form is
sargable and becomes a real index condition, so page N costs the same as page 1.

The cost is two statements where there is now one, in a helper that is deliberately shared. That
trade is the reason this is filed rather than fixed: it should be decided once, for the helper,
by someone looking at every caller — not opportunistically by whichever route happens to be
measured next.

## Why it has not bitten

Every paged route here is scoped by a workspace and usually by a second equality column, and no
deployment has the row counts where a deep page would be noticed. It is a scaling property, not a
present defect.

## Related

- `issues/24-constraint-changes-on-consent-log-will-block.md` — the other `consent_log` scaling
  note.
- M1c recorded a comparable "measured, understood, not acted on" finding about `is_erased`'s
  sequential scan, which M1d Task 9 then had to fix before a public route could call it. The same
  pattern applies here: the first route whose equality columns are weakly selective will make this
  urgent.
