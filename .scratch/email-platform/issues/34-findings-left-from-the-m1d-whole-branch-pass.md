Status: needs-triage

# Findings left from the M1d whole-branch pass

Four whole-branch reviewers (security, data, consent, evidence) read `m1d-consent-and-forms`. Two
fix waves took everything that was worth fixing inside the milestone. These are what was left on
purpose, each with the reason it was left.

Same shape as `issues/17` (M1b) and `issues/23` (M1c).

## 1. The dropped-submission metric and log line are unpinned

`public_form_submissions_dropped_total` and the `warn!` beside it fire when the rate limiter drops
a signup that `POST /f` may not refuse out loud. **Neither has a test.** The metrics recorder is
process-global — `tests/metrics.rs` holds exactly one test for that reason — and nothing in this
tree captures `tracing` output.

So the one signal an operator has that signups are being silently discarded is itself unverified.
A later wave that adds a tracing-capture harness should pin both; until then, a refactor can
delete the only thing that makes this failure visible and nothing goes red.

## 2. The drop metric cannot distinguish a per-key refusal from a full limiter map

Both reach the route as `over = Some(60)`. Telling them apart would mean widening `Quota`, which
is a change to the limiter's interface for a reporting distinction. One counter was the smaller
change; the log line names `MAX_KEYS` as the state to go and look at, because a constant stream of
drops is what a full map looks like from outside.

## 3. `save`'s action word can describe a transition that did not happen

`public/unsubscribe.rs::save` picks `Subscribed` or `Resubscribed` from a `prior` snapshot that the
`on conflict do update` does not share under READ COMMITTED. A writer committing between the two
statements leaves the word one step off. **The comment was corrected rather than the code**: the
window is one statement wide, needs a concurrent writer on the same `(contact, list)`, every
outcome is a neighbouring word for a change that really happened, and buying exactness costs a row
lock on every candidate of every save.

## 4. Every confirmation records `source = form`

True only because forms are currently the sole issuer of confirm tokens. **It breaks the moment M2
or an admin resend issues one for an import- or API-requested `pending`.** Commented at the point
the assumption is made, with the two honest fixes named: read the source off
`list_memberships.source`, or add a `Source` meaning "a confirmation link". Deriving it today would
be a mapping with no caller that needs it.

## 5. Invariant 3's concurrency is not itself asserted

The `tokio::join!` in that test is not pinned by an assertion, so the test would still pass if the
two halves ran in sequence. **Left as-is deliberately:** it discriminates anyway — dropping the
guard gives 5 consent rows instead of 3 — so the concurrency adds strength rather than coverage,
and the test is honest about what it proves.

## 6. Eight test names or comments claim more than they hold

The evidence reviewer found eight. **None of them is one of spec §8's ten invariants** — all ten
are pinned by at least one test that was individually verified to be able to fail. Three of the
eight were fixed in fix wave 2 (the schema-collision guard, the resend-window test, `page::shell`'s
`Cache-Control` claim); the rest are comments that overstate, not tests that fail to discriminate.

## 7. A membership can read `subscribed` while nothing will ever be sent to it

A `list`-scope suppression (M1d's C-1/C-4 fix) makes an address unmailable **on that list only**.
That is the point. But it surfaces nowhere: `blocking_suppression` returns `None` for it
deliberately — a per-list refusal is not a block on the address, and reporting it as one would be
the bug — and the Suppressions screen reads `address_suppressions`, which excludes list rows.

So an operator who re-adds a contact whose membership row was destroyed sees the member listed as
`subscribed`, sees no block anywhere, and no mail is ever sent to them for that list.

**The behaviour is correct and the invisibility is not.** The analogous `unsubscribed_all` state
*does* surface on the contact detail screen. Whatever shows a list refusal should make clear it is
scoped to that list, or an operator will read it as a full block and lift it.

## Not in this issue

- `issues/30` — the consent log loses which list when a list is deleted.
- `issues/32` — `enable always` will stall a logical replica.
- `issues/33` — what still bounds the connection pool.
- `issues/35` — an operator-driven unsubscribe is still lost to a deletion.
- `issues/36` — the list-refusal invariant is load-bearing and enforced by nothing.
- **The per-list unsubscribe not surviving a contact or list deletion** is a product decision, not
  a finding. It went to the product owner with the merge decision.
