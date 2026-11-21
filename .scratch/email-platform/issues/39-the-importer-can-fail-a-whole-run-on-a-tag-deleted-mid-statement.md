Status: needs-triage

# The importer can fail a whole run on a tag deleted mid-statement

Found during M1d's lock-order work, reproduced on PostgreSQL 16.15, and **deliberately not fixed
there** because the fix that worked for its sibling does not transfer.

## What happens

`contacts::import::staging::write_memberships_and_tags` resolves the tags a file names through a
`known` CTE that reads `tags` under the **statement snapshot**, while the end-of-statement
referential-integrity check for `contact_tags` uses a **newer** one. A tag deleted in between is
present for the read and absent for the check:

```
ERROR:  insert or update on table "contact_tags" violates foreign key constraint
        "contact_tags_workspace_id_tag_id_fkey"
```

That is `ApiError::Internal` → **500**, and it **fails the whole import run**, not one row.

## Why the sibling's fix does not transfer

M1d closed the identical race in `contacts::selection::tag` by taking the tag row `for key share`
before the insert, so a deleted tag becomes an honest `404` instead of an internal error. Two
things stop that working here:

1. **The importer resolves tags by name, not by id.** There is no id to pre-lock; the name-to-id
   resolution *is* the read that races.
2. **It is one statement.** The CTE and the insert are a single statement, so the lock cannot be
   hoisted above the read without restructuring the query.

## The shape of a real fix

Either:

- **Resolve names to ids in a prior statement, locking as it goes** — then the insert is by id and
  the `for key share` applies exactly as it does in `selection::tag`; or
- **Make the failure partial rather than total.** A tag that vanishes mid-import is a reasonable
  thing to skip with a row-level import error, which is machinery the importer already has. Failing
  the entire run because one tag was deleted is the disproportionate part.

The second is probably the honest one: an import names tags by name precisely because it does not
know their ids, and a name that stops resolving mid-run is ordinary rather than exceptional.

## Severity

Low likelihood, high blast radius. It needs an operator to delete a tag during an import run, and
the import is the one transaction in this product with **no bound on its duration** (`issues/22`),
so the window is as long as the run. When it fires, the whole import fails.

## Related

- `docs/adr/0005-one-lock-order-for-every-path.md` — records this as a known open gap, with the
  reason the pre-lock does not transfer.
- `issues/22` — the import run is synchronous and unbounded.
- `issues/38` — the lock order is written down and nothing enforces it.
