# M2 Sending core — decomposition

Spec §11 states M2 as one milestone: *"relays, sending domains with the DNS checker, templates,
editor and mrml, personalisation, campaigns (schedule, timezone, test sends, A/B), send pipeline,
tracking, one-click unsubscribe."*

That is larger than all of M1, which shipped as four milestones (M1a–M1d). **M2 ships as four too.**
Each ends with something usable, which is the rule §11 sets for a milestone.

## Why this order

The ordering is driven by one fact: **nothing in the product sends mail today.** M1d built double
opt-in and enqueues confirmation emails into `jobs` that are never delivered. Until a relay exists,
every consent guarantee M1d shipped is untested against real delivery, and M1's forms cannot be
used by anyone.

So the first slice is the one that makes those confirmations arrive. It is also the smallest
end-to-end slice that proves the hardest external dependency works.

## M2a — relays, sending domains, and the first real send

**Ends with:** a double opt-in confirmation email from an M1d form actually arrives in an inbox.

- Relay configuration: SMTP transport, pooled connections per relay, per-relay rate limiting
  (token bucket shared across workers), connection caps, multiple relays with failover.
- Sending domains per workspace: from-domain, DKIM (relay-signed or app-signed), Return-Path,
  with the live DNS checker for SPF, DKIM, DMARC and Return-Path alignment.
- Optional in-app DKIM signing, for relays that do not sign.
- The **worker**: the first consumer of `jobs`, using the claim protocol `0026` defines.
- The required marketing headers on every message — `List-Unsubscribe`, `List-Unsubscribe-Post`,
  `Feedback-ID`, `Message-ID` on the sending domain — wired to M1d's existing token machinery.
- Message construction: MIME, headers, the auto-generated plain-text part.

**This slice carries the project's two unanswered open items** (spec §13.3 and §13.4): whether OCI
Email Delivery's DKIM signature covers the `List-Unsubscribe` headers, and how OCI reports bounces
and complaints. §13.4 says the DKIM question must be verified **before M2 is signed off**, and it
is load-bearing here: if the relay's signature does not cover those headers, RFC 8058 one-click is
not honoured by Gmail and Yahoo, and the in-app DKIM option stops being optional.

## M2b — templates, the editor, and personalisation

**Ends with:** an operator can build a branded email and preview it.

- Block editor producing MJML, plus raw HTML/MJML mode. mrml for rendering.
- Templates and a per-workspace brand kit (logo, colours, fonts, footer).
- Merge tags with fallbacks. Conditional blocks.
- Dark-mode preview, auto-generated plain-text part, link checker.

Loaded on demand — spec §9 caps the app shell at 150 KB compressed and names the editor as one of
the two things that must not be in it.

## M2c — campaigns and the send pipeline

**Ends with:** a campaign goes out to a real audience.

- `messages`, one row per recipient, unique on `(campaign_id, contact_id)` — invariant 1.
- Launch: resolve lists or a segment minus suppressions, in batches, without blocking the UI.
- Claim with leases; a crashed worker's batch is retried — invariant 6's sibling.
- **Suppression re-checked at send time**, one lookup per batch — invariant 2.
- Per-recipient render, tracked links and unsubscribe tokens injected.
- Result recording: 4xx retry with backoff, 5xx bounce, and **ambiguous outcomes marked `unknown`
  and never auto-retried** — not double-sending beats a rare miss.
- Pause, resume, cancel within 5 seconds — invariant 5.
- Scheduling, send-at-recipient-local-time, test sends, A/B on subject/content/sender with an
  auto-winner on **clicks** rather than opens.

## M2d — tracking

**Ends with:** campaign analytics have real numbers behind them.

- `GET /t/c/{token}` click redirect: 302 **without waiting on the database**, p99 < 20 ms at
  5,000 req/s. In-memory buffer flushed in batches at least once a second; losing a few events on
  crash is acceptable.
- `GET /t/o/{token}` open pixel, same shape.
- Per-workspace tracking domains (CNAME), with the bundled Caddy's on-demand TLS asking the app
  which domains are allowed — and the app saying yes only for verified ones.
- Bot filtering: impossibly-fast clicks, scanner networks and user agents, hidden honeypot links.
- Apple Mail Privacy Protection opens flagged unreliable.

## What M2 does **not** include

- Bounce and complaint **intake** is M3, not M2. M2a produces the Return-Path and the headers that
  make intake possible; it does not consume anything.
- Automation flows are M4. M2c's pipeline is what M4's flow sends will reuse.
- Transactional sends are M5, reusing the same engine.

## Invariants M2 must satisfy

From spec §8, the ones that become testable for the first time in this milestone:

1. **At most once per campaign per contact** — a database unique constraint plus idempotent workers.
2. **No marketing email to a suppressed address**, checked at send time rather than only at launch.
3. **Every marketing email carries both `List-Unsubscribe` headers**, and the one-click endpoint
   works.
5. **Pausing stops new sends within 5 seconds.**

Invariants 4, 7 and 8 are already held by M1 and must not regress.

## The lock order applies

`docs/adr/0005-one-lock-order-for-every-path.md` binds every new writer. M2 adds writers to
`list_memberships` (none expected) and `consent_log` (unsubscribes from tracked links), and a
`messages` table that will be a new FK child of both `contacts` and `lists`. **A new FK parent or a
new cascade means re-running the ADR's enumeration**, not just obeying the order — that is the
method recorded in `issues/38`, and the thing that caught `form_lists` after the order was thought
complete.
