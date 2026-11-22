Status: needs-triage

# An over-subscribed relay now grows the queue for ever, instead of destroying mail

Recorded by M2a Task 11's implementer as the deliberate trade behind two fixes it made, so that the
trade is visible rather than discovered.

## The two defects this replaced, because they are the reason the trade is right

1. **`bucket::take` was all-or-nothing.** A tick claims up to 16 jobs and asked the bucket for all
   16. A relay rated below 16 a minute refused **every tick, for ever** — no error, no failed row,
   nothing red, mail simply never leaving. A 100-an-hour plan is an ordinary thing to buy, and it
   hit this immediately.
2. **A job the tick never started still spent an attempt.** With `min(n, limit)` making the
   hold-back the *common* path on a slow relay, each tick's overflow was destroyed after five ticks
   — punished for waiting its turn.

Both destroyed mail silently. Both are fixed.

## What the fix trades

A job that is claimed but never submitted is now **released**: the claim's increment is undone,
`run_after` is set to now, and the row is immediately claimable again. It has not been tried, so it
does not spend an attempt.

The consequence: **a relay that is permanently over-subscribed retries for ever.** An operator who
configures a limit of 10 a minute and enqueues 1,000 a minute now has a queue that grows without
bound, rather than a queue that empties itself by destroying its contents.

That is the correct default — a growing queue is visible and recoverable, destroyed mail is neither
— but it is not free, and nothing currently makes the growth visible.

## What is missing, and should exist before this ships to anyone with volume

- **A queue-depth metric**, by `kind` and by workspace. The product already exposes Prometheus
  metrics (`metrics_listen`), so this is an exporter, not new infrastructure.
- **An age-of-oldest-claimable-job metric.** Depth alone cannot distinguish a busy queue from a
  stuck one; age can.
- **Something an operator sees without going looking.** A relay whose bucket refuses for an hour
  is a misconfiguration, and the operator will not be watching a dashboard. A warning on the
  settings screen, or a log line that fires once per relay per hour rather than per tick.

## What not to do

**Do not reintroduce a cap by failing rows.** That is defect 2 again wearing a different hat, and
the failures would land on exactly the workspaces least able to notice — the ones on small plans,
whose mail is throttled precisely because they bought a small plan.

If a bound is genuinely wanted, bound the *enqueue* side, where the operator can be told at the
moment they ask for something the relay cannot deliver.

## Related

- M2a spec §9 records that the reference relay's documented limits are 50,000 a day and 300 a
  second, two orders of magnitude below the spec's §9 targets — so over-subscription is not a
  hypothetical for this deployment, it is the expected steady state until a limit increase is
  negotiated.
- `issues/33` — pool bounds, the other place where a resource limit is hand-apportioned.
