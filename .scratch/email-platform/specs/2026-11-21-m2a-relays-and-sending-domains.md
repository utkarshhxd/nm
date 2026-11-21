# M2a — relays, sending domains, and the first real send

**Spec.** Derived from `.scratch/email-platform/spec.md` §6, §7, §8, §9 and §11, and from
`m2-decomposition.md`. Research behind the rulings: `research/m2-oci-email-delivery.md`.

**Status:** draft for implementation, 2026-11-21.

---

## 1. Intent

Nothing in this product sends mail. M1d built double opt-in, the consent log, the preference centre
and signup forms, and the confirmation emails it enqueues into `jobs` are never delivered — so the
forms cannot be used by anyone and every consent guarantee M1d shipped is untested against real
delivery.

**M2a ends when a confirmation email from an M1d form arrives in a real inbox**, correctly signed,
carrying the headers a bulk sender must carry, sent through a configured relay whose limits the
product respects.

That is the smallest slice that is useful on its own and the one that proves the hardest external
dependency. Everything else in M2 — templates, campaigns, tracking — sends through what this slice
builds.

## 2. Goals

1. A workspace can configure one or more **relays** (SMTP), with credentials encrypted at rest.
2. A workspace can configure **sending domains**, and see a live **DNS checker** for SPF, DKIM,
   DMARC and Return-Path alignment.
3. The product can **sign messages with DKIM itself**, per sending domain, switchable.
4. A **worker** consumes `jobs` and sends, obeying each relay's rate limits and connection caps,
   with retry, backoff and honest handling of ambiguous outcomes.
5. Every message carries the headers a bulk sender must carry, and the unsubscribe link is **ours**.

## 3. Non-goals for this slice

- Campaigns, the `messages` table, audience resolution, A/B, scheduling — M2c.
- Templates, MJML, the editor, merge tags — M2b. M2a sends one fixed, hand-written message.
- Open and click tracking, tracking domains — M2d.
- Bounce and complaint **intake** — M3. M2a creates the conditions for it (Return-Path, headers,
  `messageId` correlation) and consumes nothing.
- Multiple regions. One region's configuration is enough; the research records that approved
  senders, suppression lists, return paths and DKIM domains are all per-region, and M2a records
  that constraint without solving it.

## 4. Anti-goals — failure even if it all works

- **An unsubscribe the product cannot see.** If a relay's own unsubscribe handling takes the click,
  the person is suppressed in the relay and nowhere in `consent_log` or `suppressions`. The product
  would then claim a consent record it does not have, which is worse than having no record.
- **A send that looks successful and was not.** OCI accepts mail to a suppressed address with
  `254 4.7.1` rather than rejecting it. A pipeline that reads that as success is lying to the
  operator.
- **Double sending.** A worker that retries an ambiguous outcome sends twice. Not sending once
  beats sending twice.
- **A relay credential in a log, an API response, or an audit row.**
- **Taking the API down by sending.** The worker must not be able to exhaust the pool `/readyz`
  and the authenticated API depend on.

## 5. Rulings

These settle questions the codebase does not decide. Each is a decision, with its reason and what
it costs if wrong.

### 5.1 The worker is a mode of the one binary, with its own pool

`nm worker` as spec §7 requires, and `nm all` runs it in-process beside `serve`. **The worker gets
its own connection pool**, sized separately from the API's 20.

*Why:* `issues/33` records that the 20-connection pool is already hand-apportioned between the API,
exports and one-click, and that `/readyz` reads an exhausted pool as not-ready. A worker drawing
from the same pool makes sending able to take the product down. Two pools cost two configuration
values and remove the whole class.

*Cost if wrong:* two pools where one would do, and a second number to size.

### 5.2 The claim protocol is `0026`'s, verbatim and unchanged

A single `update … for update skip locked` that advances `run_after` as the lease, predicated on
`failed_at is null and completed_at is null and attempts < max_attempts`. Success is
`completed_at = now()`. **Nothing deletes a `jobs` row**: the retention trigger refuses deleting a
row younger than a day, because counting those rows is the signup form's only defence against
being an open mailer.

*Why:* the contract is already authoritative in `comment on table jobs`, and `0023`'s version is
stale and uneditable. Re-deriving it is how the last milestone's defects happened.

**Lease 120 seconds. Claim up to 16 rows per tick. Poll every second, with jitter.**

