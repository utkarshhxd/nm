# OCI Email Delivery — what the docs say, and what they do not

Researched 2026-11-21 for M2a, from Oracle's own documentation. Every Oracle page cited carries
only a "Copyright © 2026" footer with no revision date, so recency cannot be verified from the
pages themselves. Where a page is demonstrably stale it is marked.

This answers spec §13 open items 3 and 4, as far as documentation can. **Three things it cannot
answer need a live test against the real tenancy**, and they are listed at the end.

---

## 1. The capacity gap — read this first

| | documented OCI limit | spec §9 target |
|---|---|---|
| per day | **50,000** (paid tier) | **5,000,000** |
| per minute | **18,000** (= 300/s) | **90,000** (= 1,500/s) |

**The reference relay is two orders of magnitude below the performance targets the spec sets.**
Limit increases are a service request, reviewed against volume, burst pattern, opt-in practice and
CAN-SPAM/CASL compliance, and require SPF and DKIM on the sending domain plus an approved sender.
Free trial is far lower still: 200/day, 10/minute.

Counting is per recipient, and **per 2 MB chunk**: a 10 MB message to one recipient counts as five
emails. Message size default 2 MB including headers, body and base64 encoding, raisable to 60 MB by
service request. Header block maximum 256 KB (`554 Message parse error` beyond it).

**Design consequence:** the send pipeline must be relay-limit-driven, not throughput-driven. The
token bucket is not an optimisation, it is the thing that keeps the relay from rejecting. The
spec's 1,500/s target is a property of the *pipeline*, provable against a sink relay, and is not
reachable through OCI without a negotiated increase.

