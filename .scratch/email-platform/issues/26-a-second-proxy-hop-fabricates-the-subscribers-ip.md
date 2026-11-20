Status: needs-triage

# A second reverse-proxy hop makes every `consent_log.ip` the proxy's address

Filed from the prior-milestone hazard audit run during M1d. This is **not** the known
`trust_proxy_headers` default — turning that flag on does not fix this.

`.scratch/email-platform/m0-decisions.md:188-189` recorded:

> hop count is not configurable… Record this in the deployment docs.

**It was never recorded.** There is no deployment page under `docs/`, and grep finds no mention of
hop count anywhere in the tree.

## What goes wrong

`client_ip` takes the rightmost entry of `X-Forwarded-For` when `trust_proxy_headers` is on. That
is correct for exactly one trusted hop. In a CDN → nginx → nm deployment — an ordinary shape for a
product that sends marketing mail — the rightmost entry is **the CDN's address**, not the
subscriber's.

M1d's public routes are the first code in the product to record a client IP as *evidence*. So in
that deployment, every `consent_log.ip` written by `/c/`, `/u/` and `/f/` is a proxy's address
presented as the subscriber's.

`carried-constraints.md` states the standard this falls short of, in the project's own words:

> a fabricated IP in a consent row is worse than an absent one, because it reads in court as the
> subscriber's.

An absent IP is an honest gap in the evidence. A CDN's IP recorded as the subscriber's is a false
record that looks like evidence, and it is the kind of false record that only gets discovered when
someone is relying on it.

## Why it is not just a docs fix any more

When M0 recorded this, the client IP was used for rate limiting and sign-in lockout, where being
one hop off is a tuning problem. M1d made it evidence. The consequence of a misconfiguration
changed from "limits apply to the wrong bucket" to "the consent log contains fabricated
provenance", and nothing in the code or the configuration notices.

## What a fix looks like

Three options, in increasing order of effort, and the first two are not exclusive:

1. **Write the deployment documentation M0 asked for**, naming the single-hop assumption and what
   breaks when it does not hold. This is the minimum and it is overdue.
2. **Make the hop count configurable** (`trusted_proxy_hops`, default 1), so the rightmost-minus-N
   entry is taken. Small, and it turns a silent wrong answer into a setting someone can get right.
3. **Record nothing rather than something false.** If the hop count cannot be established, write
   `ip = NULL` on consent rows instead of a value that may be a proxy's. The consent log is
   explicitly designed to tolerate a null IP; it is not designed to tolerate a wrong one.

Option 3 is the one that matches the project's stated standard, and it is worth deciding
deliberately rather than inheriting by default.

## Related

- `issues/24-constraint-changes-on-consent-log-will-block.md`
- `issues/25-ipv4-is-not-aggregated-by-the-rate-limiters.md`
- The `trust_proxy_headers` startup warning added in M1d Task 4b covers the *first* hop only.