*Why these:* a single SMTP send over a pooled connection is well under a second, so 120 s is two
orders of margin for a slow relay without leaving a crashed worker's rows stuck for long. Sixteen
is enough that one worker is not the bottleneck at the signup form's own rate limits, and small
enough that a crash re-does little. Polling rather than `LISTEN/NOTIFY` because the latter needs a
dedicated connection and careful reconnect handling to avoid silently missing wakeups, and the
claim is one indexed query — `shortcut: a 1 s floor on latency, upgrade to LISTEN/NOTIFY when a
tick's claim regularly comes back full`.

### 5.3 Claiming is by `kind`, even though there is only one kind today

The claim predicate takes a `kind` filter. M2a passes `confirmation_email`.

*Why:* strict FIFO across all kinds means a 5-million-row campaign sits in front of every
confirmation email, and a person waiting on a double opt-in link waits behind it. Adding the filter
now costs one predicate; adding it later means changing the protocol after M2c has built on it.

### 5.4 Relay rate limiting is shared through Postgres, spent per batch

A token bucket per relay, held in a Postgres row, debited **once per claimed batch** by the number
of messages that batch will send, never per message.

*Why:* spec §7 requires the limit to be shared across workers, and Postgres is the only coordinator
the product has. Per-message would be a write per send; per-batch is one `update` per tick. OCI
signals exhaustion with `455` and publishes no `Retry-After`, so **the bucket is not an
optimisation — it is the only thing that prevents a refusal the relay will not tell us how to wait
out.**

*Cost if wrong:* a batch that over-estimates its own size wastes tokens, slowing sending slightly.

### 5.5 We always set our own HTTPS `List-Unsubscribe`

Every message M2a sends carries:

- `List-Unsubscribe: <https://…/u/{token}>` — **ours**, never the relay's
- `List-Unsubscribe-Post: List-Unsubscribe=One-Click`
- `Feedback-ID`
- `Message-ID` on the sending domain

*Why:* OCI adds its own one-click headers when it does not find an HTTPS one, and a click on its
link suppresses the person **in Oracle's list and nowhere in ours** — no `consent_log` row, nothing
the preference centre or a GDPR export can see. The documentation says an HTTPS header we supply is
left alone, and its own FAQ contradicts that, so this must be verified (§8.1).

*Cost if wrong:* if OCI replaces ours anyway, unsubscribes become invisible to the product and
M2a's acceptance test catches it before anything ships.

### 5.6 In-app DKIM signing is first-class, not optional

A sending domain carries its signing mode: **relay-signed** or **app-signed**. App-signed means the
product holds the private key, encrypted at rest with the existing `Secrets` machinery, and signs
before submission with an `h=` that **explicitly includes `List-Unsubscribe` and
`List-Unsubscribe-Post`**.

*Why:* RFC 8058 requires the signature to cover both headers, and Oracle publishes no `h=` policy,
no way to configure one, and one stale example that omits them. The spec called app-side signing
optional; for the reference deployment it may be mandatory, and that is not known yet. Building it
behind a per-domain switch costs one enum and removes the dependency on an unanswered question.

*Cost if wrong:* an unused code path if OCI turns out to sign correctly.

### 5.7 Send-time eligibility is re-checked, and the view is not enough

Before a message is handed to the relay, the worker re-checks suppression. For a confirmation email
this is `blocking_suppression` on the address, **not** `mailable_list_members`.

*Why:* the view matches plaintext email and is therefore blind to erasure tombstones, which keep a
hash and no plaintext. A confirmation to an erased address is exactly the failure M1d's erasure
work exists to prevent. The view remains correct for M2c's campaign path, where the join is by
contact; M2a's path has a bare address and needs the hash lookup.

### 5.8 An ambiguous outcome is `unknown`, and is never retried

Three outcomes, three behaviours:

| relay says | we record | we retry |
|---|---|---|
| 2xx accepted | sent | no |
| 4xx (incl. `455`, `421`) | transient | yes, with backoff |
| 5xx | failed | no |
| **connection lost after `DATA`, before a reply** | **`unknown`** | **never** |

*Why:* spec §7 states it and the reason stands — not double-sending matters more than a rare miss.
A person receiving a second copy of a confirmation email is a worse failure than one not arriving,
because the second copy is indistinguishable from the product being broken.

**`254 4.7.1 Suppression for user …` is not a success.** OCI accepts mail to a suppressed address
rather than rejecting it. It is recorded as a delivery failure with its own reason.

### 5.9 Relay credentials are encrypted at rest and never leave the process

Stored with the `Secrets` encrypted-blob machinery that already carries `KEY_ID`. Never in an API
response, never in a log line, never in an audit row — spec §8 invariant 7. The API returns a
relay's host, port, username and *whether* a password is set; never the password.

### 5.10 The ADR enumeration is re-run, not merely obeyed