Sources: [Overview — Service Capabilities and Limits](https://docs.oracle.com/en-us/iaas/Content/Email/Concepts/overview.htm),
[Limits](https://docs.oracle.com/en-us/iaas/Content/Email/Reference/gettingstarted_topic-Limits.htm).

---

## 2. `List-Unsubscribe` — OCI implements it too, and may overwrite ours

**OCI Email Delivery adds its own RFC 8058 one-click unsubscribe headers** (feature released
2024-06-14). A hit on its link adds the recipient to *OCI's* suppression list with reason
`UNSUBSCRIBE`, bumps the `EmailsListUnsubscribed` metric, and writes an `unsubscribe` log record.

The documentation **contradicts itself** on what happens to a header we supply:

- [List-Unsubscribe](https://docs.oracle.com/iaas/Content/Email/Reference/list-unsubscribe.htm),
  step 1: *"If the email doesn't have an HTTPS List-Unsubscribe URL already set, Email Delivery
  replaces any existing header with a one-click unsubscribe link. Otherwise, the unsubscribe
  headers are left alone."*
- The same page's FAQ: *"Email Delivery will add the required headers only if they're not already
  present in submitted emails."*

Read together: an **HTTPS** `List-Unsubscribe` we set is probably kept; a `mailto:`-only one is
probably replaced. **Untested.**

And explicitly, if our own HTTPS handler is used: *"requests … will not be processed by Email
Delivery, so there will be no suppression list addition, nor any metric or log record added."*

### Ruling for M2a

**We set our own HTTPS `List-Unsubscribe` and `List-Unsubscribe-Post`, always.**

Not a preference — a correctness requirement. M1d built the consent log, the preference centre and
the suppression model, and spec §8 invariant 3 is ours to hold. If OCI's link handles an
unsubscribe, the person is suppressed **in Oracle's list and nowhere in ours**: no `consent_log`
row, no `suppressions` row, nothing the preference centre or an export can see. The product would
claim a consent record it does not have.

Accepting that OCI then records nothing for those unsubscribes is the correct trade: we own the
record, and `issues/35`'s durability work means ours survives deletion.

---

## 3. DKIM coverage — **not documented, and it blocks sign-off**

No Oracle page states which headers go in the `h=` tag, whether client-supplied headers are signed,
or how to configure the signed-header list. The DKIM configuration surface is **only** selector and
key management — create, rotate, import. Maximum two keys per email domain, one active. BYODKIM
(import your own key) was added 2025-02-20. Neither `CreateDkim` nor `UpdateDkim` takes a header
list.

The one published example signature, on
[Managing DKIM](https://docs.oracle.com/en-us/iaas/Content/Email/Tasks/configuredkim.htm):

```
a=rsa-sha256; c=relaxed/relaxed; s=prod-fra-20191115;
d=fra1.rp.oracleemaildelivery.com;
h=Date:To:From:Subject:Message-Id:MIME-Version:Sender;
```

That `h=` does **not** include the unsubscribe headers — but the selector dates the sample to
around 2019 and the page does not say the list is fixed, default or complete. It is an
illustration, not a policy statement.

**Why it matters:** RFC 8058 requires the DKIM signature to cover both `List-Unsubscribe` and
`List-Unsubscribe-Post`. Gmail and Yahoo honour one-click only when it does. If OCI's signature
does not cover headers we supply, our one-click is not honoured and **in-app DKIM signing stops
being optional** — spec §6 lists it as "optional … for relays that don't sign", and this would
make it required for the reference deployment.

It is plausible that headers OCI adds *itself* are inside its own signature, since otherwise its
own one-click claim fails at Gmail. That says nothing about headers we add.

**Design consequence:** M2a builds in-app DKIM signing as a first-class, switchable path rather
than a later addition, and the live test below decides whether the reference deployment turns it
on.

---

## 4. Bounces and complaints — no webhooks, logs and a suppression API

Everything is pull-based. Email Delivery is **not** in the Events producers list, so there are no
per-message Events rules.

| path | what arrives | where |
|---|---|---|
| SMTP rejection (sync) | response on the submission connection | the SMTP session |
| submission record | `action=accept`, with `errorType`, `smtpStatus`, `messageId`, `sender`, `envelopeSender`, `recipient`, `receivingDomain` | OCI Logging, `emaildelivery.emaildomain.outboundaccepted` |
| async bounce | `action=bounce`, with `errorType` = `soft`\|`hard`, `bounceCategory` (18 values, e.g. `bad-mailbox`, `quota-issues`, `spam-related`, `policy-related`), `bounceCode` (RFC 1893, e.g. `5.5.0`), `smtpStatus` verbatim, `originalMessageAcceptedTime`, `recipientMailServer` | OCI Logging, `…outboundrelayed` |
| complaint (FBL) | `action=complaint` — **base fields and `message` only**, no `smtpStatus`, no `bounceCode`, **no ARF body** | same |
| unsubscribe | `action=unsubscribe`, plus `recipientIp`, `userAgent` | same |
| aggregates | Emails Accepted / Relayed / Hard Bounced / Soft Bounced / Suppressed / Complaints / ListUnsubscribed / Opened / Clicked, 90-day retention | OCI Monitoring, `oci_emaildelivery` |
| per-address state | `emailAddress`, `reason`, `timeCreated`, `timeLastSuppressed`, `messageId`, `errorDetail` (raw SMTP error), `errorSource` (from the RFC 3464 DSN) | Email API `ListSuppressions` / `GetSuppression` / `CreateSuppression` / `DeleteSuppression` |

Facts that shape M3:

- **Logging is opt-in per email domain** and off by default. Nothing is retroactive — if it is not
  enabled before a send, that send's bounces are unrecoverable.
- **Reading:** Logging Search API, query window **capped at 14 days per query**, paginated via
  `opc-next-page`.
- **The only push path** is Connector Hub with a Logging source, targeting Notifications,
  Streaming, Functions or Object Storage. Logging-source limits: 24 h retention, ~10 GB/hour to
  Streaming or Object Storage; Notifications drops messages over 128 KB.
- **No ARF, no FBL enrolment.** *"The Email Delivery service takes care of the feedback loops and
  platform reputation automatically."* Gmail, Yahoo and Microsoft complaints reach us only as
  `action=complaint` rows, the Complaints metric, and `COMPLAINT` suppression entries. Oracle owns
  those FBL relationships because the FBL is keyed to its IPs and return-path.
- *"Email Delivery doesn't offer INBOX service or automated DMARC report processing"* — a DMARC
  `rua`/`ruf` mailbox is ours to run.
- **Suppression is not optional:** *"You can't opt out from the suppression mechanism."* Reasons:
  `COMPLAINT`, `HARDBOUNCE`, `SOFTBOUNCE`, `MANUAL`, `UNSUBSCRIBE`, `UNKNOWN`. Addresses are
  lowercased.
- **A send to a suppressed address is accepted, not rejected**: SMTP `254 4.7.1 Suppression for
  user <ocid> to <recpt>` with an `outboundaccepted` record carrying
  `errorType="Recipient suppressed"`. **So a silent non-delivery looks like a success** unless the
  log is read. M3 must treat that `errorType` as a delivery failure.

Oracle's own advice matches the architecture: *"The best way to keep a clean database is to sync
with Email Delivery using API calls so any hard bounces, unsubscribes, and suppressions are marked
as such."*

---

## 5. Envelope, Return-Path and sender constraints

- **The Return-Path is Oracle's by default.** *"Email Delivery is required to process bounces to
  protect the reputation of our IP addresses and domain."* **Per-recipient VERP in our own envelope
  is not available** — correlate on `messageId` instead. This rules out the usual bounce-routing
  design.
- **Custom Return Path** exists (released 2024-05-14): the domain must match the approved sender's
  domain or be a subdomain, suggested form `<REGIONKEY>.rp.<sending-domain>`. Either a CNAME to the
  regional bounce domain, or MX `10 bmta.email.<REGION>.oci.oraclecloud.com` plus SPF
  (`include:rp.oracleemaildelivery.com`, or `ap.`/`eu.` variants). DNS first, then create the
  return path. Bounces still land at Oracle's BMTA — this changes the visible domain, not who
  processes bounces.
- **Envelope From must equal header From**, and both must be approved senders: *"if the SMTP 'From'
  address is not the same as the 'From' address in the email body, the email is rejected."*
- **Approved senders are regional and per-compartment.** A sender created in Phoenix cannot send
  via Ashburn. SMTP credentials are global, everything else — approved senders, suppression list,
  custom return path, DKIM domain — is per region.
- **SMTP: port 465 implicit TLS** per
  [Configuring SMTP Connection](https://docs.oracle.com/en-us/iaas/Content/Email/Tasks/configuresmtpconnection.htm),
  though other Oracle pages recommend 587 STARTTLS; both appear supported and the docs conflict.
  `AUTH PLAIN`. Endpoint `smtp.email.<region>.oci.oraclecloud.com`. Two SMTP credentials per user.
- **HTTPS submission** is an alternative, with **raw MIME submission added 2025-08-05** — relevant
  if full header control over SMTP proves awkward.

### Throttling signals

`455 Maximum messages sent per minute reached : limit is <limit>` ·
`455 Maximum messages sent per day reached : limit is <limit>` ·
`421 Too many connections, try again later` ·
`421 Too many auth failures, try again later` (IP-level, from repeated bad credentials or
non-approved senders) · `535 Authorization failed: address <address> not authorized` ·
`552 Exceeds byte limit` · `553 <address> Invalid email address` · `554 Message parse error`.

HTTPS equivalents: `400 LimitExceeded`, `429 Too many requests`, `404 NotAuthorizedOrNotFound`.

**No documented `Retry-After` or backoff interval.** All are 4xx: the sender must treat `455` and
`421` as retryable and pace itself. Our token bucket has to be the thing that prevents them, since
the relay will not tell us how long to wait.

---

## 6. What documentation cannot settle — test against the real tenancy

In priority order. Item 1 blocks M2a sign-off.

1. **Does OCI's `h=` include `List-Unsubscribe` and `List-Unsubscribe-Post`?** Send to a seed
   Gmail and Yahoo box and read the raw `DKIM-Signature`.
2. **Are client-supplied headers signed at all?** Same test with an arbitrary `X-` header.
3. **Replace or keep:** does OCI preserve our HTTPS `List-Unsubscribe` pair? Test a `mailto:`-only
   header too. The docs contradict themselves.
4. **Does a DKIM signature we apply before submission survive?** If OCI mutates headers after
   acceptance, our own signature breaks — and that is the obvious workaround, so it must be tested
   before anything depends on it.
5. **Which `d=` appears with customer DKIM configured** — our domain, Oracle's, or both signatures.
   Matters for DMARC alignment and for RFC 8058, where the covering signature should align with the
   From domain.
6. **Suppression list scope:** the suppression page says tenancy-level (and that `compartmentId`
   must be the `tenancyId`); the overview and List-Unsubscribe pages say regional. Probably
   tenancy-wide *within* a region. Multi-region needs one sync per region.
7. **`unsubscribe` as a `data.action` filter value** — documented as a field value, absent from the
   log-search guide's table.
8. **Custom headers:** the field reference says they are approved-sender-level, **maximum four, set
   by support request, "(Available in future)"**, not included in unsubscribe log records and not
   available for metric filtering — yet the log-search guide shows `data.headers."x-subscriberid"`
   queries. **Do not design campaign or recipient correlation on custom headers.** `messageId` is
   the safe key.
9. **Soft-bounce suppression policy:** `SOFTBOUNCE` is a valid reason, but no document says how
   many consecutive soft bounces trigger it.
10. **Retry interval for `455`** — undocumented.

## A comparison worth no weight as evidence

Azure Communication Services has this exact defect reported publicly — its DKIM signature does not
cover `List-Unsubscribe`. That is an analogy about how easily a relay gets this wrong, not evidence
about OCI.
