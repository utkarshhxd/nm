Status: needs-triage

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
