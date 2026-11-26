Status: resolved 2026-11-25

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

## A second instance, same shape, found during M2a

`nm_enum` — the scratch database created to run catalog queries against a schema migrated through
`0028` — now refuses `nm migrate` too:

```
migration 28 was previously applied but has been modified
```

The cause is **not** an edit to the migration's content. `0028` was written with CRLF line endings
while `.gitattributes` specifies `* text=auto eol=lf`, so the working-tree file disagreed with the
blob git stored. Normalising it to LF changed the bytes `sqlx::migrate!` hashes at compile time,
and therefore its checksum, without changing a single character of SQL.

**That is worth knowing on its own**: a migration's checksum covers its bytes, so a line-ending
normalisation, a trailing-whitespace strip, or an editor rewriting the file invisibly is enough to
lock out every database that already has it. On Windows this is easy to do by accident — a tool
that writes CRLF into a repo configured for LF produces exactly this, and `git diff` shows nothing
because the diff is normalised.

The workaround used during M2a was to apply the one `alter table` by hand so the `sqlx` macros
could compile. `sqlx::test` databases are built from nothing each run and are unaffected, which is
why the suite stayed green throughout — the same blind spot this issue already describes.

This strengthens the case for option 2 above: updating the stored checksum is the honest fix for a
change that did not alter the SQL, and both instances on this branch are exactly that.

## Diagnosis, M2b Task R1 (the fix itself is NOT yet done)

`sqlx migrate info` against the `nm` database: **only `0023` diverges**; `1-22` and `24-27` are
`installed` and match, `28-31` are simply `pending`.

- applied checksum: `ca718464559d3dd2cd59edcac6fdbc7b88355262114f702530ff0a504bd586cf504e9d624fe38ca26b6b54fb991b975d`
- local checksum:   `7f9e4a69786a627b735228cd68ed66dec2a9eb30feaa01fd77950199113bf891d71174161bd8728eaae16352b16e4c11`

Hashing every committed version of `0023` (sha384, as sqlx does) with LF and with CRLF line endings:
the applied checksum equals the **CRLF form of the current file (`d27656d`)**, and the local checksum
equals its LF form. So the two differ in line endings only: the SQL is character-for-character
identical once normalised. The cause recorded above (a comment-only edit in `d27656d`) is **not**
what happened; the applied bytes already contained that edit. The database was migrated from a
working tree that had CRLF, which `.gitattributes` (`* text=auto eol=lf`) later normalised, the same
mechanism as the `0028` instance above.

Remedy (authorised, NOT yet executed: the action was denied by the permission layer in R1): drop and recreate the `nm` database, never edit `_sqlx_migrations`.

## Resolved — 2026-11-25

Diagnosis confirmed twice, independently. The applied checksum is the sha384 of `0023` in its
**CRLF** form; the local checksum is the sha384 of the same file in its **LF** form. Full match on
both sides, at commit `d27656d`. The SQL never differed by a character — only the line endings did,
and migration checksums cover bytes.

Remedy executed: `drop database nm with (force)` / `create database nm owner nm`, then
`cargo sqlx migrate run`. All 32 migrations applied cleanly, `migrate info` reports nothing
pending. No test data was lost: `#[sqlx::test]` builds its own database per test, which is also
why the broken dev database never showed up as a failing suite.

**The mechanism recurred during M2b**, which is the part worth keeping. A Python edit to `0032`
left CRLF in the working tree; `.gitattributes` (`* text=auto eol=lf`) normalises on commit, so the
committed blob was correct and nothing noticed the working tree was not. It was caught by eye.
A migration applied locally from CRLF bytes records a checksum that can never match again — so the
guard against it is a test, not vigilance. Task R8 adds one: any CR byte under `migrations/` fails
the suite.
