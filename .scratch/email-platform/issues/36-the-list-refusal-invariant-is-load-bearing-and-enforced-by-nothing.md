Status: RESOLVED 2026-11-21 — the invariant is now enforced by an index; kept for the reasoning

# "At most one live list-scope refusal per address per list" is load-bearing and enforced by nothing

> **Resolved in fix round 2 (`8cd78aa`), before M1d shipped.** The title describes the state this
> issue was filed in, and the state is gone. A re-review reproduced a second, worse path to the
> same violation — one that silently re-pointed an orphan and reopened the delete-and-recreate
> resurrection the `list` scope exists to close — so the answer was a redesign rather than another
> clause:
>
> ```sql
> create unique index suppressions_live on suppressions (workspace_id, scope, email_hmac, domain)
>   nulls not distinct where lifted_at is null and scope <> 'list';
> create unique index suppressions_live_list on suppressions (workspace_id, email_hmac, list_id)
>   where lifted_at is null and scope = 'list' and list_id is not null;
> -- orphans (list_id is null) are deliberately NOT unique, so a list deletion can never raise
> ```
>
> **A live list refusal is keyed on the list, not on a name a rename moves.** No unique index over
> a live list row mentions `list_name`, so the rename carve-out and every stale name it produced
> were *deleted* rather than guarded, and `unsubscribe_list` now conflicts on the list index and
> does nothing. Two live refusals on one list are a `23505` instead of an invariant somebody
> maintains.
>
> The Minor-1 cost recorded below — a `23505` surfacing as a 500 on a public untick — **does not
> land**, because the arbiter is now the index that would raise, so the conflict resolves to
> `do nothing`. The prior reviewer had added the index without re-aiming the conflict target.
> Shipping it inside `0027` also means there is no existing-duplicate repair problem.
>
> Everything below is the original issue, kept because the reasoning is why the redesign happened.

Filed from M1d's C-1/C-4 fix round, by the implementer that fixed the defect this invariant's
absence caused. It is the honest version of "we fixed it" rather than the comfortable one.

## The shape

`0027` added a `list` suppression scope keyed on `(workspace_id, email_hmac, list_name)`, plus two
triggers on `lists`:

- **rename** — `after update of name`, move every refusal bound to that list onto the new name.
- **adoption** — `after insert`, bind orphaned refusals (`list_id is null`) whose stored
  `list_name` matches the new list, so every reader can match on `list_id` alone.

`suppressions_live` enforces uniqueness on **`list_name`**. Nothing enforces uniqueness on
**`list_id`**. So two live refusals can exist for one address on one *list*, as long as their
stored names differ — and the rename statement then moves both onto one name at once and violates
`suppressions_live` from inside a trigger, which rolls back the operator's rename as a 500.

One reachable sequence was found and closed during the fix round:

1. an address refuses a list named `B`; `B` is deleted → an orphan carrying the name `B`
2. the same address refuses a list named `A`
3. rename `A` → `B`: the carve-out leaves the first row at `A` while adoption pulls the orphan onto
   the same list → **two live refusals on one list**
4. rename again → `duplicate key value violates unique constraint "suppressions_live"`

The fix was one clause on the adoption statement — do not adopt an orphan onto a list where that
address already has a live refusal. It is a no-op on the ordinary insert path, because a freshly
created list has no refusals yet.

## Why this is filed rather than closed

**The clause maintains the invariant; it does not make violating it impossible.** The obvious
enforcement is a partial unique index on `(workspace_id, email_hmac, list_id)` for live non-orphan
rows.

**Correction, 2026-11-21.** This issue first recorded the reason for deferring that index as "it
moves the failure onto `on delete set null`, so deleting a list would start failing." **That is
wrong**, and a re-review found it wrong by building the index and running the sequences: a list
deletion does *not* fail, because `on delete set null` nulls `list_id` and the row drops out of the
partial index. Delete-and-recreate adoption still works too.

What the index actually does is **catch the violation instead of preventing it** — it turns a
silent duplicate into a `23505` on the subscriber's untick, which is a 500 on a public,
unauthenticated page. That is the real cost and the real reason it needs a design pass rather than
a clause in a fix round.

One practical consequence for whoever takes it: **once a workspace already holds the duplicate,
`create unique index` fails outright** (`Key (…) is duplicated`). Adding this index later needs a
data repair first, and the repair has to decide which of two live refusals for one list is the one
to keep.

Closing this properly means deciding what the key for a list refusal actually is — the name, the
id, or a pair with an explicit rule for orphans.

## What the next person needs to know

- The two triggers interact, and the state space is larger than either the controller or the
  reviewer enumerated on the first pass. The three-step and four-step sequences above were both
  found by someone chasing a claim, not by reading.
- **Any change to either trigger must be exercised against: rename, rename-twice, delete, delete
  and recreate under the same name, delete and recreate under a different name, and an orphan
  already holding the target name.** That list is the minimum, and it is not in any test file as a
  single suite.
- `suppressions_live` is `nulls not distinct`, which is why address and domain rows (null
  `list_name`) are unaffected by any of this.

## A second, smaller cost recorded here

`unsubscribe_list` now reads `lists` with `for key share`, which is what stops a concurrent rename
from leaving a stale name behind — and which also removed a **pre-existing ABBA deadlock** between
the preference centre's save and an operator's rename.

The cost: **an untick on the public, unauthenticated preference centre now waits on an operator's
rename in progress.** Today a rename is one `UPDATE` plus its trigger, so the wait is short. If a
rename ever grows a longer transaction around it, that becomes a preference-centre stall on a page
a stranger can reach. Whoever adds work to the rename path owns that.

## Related

- `issues/32` — `consent_log`'s triggers are `enable always`; `0027`'s are `ORIGIN`, deliberately
  and for the opposite reason. `0027` says so.
- `issues/35` — the operator half of the same deletion hole, still open.
