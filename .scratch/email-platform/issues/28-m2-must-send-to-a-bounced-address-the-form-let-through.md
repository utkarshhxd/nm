Status: needs-triage — **M2 must read this before its worker ships**

# M1d's signup form deliberately enqueues for a bounced address, and M2's worker may drop it

Filed from M1d Task 9's fix round, where the implementer raised it as a thing that "may buy nothing
end to end". It is right, and the layer that can finish the job is M2's.

## What M1d decided

`POST /f/{workspace}/{slug}` refuses a suppressed address for `complaint`, a manual block,
`gdpr_erasure` and `unsubscribed_all` — but **lets a previously `Bounced` address through**.

The reasoning, which stands: a bounce is a *deliverability* fact, not a consent decision. Someone
re-typing their address into a signup form is usually fixing exactly the thing that bounced, and
**the double opt-in confirmation email is itself the deliverability test.** Permanently refusing
them, with no mechanism a subscriber can reach, strands a person who has done nothing wrong and
cannot find out why.

So the route creates the contact, writes the consent row, and enqueues a `confirmation_email` job.

## Where it breaks

If M2's worker refuses to send to any address with a live suppression — the obvious and otherwise
correct rule — then the contact exists, the job is queued, **the worker drops it, and the person is
stuck one layer further down than before.** Worse than the old behaviour, because now they have a
`pending` membership and a consent row saying they asked, and still no email.

Nothing in the queue row says "this one is deliberate". A worker written from first principles will
drop it.

## What M2 needs to do

Pick one, deliberately:

1. **Let `confirmation_email` jobs through a `Bounced` suppression** and treat the send result as
   the test: if it bounces again, the suppression stands and nothing has been lost; if it delivers,
   lift the bounce. This is the behaviour M1d's decision assumes.
2. **Refuse, and tell the person.** Then M1d's route must stop enqueueing for a bounced address and
   say something truthful on the page instead — which puts the problem back where it was, so this
   option means reversing the M1d ruling too, not just ignoring it.

Option 1 is the one the consent model is built for. Option 2 is defensible but it is not a
no-decision: it requires an M1d change, and leaving both halves as they are now is the only
outcome that is certainly wrong.

## A related limit, same route

Two genuinely concurrent submissions can still enqueue two confirmation emails for the same
`(contact, list)`: neither transaction sees the other's uncommitted row, and the 15-minute resend
throttle is decided in the same statement as the insert but cannot see an uncommitted sibling.
Bounded at two and documented in the code. Closing it needs a lock, and the per-address advisory
lock is currently taken only when a contact is created.

## Related

- `issues/27-lifting-a-bounce-can-lose-a-one-click-unsubscribe.md` — the other place where
  per-reason suppression semantics are coarser than the product needs.
