Status: needs-triage

# The dev database cannot be migrated, because 0023 was edited after it was applied

Found during M2a Task 3, by an implementer that needed `0028` applied so the `sqlx::query!` macros
could type-check against it.

## What happens

```
$ bash scripts/dev.sh cargo run -- migrate
migration 23 modified
```

`sqlx` refuses to run any migration against the `nm` database, because the checksum it stores for
`0023_forms_and_jobs.sql` no longer matches the file.

## Why

`d27656d` ("fix: the jobs contract says how a row is claimed") **edited `0023` after it had already
been applied** to the dev database. The edit was the right call on its own terms — it added the
claim protocol as a stated requirement on the consumer, because nothing had warned a worker off a
plain `select … where run_after <= now()` followed by a separate update, and that race sends one
person the same confirmation email twice.

The lesson was learned immediately afterwards and written into `0026`'s table comment, which says
in terms that **`0023` cannot be amended: its checksum has been taken**, and makes the comment on
`jobs` the authority instead. So the project already knows; what nobody checked is that the dev
database was left holding the old checksum.

## Why the test suite is green anyway

`sqlx::test` creates a private database per test and runs every migration into it from nothing, so
it never compares against a stored checksum. **A fully green suite is compatible with a `migrate`
that cannot run at all**, which is the part worth remembering.

## Blast radius

Today it is a development inconvenience: the workaround is to apply a migration by hand, which is
what Task 3 did, or to recreate the database.

It stops being an inconvenience the moment there is a deployment whose database predates
`d27656d` — `nm migrate` refuses, and `auto_migrate` is on by default, so the server will not come
up. Nothing is deployed yet, which is the only reason this is filed rather than fixed.

## Options

1. **Recreate the dev database.** Cheapest, loses whatever is in it — which is why this was not
   done unilaterally; the data is the user's.
2. **Update the stored checksum in place** for `0023` in `_sqlx_migrations`. Honest only because
   the edit was comment-only; it would be dishonest for an edit that changed a statement.
3. **Leave it and document the workaround.** What is happening by default today.

Option 2 is probably right for any database that exists before the first deployment, and the
decision should be taken once, deliberately, rather than per-developer.

## Related

- `0026_jobs_references_and_consent_guards.sql` — the table comment that records the rule this
  broke, written after it was broken.
- M2a's `0028` was applied by hand for the same reason.
