Status: needs-triage

# An untick that races a list deletion is silently dropped

Filed from M1d's C-1/C-4 fix round 2 by the implementer that caused it, who named it as its own
defect rather than leaving it for a reviewer.

## What happens

`unsubscribe_list` reads the `lists` row with `for key share` — which is what stops a concurrent
rename from leaving a stale name behind, and what removed an ABBA deadlock. When the competing
statement is a **deletion** rather than a rename:

1. a subscriber unticks list `L` on the preference centre
2. an operator deletes `L`; the untick queues behind it on the `lists` key lock
3. the delete commits; the untick's select now matches nothing
4. no suppression row is written, and `unsubscribe_list` returns `Ok(())`

**The refusal is silently dropped.** The page tells the person their preferences were saved.

## Why it is worse than it was

Before `for key share`, this race either wrote nothing or failed the foreign key — **a loud 500**.
The fix turned a loud failure into a silent success, *and* made the window far likelier to be
entered, because the untick now **queues behind** the deletion instead of racing it.

`delete_list` is not a short transaction: it holds the `lists` key lock across the whole cascade —
`list_memberships`, `jobs`, `form_lists`, plus the `set null` sweep over `suppressions` and
`consent_log`. On a large list that window is wide.

## Why dropping it is *mostly* right

There is no list, there are no memberships, and there is nothing left to refuse. The exception is
the one case this entire scope exists for: **a list of that name recreated later.** A refusal
recorded as an orphan would have been adopted by the new list; a refusal dropped is gone.

## Why it was not fixed in that round

Writing the refusal anyway needs a list **name**, and the only name available is the one the caller
read — which is exactly the stale read that `for key share` was added to stop trusting. Telling
"the best record of a list that no longer exists" apart from "a stale snapshot" is a judgment the
code cannot currently make, and it deserves its own design pass rather than a clause in a round
already carrying an index redesign.

**It was made loud instead**: a warning naming the list id, with no address logged. That warning
has no test — it is a log line in a branch only a committed concurrent deletion reaches.

One detail worth keeping, because it is the kind of guard this branch has repeatedly had to fix:
the warning was first written as `rows_affected() == 0`, which would have fired on the **ordinary
idempotent conflict** as well — reporting a deletion on a correct path. It now reads whether the
`lists` row existed, out of the same materialised CTE the row is built from.

## What a fix looks like

Decide whether a refusal for a list that no longer exists should be recorded as an orphan, and if
so, where its name may honestly come from. Options worth weighing:

1. **Record the orphan from the locked read.** The name is trustworthy at the moment the lock is
   held; the question is whether it still is after the delete commits.
2. **Re-read inside the same transaction after acquiring the lock** and distinguish "never existed"
   from "existed and is now gone" explicitly, recording an orphan only in the second case.
3. **Accept the drop** and say so in the preference centre's copy, rather than reporting success.

Option 3 is the honest minimum if neither of the others is taken.

## Related

- `issues/36` — the uniqueness redesign that this window sits beside.
- `issues/35` — the operator half of the same deletion hole.
