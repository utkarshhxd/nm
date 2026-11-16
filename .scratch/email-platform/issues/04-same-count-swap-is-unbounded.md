Status: resolved

# A selection can swap which contacts it means without changing how many

Found in M1a task 17 review. **Blocking for M1b's importer**, which is what turns this from exotic
into routine.

## What the widening check actually buys

`Selection::widened` compares the count the token was issued with against the count the predicate
resolves to at execution time. It buys exactly one sentence:

> No more contacts are acted on than the person was shown.

It buys nothing at all about *which* ones. One contact leaving the match while another joins it
leaves the count unchanged, and the check returns false. The snapshot (`id <= as_of`) freezes
creation time only; `push_filter` reads list memberships, tags and names as they are when the
action runs.

## The magnitude is not small

The number that can be substituted equals the number that left the match inside the token's
one-hour window. Nothing bounds that below the size of the selection itself. An import that
retags a segment -- removing `churn` from one group and adding it to another -- can swap every
contact a token stands for, and both the server check and the UI's report stay silent, because
both are counting.

The TTL bounds how long the window is open. It does not bound how much moves inside it.

## Why M1b is the trigger

M1a has no importer and no sender, so the only way to move a contact across a filter boundary is a
person clicking in another tab. M1b ships the importer, and an import that *updates* existing
contacts is exactly that write. Spec section 8.2's own justification for the snapshot only covers
imports that **create** contacts, which the `as_of` bound already handles.

## What a fix would have to carry

The materialised set of ids the filter matched at issue time, which is the thing a predicate
selection exists not to carry: "all 2.3 million matching" cannot be sent as a list, and that is
the whole decision in `docs/adr/0003-selection-is-a-snapshotted-predicate.md`.

Plausible directions, none of them free:

- Store the matched id set server-side under the token id, with a TTL, and intersect at execution.
  Costs storage proportional to the selection and makes the token stateful.
- Add a `contacts.updated_at`, or a filter-relevant version counter, and bound on it as well as on
  `id`, so a contact changed after `as_of` falls out of the selection entirely. Cheaper, and it
  changes the semantics: an edit would drop a contact from a selection it genuinely matches.
- Accept it, and say so in the UI at selection time rather than after the action.

Decide before the importer lands, not after.

## Comments

**2026-11-14 — resolved in M1b by the first of the three directions above.** Verified against the
code and the tests, not against the spec.

`POST /contacts/selection` now evaluates the predicate **once**, inside one transaction, and writes
the ids it matched into `selection_snapshot_members` (`selection::materialise`,
`src/contacts/selection.rs:283`, migration `0017_selection_snapshots.sql`). The token carries
`{workspace_id, snapshot_id, issued}` and nothing about the predicate, so there is no predicate left
to re-evaluate and no filter in the token that could describe a wider set. A bulk action **narrows
`contacts` by** the frozen set and never reads ids out of it. A swap needs one contact to leave the
match while another joins it; neither half can happen to a set of ids already written down, so the
check that "counts and does not identify" has been replaced by one that identifies.

The second direction this issue proposed -- an `updated_at` or version counter bounding the
selection -- was tried and **killed with measurements**, and that argument is why this issue was
closed by storage and not by a bound. Negation inverts under a time bound (`not_has` bounded by `t`
becomes true once the tag row is deleted, so a contact that was never shown is acted on -- a
*superset* violation, and through `not_member_of` it is the re-solicitation predicate), and `now()`
is transaction-start time, so a long transaction -- an import, which is what this issue named as the
trigger -- commits rows stamped before a token issued in between. The long form is in
`docs/adr/0004-selection-is-a-materialised-set.md`, which supersedes `0003`.

The cost this issue predicted is real and is recorded rather than hidden: 4,887 ms and 158 MB for a
1M-row selection, against a 150 ms budget. It is paid for with a 1,000,000 cap, a rate limit,
one live snapshot per actor, a detached materialising task and an `unlogged` members table. ADR
0004's "Cost if wrong" carries all of it.

- Commits: **`7306d70`** (snapshots: migration, `materialise`, cap, supersede, reaper), `8376982`
  (its review items, including the API-key supersede), **`6f8aaa1`** ("feat: bulk actions read the
  frozen set, never a predicate" -- the commit that actually closes this), `852ffa5` (its review
  items).
- Rust tests, `tests/bulk.rs`: `a_tag_applied_after_the_selection_does_not_bring_its_contact_in`
  (`:265`) is the swap in the joining direction -- a colleague tags three more contacts into the
  filter and `bulk/delete` still hits only the one that was frozen, with the other three alive;
  `a_tag_removed_after_the_selection_does_not_take_its_contact_out` (`:294`) is the leaving
  direction, which a time bound could never give; `not_has_is_frozen_in_both_directions` (`:315`)
  and `not_member_of_is_frozen_in_both_directions` (`:353`) pin the inversion that killed the
  timestamp design; `refuse_if_widened_never_fires` (`:659`) asserts the old counting guard is now
  dead weight. Restoring the re-evaluating design as a mutation reds **eleven** tests, including
  the lib unit test -- that is the suite failing against the exact bug it exists to prevent.
  Building `sel` from the members table instead of narrowing `contacts` gives a `500`, an FK
  violation, which is why the "narrows, never supplies" rule is a tested property and not a warning.

**Not closed by this, and tracked elsewhere:** a frozen set records identity and nothing else, so a
future **merge** feature would make it stop meaning what it says. M1b spec §15.3 and ADR 0004 both
carry the requirement -- an importer may *update* a contact, never replace one, and a merge must
remap snapshot members or invalidate live snapshots.
