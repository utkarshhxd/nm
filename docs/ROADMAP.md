# NM — roadmap and status

*As of 27 November 2026. NM is the working name.*

NM is a self-hosted email-marketing platform. It replaces our Interspire installation. It covers
contacts and consent, campaigns, automation and an API, and it is built to send at volume with
deliverability rules enforced by the product rather than left to the operator.

## Where we are

Work started on 7 November. Each milestone is split into slices, and each slice ends with
something usable that is merged to `main`.

| Milestone | What it delivers | Status |
|---|---|---|
| **M0 Foundations** | Repository, CI, accounts (passwords, passkeys, TOTP), workspaces and roles, API skeleton with OpenAPI, UI shell and design system | **Done** 11 Nov |
| **M1 Audience** | Contacts and custom fields, lists, tags, the segment engine, CSV import, suppressions, double opt-in, signup forms, preference centre, consent log | **Done** 21 Nov (four slices) |
| **M2a** Relays and sending domains | SMTP relays with failover, sending domains with a live SPF/DKIM/DMARC checker, in-app DKIM signing, the send worker, the first real email | **Done** 22 Nov |
| **M2b** Templates and editor | Block editor, hand-written MJML/HTML, brand kits, safe personalisation (merge tags escaped for where they land), preview with lint and link checks | **Done** 26 Nov |
| **M2c** Campaigns and send pipeline | Campaigns with audiences, scheduling across time zones, test sends, A/B splits, the send pipeline with pause/resume/cancel and progress, and one-click unsubscribe | **Done** 26 Nov |
| **M2d** Tracking (launch cut) | Open and click tracking, send-time hard bounces into suppression, a campaign report, and a local run package | **Done** 27 Nov. Tracking switches and custom tracking domains move to a later slice. |
| **M3** Feedback loop | Bounces and complaints the relay reports after the send, automatic pause on bad signals, bot filtering, analytics, automatic A/B winner | Estimate mid December |
| **M4** Automation | Flow builder, automation engine, triggers and steps, versioning | Estimate late December. This is the largest milestone. |
| **M5** Platform | Complete API, bulk endpoints, transactional sends, outgoing webhooks | Estimate early January |
| **M6** Migration | Interspire and listmonk importers. Our first brand moves over. | Estimate early-to-mid January |
| **M7** Hardening and v1.0 | Benchmarks, security review, documentation, packaging | Estimate mid January |

## Launch

The first usable product, campaign sending plus tracking, is complete with M2c and M2d. It runs
locally with one command (`docs/run-locally.md`): the app, its database, and a local mail catcher
for trying sends safely. Moving it into our Kubernetes cluster comes next, once the local trial is
signed off. What was deliberately left out of the launch cut is listed in `issues/54`.

## When is v1 done

The build is estimated to complete in **mid January 2027**. v1 is formally done only when the
reference deployment has moved over and has **sustained its full production volume for two
weeks in a row** with no breach of the sending rules. That puts v1 at **early February 2027 at
the earliest**.

**How the estimates are made.** The six slices delivered so far took about three to four days
each. The dates above apply that pace to the remaining work. They are not commitments.
Milestones from M3 on have not been designed in detail yet, and each is re-estimated when it is.

## Known risks to the dates

- **Relay limits.** Our relay (OCI Email Delivery) allows 50,000 emails a day and 300 a second
  by default. The product targets 5 million a day. Raising the limit is a service request to
  Oracle, and it should start early. The product's own throughput is proven against a test
  relay instead.
- **Live-relay checks.** Six questions about OCI's behaviour can only be answered by sending
  through a real OCI account: bounce reporting, suppression replies, and whether its DKIM
  signature covers the unsubscribe headers. They must be answered before M2 is signed off.
- **M4 (automation)** is the largest and least-defined milestone, and the likeliest to move.

## How the work is checked

Every slice goes through the same steps before it reaches `main`:

1. A written spec, then a task-by-task plan.
2. Each task is built with its tests, then reviewed independently. The reviewer deliberately
   breaks the code in the places that matter, such as tenant isolation and escaping, and
   confirms the tests catch it.
3. A full CI gate: Rust tests, the web build, browser tests against a real server, a dependency
   licence and security audit, and an offline build check.
4. A whole-branch review of how the pieces fit together, then a pull request merged to `main`.

## M2c in detail

*Done 26 November. Ten tasks in two tracks, server and web, built in parallel where they do not depend
on each other.*

| Task | What it builds | Depends on |
|---|---|---|
| R1 | Database schema for campaigns and the message queue | — |
| R2 | The send-ready email: footer with postal address, unsubscribe link, content frozen at launch | — |
| R3 | Campaign API: create, edit, schedule (time zones and DST), test-send requests | R1, R2 |
| R4 | The message queue: claiming work, crash safety, never sending twice | R1 |
| R5 | Launch: freezing content and resolving the audience | R2, R3, R4 |
| R6 | The send loop: relay rate limits and failover, pause and resume, test-send delivery | R2, R4, R5 |
| R7 | Concurrency and lock-order checks, throughput benchmark | R3–R6 |
| W1 | Campaign list screen | — |
| W2 | Campaign editor: audience, schedule, A/B, test send | W1 |
| W3 | Live send-progress view, wired to the real API, with browser tests | W2, R3 |

- **Campaigns:** draft, scheduled, sending, sent, paused, cancelled. Changes are role-gated.
- **Audience:** lists and segments, minus suppressed, unsubscribed and non-consenting contacts.
  It is fixed when the send starts, so it cannot change mid-send.
- **Scheduling** in a named time zone.
- **Test sends** to explicit addresses.
- **A/B splits.** Automatic winner selection needs open and click data, so it arrives with M3; at launch the winner is picked by hand, guided by clicks per link.
- **Send pipeline:**
  - Respects each relay's rate limit, with failover between relays.
  - Never mails the same contact twice for one campaign, even after a crash.
  - Supports pause, resume and cancel, and shows live progress.
- **One-click unsubscribe (RFC 8058)** in every campaign email, as Gmail's and Yahoo's
  bulk-sender rules require.
- **Every campaign email carries the sender's postal address**, as anti-spam law requires,
  including hand-written templates.
