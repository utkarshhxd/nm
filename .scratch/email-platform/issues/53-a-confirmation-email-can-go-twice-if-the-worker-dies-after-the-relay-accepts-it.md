# 53 — A confirmation email can go twice if the worker dies between the relay's 250 and the row's completion

Status: needs-triage

**Found by:** the M2c spec (§9, "Known constraints inherited"), where it is recorded as not
retrofitted. Filed here by M2c R7 so that it is on a worklist and not only in a spec.
**Blocks:** nothing. It is a duplicate of a low-stakes message, not a lost or a mis-sent one.

## What is true

`worker::confirmation::handle` sends the double opt-in email and **then** records the outcome:
`attempt` submits through the relay, and only when the relay answers `250` does `complete` mark the
job. The job's lease (`JOB_LEASE`, 120 s) is what keeps another worker off it meanwhile.

If the worker dies, is killed, or loses its database connection **after the relay accepted the
message and before `complete` lands**, the row is still `claimed, not completed`. When the lease
runs out another claim takes it and sends the confirmation again. The subscriber gets two emails
with two links; both work.

`handle` already says so in a comment (a `complete` that did not land "means this row is claimed
again when its lease expires, and the message goes twice"), so the window is known. What is new is
that M2c closed the same window for campaign messages and not for this path.

## Why M2c did not retrofit it

Campaign messages are marked `submitting` in a committed statement **before** SMTP, and a
`submitting` row is never claimed again: when its lease expires the sweep records it `unknown`, and
`unknown` is final. That costs a message that may have been delivered being counted `unknown`, and
never a second copy. It needed a `messages` table with a status column and a claim token.

The `jobs` table has neither. Doing the same for confirmations means a status or a marker column
on `jobs`, a claim path that refuses a marked row, and a decision about what a stuck marked row
becomes — and a stuck confirmation is different from a stuck campaign message, because the
subscriber is waiting for the link and a lost one is a lost subscription. For a campaign, `unknown`
is the right ending. For a confirmation, a duplicate is probably the better failure than silence.

## The decision this needs

Whether to close it at all. The options:

1. **Leave it.** A duplicate double opt-in email is harmless to the subscriber (both links work)
   and needs a worker death at one exact moment. Record that this is deliberate and delete the
   worry.
2. **Mark before send, as campaigns do, and treat a stuck mark as retryable.** Closes the duplicate
   for the crash case and re-opens it as "resend after the mark's lease", so it narrows the window
   without removing it. Probably not worth a migration.
3. **Mark before send and treat a stuck mark as `unknown`, as campaigns do.** No duplicate ever,
   at the price of a subscriber who never gets their link when a worker dies at the wrong moment,
   and has to ask for another.

Option 1 is where this stands today by default.

## Related

- M2c spec §4.7 ruling 3 (the write-ahead) and §9.
- `src/campaign/queue.rs`, `mark_submitting` and `sweep`: what campaigns do.
- `src/worker/confirmation.rs`, `handle`: the window, and the comment that names it.