`relays` and `sending_domains` are new tables. `issues/38` records the method: **a new foreign-key
parent, or a new cascade, means re-running ADR 0005's enumeration**, not just obeying the order.
That is what caught `form_lists` after the order was thought complete. M2a's plan carries that as a
task with its own deliverable — an updated enumeration table in the ADR, with a ruling and a reason
per new parent.

## 6. What gets built

**Schema** (one migration, `0028`):

- `relays` — per workspace: name, host, port, security (implicit TLS / STARTTLS), username,
  encrypted password, max connections, rate limit per minute, enabled, failover order.
- `sending_domains` — per workspace: domain, from-name default, signing mode (`relay` | `app`),
  DKIM selector, encrypted private key (app mode), return-path domain, and the last DNS check
  result per record type with its timestamp.
- `relay_buckets` — the shared token bucket, one row per relay.

Every index `relays` and `sending_domains` will need goes in at creation: the SPDX-header
convention means every migration runs inside a transaction, so `create index concurrently` is
impossible later.

**Rust:**

- `src/relay/` — SMTP transport with pooled connections, the token bucket, failover, and the
  outcome classification of §5.8.
- `src/sending_domains/` — CRUD, the DNS checker (SPF, DKIM, DMARC, Return-Path alignment), and
  the app-side DKIM signer.
- `src/worker/` — the claim loop, `nm worker`, and `nm all`'s in-process variant.
- `src/mail/` — MIME construction: headers, the plain-text part, `Message-ID`, `Feedback-ID`, and
  the unsubscribe headers of §5.5.

**Web:** relay and sending-domain settings screens, with the DNS checker's status per record and
an explicit acknowledgement before sending from a domain that fails its checks.

## 7. Correctness invariants for this slice

Each needs a test that can fail.

1. **A `jobs` row is never sent twice.** Two workers racing the same row: exactly one claims it.
2. **A suppressed or erased address is never mailed**, checked at send time, including the erasure
   tombstone case the view cannot see.
3. **Every message carries both `List-Unsubscribe` headers**, they point at *our* endpoint, and
   when the domain is app-signed the DKIM `h=` covers both.
4. **An ambiguous outcome is never retried.**
5. **`254 4.7.1` is recorded as a failure**, not a success.
6. **A relay password never appears** in an API response, a log line, or an audit row.
7. **The worker cannot exhaust the API's pool** — it has its own.
8. **The relay's rate limit is never exceeded across concurrent workers.**

## 8. Acceptance — and the question only a live tenancy can answer

### 8.1 The DKIM test, which gates sign-off

Spec §13.4 requires this before M2 signs off, and documentation cannot settle it. Send a seed
message through the real OCI tenancy to a Gmail and a Yahoo address, read the raw headers, and
record:

1. Does OCI's `DKIM-Signature` `h=` include `List-Unsubscribe` and `List-Unsubscribe-Post`?
2. Are client-supplied headers signed at all?
3. Does OCI **keep** our HTTPS `List-Unsubscribe` pair, or replace it? Test a `mailto:`-only header
   too — the docs contradict themselves.
4. Does a signature **we** apply before submission survive OCI's handling? If OCI mutates headers
   after acceptance, app-side signing breaks and that is the planned workaround.
5. Which `d=` appears when customer DKIM is configured — ours, Oracle's, or both.

**If 1 and 2 are no, app-side signing stops being a switch and becomes the only supported mode for
this relay.** The code supports both either way; the answer decides the default.

### 8.2 The slice is done when

A double opt-in signup through an M1d form produces a confirmation email that arrives, whose link
confirms the subscription, whose `List-Unsubscribe` is ours and works, and whose DKIM signature
covers it — with the full gate green and the ADR enumeration re-run.

## 9. Known constraints this slice inherits

Recorded so they are not rediscovered:

- **The reference relay is two orders of magnitude below §9's targets** — 50,000/day and 300/s
  against 5M/day and 1,500/s. The throughput target is a property of the pipeline, provable
  against a sink relay, and is not reachable through OCI without a negotiated increase.
- **Envelope From must equal header From**, and both must be approved senders. Per-recipient VERP
  is unavailable; correlation is on `messageId`.
- **The Return-Path is Oracle's** unless a custom one is configured, and even then Oracle's BMTA
  processes the bounces.
- **Message size 2 MB by default**, header block 256 KB, and billing counts one email per 2 MB
  chunk per recipient.
- **`jobs.payload` holds a live 14-day confirm token and a plaintext address**, kept safe only by
  the contact-cascading composite FK `0026` added. M2a must not widen that exposure.
- **`consent_log` is append-only with `enable always` triggers.** A row written during sending
  cannot be corrected later.
