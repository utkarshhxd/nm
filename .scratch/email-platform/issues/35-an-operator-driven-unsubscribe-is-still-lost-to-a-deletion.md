Status: needs-triage — **the remaining half of M1d's C-1**

# An operator-driven unsubscribe is still lost to a deletion

M1d closed this for everything the subscriber does to themselves and left it open for what an
operator does on their behalf. The asymmetry is deliberate, it is bounded, and it is written down
here because the alternative is someone discovering it from a complaint.

## What is closed

| path | durable? | how |
|---|---|---|
| `POST /u/{token}` one-click | yes, already was | address-keyed `unsubscribed_all` suppression, which outlives the contact row and the list row and covers lists that do not exist yet |
| preference centre, box unticked | yes, new in `0027` | `list`-scope suppression keyed on `(workspace_id, email_hmac, list_name)` |
| import, form signup, confirm | honour both | an import or a form submission cannot re-subscribe past either |

## What is open

`lists::unsubscribe_member` (one member) and `selection::unsubscribe` (bulk) write **only** the
`list_memberships` row and its consent row. Both cascade on `contacts` and on `lists`. So:

1. An operator unsubscribes someone — because they replied asking to be taken off, say.
2. The contact is deleted, or the list is deleted. Either is a routine Editor action.
3. An import naming that address and that list name re-subscribes them, and the consent log cannot
   say they ever left.

Because the importer matches a list by name, **delete-recreate-import still resurrects everyone an
operator unsubscribed from that list.**

## Why it was not closed with the rest

`suppressions` is keyed on `email_hmac`, derived in Rust from the instance secret —
`crypto::suppression_hmac`, a documented one-way door. `contacts` has no hash column. The bulk path
handles up to `MAX_SELECTION` = 1,000,000 contacts set-based, in one statement, in one transaction.
So a durable write on that path means one of:

- **send the derivation key into SQL** so the insert can compute the HMAC set-based — which puts
  the instance secret in query text and in any log that captures it; or
- **round-trip every address into Rust** to hash it, then write back — a million addresses, and
  then `suppressions::lock_address`'s recorded SHORTCUT bites: N advisory locks is
  `out of shared memory`, and skipping them inverts the lock order the erase path depends on.

Wiring the single-member path but not the bulk one was rejected for a better reason than cost:
it would make "did your unsubscribe survive the deletion?" depend on **which button the operator
pressed**, which is worse than a gap that is uniform and documented.

## The fix, when someone takes it

**Add `email_hmac` to `contacts`**, written on create and on any address change, backfilled for
existing rows. That makes the bulk insert set-based — `insert into suppressions … select … from
contacts where id = any($1)` — with no key in SQL and no per-address round trip, and both operator
paths then write the same `list`-scope row the preference centre writes.

It is its own change: a new column holding a keyed hash of an address, a backfill, and a write path
that must not drift from `crypto::suppression_hmac`. **Changing that derivation string already
makes every stored hash unreproducible**; a second producer of the same hash doubles what a change
to it breaks, and that needs to be stated wherever the column is defined.

## What must not happen meanwhile

Nothing in the admin UI, the API docs or the spec may suggest an operator unsubscribe is durable.
Spec §8 invariant 1 carries the amendment: the invariant holds for what the person does, not for
what an operator does on their behalf.

## Related

- `issues/30` — the consent log loses which list once that list is deleted.
- `docs/adr/0001-contacts-have-no-status-column.md` — why a suppression, not a column, is where a
  standing refusal lives.
