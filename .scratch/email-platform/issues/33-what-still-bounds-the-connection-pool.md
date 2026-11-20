Status: needs-triage — **read before raising `max_connections` or adding a timeout layer**

# What still bounds the 20-connection pool, and what does not

Filed from M1d's second fix wave, which closed the one hole that could take the instance down and
deliberately left the rest of the arithmetic where it was.

## What was closed

`POST /u/{token}` is exempt from `429` by ruling: a rate-limited unsubscribe is spec §4's second
anti-goal. That exemption meant the route had no bound at all — it opens a transaction and takes
`pg_advisory_xact_lock` on the address, so repeats of **one** valid link queued on that lock, each
holding a pooled connection while it waited. Measured: with no bound, twelve concurrent one-clicks
on one token held **8 of 8** connections of a test pool, and `/readyz` reads an exhausted pool as
not ready — so one stranger with one old link could take the authenticated API down.

Now bounded by `MAX_CONCURRENT_ONE_CLICKS = 4`, an instance-wide semaphore acquired **before**
`pool.begin()`. It refuses nobody: a request without a slot waits, then is served normally.

## What is not bounded, in descending order of how much it matters

1. **`db::connect` has no `acquire_timeout`.** sqlx's 30-second default stands. Any route that
   saturates the pool makes every other route wait up to thirty seconds rather than fail fast.
   Adding one turns pool pressure into `500`s across the whole authenticated API, which is a
   product-wide behaviour change and belongs to whoever owns the pool — not to a one-route fix.

2. **The four authenticated limiters are keyed per workspace.** `AppState` already records the
   arithmetic: four workspaces acting at once can hold all twenty connections. Nothing instance-wide
   caps authenticated concurrency.

3. **The one-click semaphore's wait queue is unbounded.** That is the price of never refusing:
   ten thousand concurrent one-clicks on one token now cost ten thousand parked tasks instead of a
   dead pool — far cheaper, but not free, and nothing sheds them. A per-token in-flight counter is
   where that would be bounded. It was deliberately not added: the semaphore bounds the share of
   the pool the exempt route may hold, which is what the finding was about, and a second mechanism
   is a second thing to keep correct.

## What to decide before the first real deployment

Pick one bound for the whole process rather than one per route:

- an `acquire_timeout` plus a global concurrency layer, sized against `max_connections`, with the
  one-click route still exempt from refusal; or
- a larger pool with the per-workspace limiters tightened; or
- leave it, and accept that saturation presents as slowness rather than refusal.

Whichever is chosen, `MAX_CONCURRENT_ONE_CLICKS` and `MAX_CONCURRENT_EXPORTS` are both sized
against `max_connections(20)` by hand. **Changing the pool size silently invalidates both**, and
nothing in the code will say so.

## Related

- `issues/19` — the saturation tests stack a pool on a pool.
- `issues/25`, `issues/26` — what a rate-limit key is actually worth.
