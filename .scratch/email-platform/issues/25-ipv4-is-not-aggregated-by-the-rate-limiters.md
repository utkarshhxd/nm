Status: accepted-as-designed

# IPv4 is not aggregated by either rate limiter, so a /24 buys 256 budgets

Filed from M1d Task 4b's fix round, where it was raised by the implementer and deliberately not
acted on. **This is a record of a decision, not a defect report.**

`auth::rate_limit::bucket_of` masks an IPv6 address to its /64 and leaves an IPv4 address alone.
So one IPv6 /64 is a single rate-limit key, while one IPv4 /24 is 256 separate keys — 256 budgets
on the public routes, and 256 sign-in lockout budgets, which has been true since M0.

## Why this is the right asymmetry

The reason /64 masking exists at all is that **an IPv6 /64 is free.** Any VPS hands one out with
the instance, so 2^64 addresses cost an attacker nothing, and a per-address limiter against that
is not a limiter. That was M1d Task 4b's first Critical finding: the public surface was
effectively unlimited until the key was bucketed.

An IPv4 /24 is the opposite. Addresses are scarce, they cost real money, and a block is registered
to whoever bought it. An attacker who commits a /24 to this has spent something and left a trail.

Treating the two differently is the correct response to their actually being different. Masking
IPv4 to a /24 would cost far more than it buys: households, offices, CGNAT ranges and
link-scanning proxies would collapse into shared buckets, and the comment on `PUBLIC_PER_MINUTE`
already reasons carefully about exactly those cases.

## What would change this

- A measured abuse pattern from IPv4 ranges that the current limits do not absorb.
- A decision to limit by something other than address — a token, a workspace, or a proof-of-work —
  at which point the address is no longer the key and this question dissolves.

Either is a product-wide decision about how this product rate-limits, not a change to one limiter.
It is filed here so the next person who notices the asymmetry finds the reasoning instead of
"fixing" it.

## Related

`issues/24-constraint-changes-on-consent-log-will-block.md` — the other decision recorded during
the same fix round.
