Status: needs-triage

# A constraint change on `consent_log` blocks the table, and it is specced to grow like `events`

Filed from M1d Task 4a's fix round, where it was raised as a Minor and deliberately not changed.

`migrations/0021_consent_requested_action.sql` widens `consent_log`'s `action` check with
`drop constraint` / `add constraint`. That takes `ACCESS EXCLUSIVE` and validates every row.

**That is the right call today and the wrong call later.** The implementer's reasoning for leaving
it, which stands: the alternative — `add constraint … not valid` followed by
`validate constraint` — avoids the blocking scan, but leaves a window in which the constraint
exists and is unvalidated. For a `consent_log` holding a handful of rows in a dev database, that
window is a worse trade than a lock measured in milliseconds.

## Why it is filed anyway

Spec §10 leaves open "whether `consent_log` is partitioned now or later; it is append-only and
**will grow like `events`**". So the table this migration locks instantly today is explicitly
planned to become one of the largest in the product. Nothing records that the migration pattern
has to change with it, and the next person to add an action value will copy `0021`.

The product has not shipped, so no deployment has a large `consent_log` yet. This is a note about
the first one that does.

## What a fix looks like

The next constraint change on `consent_log` — or any change to `0021`'s pattern once the table is
partitioned or large — uses:

```sql
alter table consent_log add constraint consent_log_action_check check (...) not valid;
alter table consent_log validate constraint consent_log_action_check;
```

`validate constraint` takes `SHARE UPDATE EXCLUSIVE`, which does not block reads or writes. The
unvalidated window is acceptable at that size precisely because the blocking scan is not.

The same question applies to any future index on `consent_log`: `create index concurrently`, not
`create index`. `0020`'s index and `0023`'s paging index were both created on a table that was
empty at the time.

## What this issue is not

It is not a defect in `0021`. `0021` is correct, and re-writing it now would mean amending a
migration that has already run — which drifted the dev database once already in this milestone.
The two-step form belongs in the *next* migration that touches this table, not retroactively in
this one.
