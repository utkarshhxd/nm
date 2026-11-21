Status: needs-triage

# Lifting a bounce can silently lose a one-click unsubscribe

Filed from M1d Tasks 5 and 6, where the implementer found it and recorded it as a `shortcut:` in
the code. Nobody is mailed wrongly today; this is about a state the product can reach later.

## The mechanism

`suppressions_live` is unique **per address, regardless of reason**. So when one-click unsubscribe
writes its workspace-wide opt-out, and that address already has a live suppression for some other
reason — a `bounced` row, say — the insert is skipped. The bounce row stays and no
`unsubscribed_all` row is ever written.

That is harmless while both exist, because either one blocks the address and the person is not
mailed.

It stops being harmless the moment an operator lifts the bounce. Lifting a bounce is an ordinary,
legitimate action — the address was failing, it has been fixed, the operator clears it. But the
`unsubscribed_all` that the person themselves asked for was never recorded, so clearing the bounce
clears the only thing blocking them, and **the product starts mailing someone who clicked
unsubscribe.**

The consent log still records `Action::Suppressed`, so the evidence that they unsubscribed
survives. What does not survive is the thing that *enforces* it.

## Why it is filed rather than fixed

Fixing it properly means making the live-suppression index per-reason, so an address can hold a
`bounced` row and an `unsubscribed_all` row at once and lifting one leaves the other. That is a
schema change to `suppressions` — the table `docs/adr/0001` calls the one place that can forbid
sending — and it touches every reader of `blocking`. It is not a change to make inside a task
about the preference center, and it deserves its own thinking about what "lift" means when several
reasons are live.

## What a fix looks like

1. Replace the per-address unique index on `suppressions_live` with a per-`(address, reason)` one.
2. Decide what the suppressions UI shows when several reasons are live, and what "lift" does there
   — almost certainly lift one reason, not the address.
3. Keep `blocking` as the single definition of mailability: it is true while *any* live reason
   exists, which is already how it reads.

Until then, the `shortcut:` comment in `src/suppressions/` records the limit at the place someone
would change it.

## Related

- `docs/adr/0001-contacts-have-no-status-column.md` — why a suppression, not a membership, is the
  durable opt-out.
- Hazard report finding 10, as amended: the preference center may lift `unsubscribed_all` and
  nothing else. That rule and this issue meet when an address holds both reasons.
