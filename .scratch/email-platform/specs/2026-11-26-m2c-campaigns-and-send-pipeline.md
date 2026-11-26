<!-- SPDX-License-Identifier: AGPL-3.0-only -->

# M2c — campaigns and the send pipeline

**Milestone:** M2 Sending core, third slice. **Follows:** M2a (relays, worker), M2b (templates).
**Precedes:** M2d (tracking, auto-winner on clicks).
**Sources:** `spec.md` §6, §7 (sending pipeline), §8, §9, §11, §13.3/4/7; M2a spec; M2b spec;
ADR 0005; issues 41, 43, 48, 49, 50, 51.

This spec is the decisions. Each is `Ruling: what — why — cost if wrong`.

---

## 1. Intent

An admin sends a campaign to a real audience: lists, optionally narrowed by a segment, minus
everyone who may not be mailed. Each recipient gets exactly one personalised, signed message
with a working one-click unsubscribe and the sender's postal address. The send can be scheduled
in a named timezone, tested, split for A/B, paused, resumed and cancelled, and its progress is
visible.

## 2. Non-goals

- Open and click tracking, tracking domains, the redirector, the auto-winner. M2d.
- Bounce and complaint intake, auto-pause on rates. M3.
- `Idempotency-Key`, approval workflow, RSS, best-time-to-send.
- Partitioning `messages` (ruling 4.2.6).

## 3. Anti-goals

1. A contact mailed twice for one campaign, by retry, crash, lease expiry or two workers.
2. A message to someone who unsubscribed after launch.
3. A marketing message with no postal address or no working unsubscribe.
4. A message rendered from a document that was not classified.
5. A relay outage or throttle that destroys queued mail.

## 4. Rulings

### 4.1 Model, states and roles

- **Ruling: states are `draft`, `scheduled`, `sending`, `paused`, `sent`, `cancelled`, `failed`,
  with exactly the transitions below** — one closed machine is what every handler and the worker
  test against — a missing transition is a 409 until a spec change adds it.

  | from | to | by | role |
  |---|---|---|---|
  | draft | scheduled | `POST …/schedule` | admin |
  | scheduled | draft | `POST …/unschedule` | admin |
  | scheduled | sending | worker launch at `send_at` | system |
  | scheduled | failed | worker launch refused (reason stored) | system |
  | scheduled, sending, paused | cancelled | `POST …/cancel` | admin |
  | sending | paused | `POST …/pause` | editor |
  | sending | paused | worker system pause (reason stored) | system |
  | paused | sending | `POST …/resume` | admin |
  | sending | sent | worker, when every message is recorded | system |

- **Ruling: viewer reads; editor creates and edits drafts, deletes drafts, previews, test-sends
  and pauses; admin schedules, unschedules, resumes, cancels and picks the A/B winner** — sending
  spends the workspace's reputation and cannot be recalled, so the brake is cheaper to reach than
  the accelerator (the brand-kit reasoning, contract §9) — an editor-only team needs an admin to
  send.
- **Ruling: a transition into the state the campaign is already in answers 200 with the view;
  any other wrong state is `409 campaign-state`** — state-machine idempotency gives retry safety
  with no key store — a client retrying a different transition after a race must read the 409.
- **Ruling: `Idempotency-Key` is not built in M2c**; it moves to M5 with the rest of API
  completeness — no store exists in the codebase and every send-affecting POST here is already a
  guarded transition — a retried `POST /campaigns` makes a second draft, which sends nothing.
- **Ruling: content (audience, from, variants, A/B) is editable only in `draft`; deletion is
  draft-only** — the launch validates one immutable snapshot, and a draft has no messages —
  operators unschedule to edit.
- **Ruling: system transitions write `failure_reason` / `pause_reason` on the row and no audit
  row** — `audit::record` needs a request actor and the reason on the row is what the screen
  shows — the audit log does not list worker-made state changes.

### 4.2 Audience and resolution

1. **Ruling: the audience is the union of one or more lists' mailable members
   (`mailable_list_members`), optionally narrowed by one segment; never a segment alone** —
   consent lives in list membership (ADR 0001); a segment is a predicate, not a permission —
   "everyone subscribed to anything" means picking every list.
2. **Ruling: `list_ids uuid[]`, `segment_id`, `sending_domain_id` and each variant's
   `template_id` carry no foreign key; they are validated at save, at schedule and at launch** —
   a key into `lists` would make `campaigns` a child of lock class 2 and add a cascade path to
   ADR 0005 — a list deleted after scheduling fails the launch, with the list named in
   `failure_reason`.
3. **Ruling: resolution is a `campaign_launch` job that inserts in batches of
   `LAUNCH_BATCH = 10_000` contacts by ascending contact id, one transaction per batch, with
   `on conflict (campaign_id, contact_id) do nothing`; a claim works for at most
   `LAUNCH_SLICE = 5 s`, then completes and enqueues its successor in the same transaction as
   the cursor** — no long transaction, crash resumes from the cursor, a re-run batch inserts
   nothing twice, and the jobs loop is never blocked past 5 s — the recipient set is frozen per
   batch snapshot, not at one instant: a contact who joins mid-resolution may or may not be
   included depending on its id.
4. **Ruling: dedup by address is the `(campaign_id, contact_id)` unique key** — `contacts` is
   unique on address per workspace — none.
5. **Ruling: `messages` has no foreign key to `contacts`** — ADR 0004's precedent for
   `selection_snapshot_members`; a deleted or erased contact's message is `skipped` at send, and
   erasure leaves only a uuid — a contact deleted and re-created mid-resolution may get a row
   under each id; the old one is skipped at send.
6. **Ruling: `messages` is not partitioned in M2c** — the master spec's monthly partitioning
   needs a retention rule nobody has set, and M3's `events` brings the same machinery — converting
   later is a copy-and-swap migration on a large table.
7. **Ruling: segment rules are loaded once per resolution phase and reused for every batch** — one
   predicate per phase — a segment edited mid-resolution does not apply to that phase.
8. **Ruling: the editor's recipient estimate is one count query with a 2 s statement timeout,
   answering `{count, exact: true}` or `{count: null, exact: false}`, on the segment-preview
   limiter** — the operator's last check against the wrong audience must not cost a 10M-row scan
   per keystroke — a large audience shows "too many to count quickly".

### 4.3 Schedule and timezone

1. **Ruling: §11's "timezone" is a wall-clock time in a named IANA zone, in one of two modes:
   `instant` (one instant for everyone) and `recipient_local` (that wall-clock time in each
   contact's `timezone`, else the campaign zone)** — spec §6 lists per-recipient local time as
   v1, and a per-message `run_after` makes it one column — a recipient-local send spans up to
   26 hours.
2. **Ruling: DST follows RFC 5545 §3.3.5: a skipped local time moves forward by the gap; an
   ambiguous one takes the first occurrence; the API echoes the resolved `send_at`** — one
   standard rule for both modes, and the operator sees the instant before confirming — 02:30 on a
   spring-forward day sends at 03:30.
3. **Ruling: `recipient_local` instants are computed in Rust (`chrono-tz`) for every zone in
   `chrono_tz::TZ_VARIANTS` and passed to the resolution statement as two arrays** — contact
   zones are validated by `chrono-tz`, so Postgres's tz database must not decide them — none.
4. **Ruling: `recipient_local` requires the earliest instant (the UTC+14 zone) to be in the
   future at schedule time, and refuses A/B (422)** — every zone then gets its local time, and a
   test cohort spread over 26 hours has no meaningful winner time — operators wanting both run
   two campaigns.
5. **Ruling: `send_at = null` means now; the launch job's `run_after` is `send_at` (or the
   earliest instant)** — scheduling is the jobs table's own `run_after` — none.
6. **Ruling: resume sends overdue recipient-local messages immediately** — skipping them loses
   mail — some recipients get it outside their local time; the resume dialog says so.

### 4.4 From address (issue 41)

- **Ruling: a campaign carries `sending_domain_id`, `from_local_part`, `from_name` (per variant
  overridable) and optional `reply_to`; header From and envelope From are
  `{from_local_part}@{domain}`** — closes issue 41's campaign half, and OCI requires envelope
  From = header From — none.
- **Ruling: confirmation emails keep M2a's alphabetical shortcut; issue 41 stays open, narrowed
  to the transactional default, for M5** — the default flag needs its own delete rule and screen —
  a two-domain workspace's confirmations still come from the alphabetically first domain.
- **Ruling: the M2a domain gate (all DNS checks pass, or an admin acknowledged) applies at
  schedule, at launch and per send batch** — one gate, read where the send happens — none.

### 4.5 Content: freeze, footer, unsubscribe link (issues 48, 49, 51)

1. **Ruling: at launch, each variant's template is compiled fresh from its authored document
   against the current brand kit and fields, with the system footer, and stored in
   `campaigns.frozen`; the send path only substitutes into that frozen rendition through
   `Ready::render`** — `mrml` never runs per contact, and a template or kit edited after launch
   cannot change an in-flight campaign — a fix to a typo after launch needs cancel and a new
   campaign.
2. **Ruling (issue 49): recompile on read, before the send path, in memory, never written
   back.** The freeze always compiles. A stored-template preview whose rendition is `Stale` or
   `MarkerFormat` recompiles in memory instead of answering 409. `FieldsChanged` still refuses
   (409 `template-stale`) until a human re-saves — a kit or renderer change does not change what
   a tag means, a field-kind change does — a preview after a kit edit pays one `mrml` render
   (limited to 120/min).
3. **Ruling: the freeze and every compile outside a save run in `spawn_blocking`, outside any
   transaction** — issue 50 item 3's lesson, not repeated — none.
4. **Ruling (issue 51): every campaign rendition carries a product-owned system footer, inserted
   into the HTML after `mrml` and before classification, immediately before the document's real
   end-of-body as html5ever's tokenizer sees it (appended at the end when there is none).** It
   holds the unsubscribe link and, for raw `mjml`/`html` kinds, the postal address (a `blocks`
   template already emits it from the brand kit). — The footer is inside the classified document,
   so nothing is appended after classification, and no operator layout is guessed at — a raw
   template whose end is inside an unclosed comment or script fails to freeze with
   `CompileError::FooterHidden`.
5. **Ruling: schedule, launch and test send refuse when the brand kit's `footer.address` is blank
   (`409 postal-address-required`)** — CAN-SPAM requires it in every marketing message — an admin
   must set it before the first send.
6. **Ruling (issue 48 and ruling 4 of the brief): the unsubscribe URL is
   `{NM_PUBLIC_URL}/u/{token}`, absolute and https.** In the footer the base is a literal; the
   token is a product-only slot, `unsubscribe_token`, in a URL path position, which no operator
   registry can name. The token is issued per message by `public::token::issue` with
   `purpose = Unsubscribe` and `ref_id = campaign_id`. — The URL is never built from merge-tag
   output, and `consent_log.source_ref` names the campaign — none.
7. **Ruling: the footer's exact markup is fixed** (centred, 12px, `#6b7280`, link text
   "Unsubscribe"); the text part gets `Unsubscribe: {url}` and, for raw kinds, the address.
   English-only — multi-language is "later" in spec §6 — non-English senders see one English word.
8. **Ruling: subjects take merge tags in the same grammar, compiled at save and at freeze,
   rendered as plain text with every control character replaced by a space** — "Hi {{
   first_name }}" is ordinary marketing copy, and header injection is closed at render — none.
9. **Ruling: the one-click endpoint is M1d's `POST /u/{token}`, unchanged (workspace-wide opt-out
   and every list)** — it already implements RFC 8058 and the ADR 0005 order; M2c owes the
   per-recipient link and the end-to-end proof — a one-click unsubscribes from the whole
   workspace, not one list.

### 4.6 Headers

- **Ruling: every campaign message carries `From`, `To`, `Reply-To` (when set), `Subject`,
  `Date`, `Message-ID: <{messages.id}@{sending domain}>`,
  `List-Unsubscribe: <https://…/u/{token}>`, `List-Unsubscribe-Post: List-Unsubscribe=One-Click`
  and `Feedback-ID: {campaign_id}:{variant}:{workspace_id}:nm`; app-signed domains sign
  `SIGNED_HEADERS` plus `Reply-To`** — a deterministic `Message-ID` lets M3 correlate a DSN to a
  message, and the Feedback-ID groups by campaign as Gmail's loop wants — none.
- **Ruling: no `mailto:` in `List-Unsubscribe`** — M2a's ruling, and a mailto that nothing reads
  is a lost unsubscribe — none.

### 4.7 The pipeline

1. **Ruling: `messages` is its own queue, claimed with `0026`'s protocol shape (lease in
   `run_after`, `for update skip locked`, `attempts < max_attempts`); `jobs` carries only control
   work (`campaign_launch`, `campaign_test`)** — one row per recipient rather than two, and the
   unique key that enforces invariant 1 sits on the row being claimed — a second claim statement,
   held clause by clause by its own test as `CLAIM` is.
2. **Ruling: a tick claims `CAMPAIGN_BATCH = 500`, split evenly across the sending campaigns by a
   per-campaign lateral claim, ordered by `(run_after, id)` within each** — a 5M campaign does not
   starve a small one, and each lateral walks its own index range — none.
3. **Ruling: write-ahead. Before SMTP, one committed statement moves the message `queued →
   submitting`, fenced by the claim's `attempts`; a `submitting` row is never claimed; one whose
   lease expired is swept to `unknown` and never retried** — closes the crash-after-accept double
   send that a lease alone leaves open — one extra write per message, and a crash between accept
   and record is recorded `unknown` though it was delivered.
4. **Ruling: outcomes, with the new `Outcome::Deferred`:**

   | relay | outcome | message | attempt spent |
   |---|---|---|---|
   | 2xx | `Sent` | `sent` | — |
   | 4xx about this message | `Transient` | `queued`, backoff (M2a's) | yes; the 5th fails it |
   | 5xx, or the suppression notice | `Failed` | `failed` | — |
   | broke after `DATA` | `Unknown` | `unknown`, never retried, never failed over | — |
   | `455`, or never connected | `Deferred` | released, next relay tried | **no** |

   A `Deferred` relay's bucket is drained for 60 s, or 1 h when the reply says "per day". —
   Relay-level trouble (OCI's daily 50,000) must hold mail, not destroy it (issue 42) — a relay
   that is down for good keeps mail queued for good, visible as a queue that does not move.
5. **Ruling: eligibility is re-checked once per batch, by one query over `mailable_list_members`
   for the campaign's lists, joined by contact; a message that fails it is `skipped`** — spec §7
   step 3 — an unsubscribe landing during a batch can miss that batch, which is at most one tick
   budget (60 s).
6. **Ruling: outcomes are recorded in one statement per flush (every 250 ms or 100 outcomes),
   and the campaign's counters and finish check move in the same transaction** — spec §7 step 6,
   and progress never scans `messages` — a crash loses at most one flush of outcomes, which the
   sweep records as `unknown`.
7. **Ruling: pause is read from a campaign-state map the worker refreshes every 1 s; each send
   checks it immediately before `submitting`, and an unstarted message is released** — invariant
   5's 5 s with margin — an SMTP session already past `submitting` completes.
8. **Ruling: the worker runs two loops and a refresher: the jobs loop (confirmation, test,
   launch; concurrency `NM_WORKER_CONCURRENCY`) and the messages loop (concurrency
   `NM_WORKER_CAMPAIGN_CONCURRENCY`, default 32); `NM_WORKER_POOL_SIZE` default rises from 5 to
   10** — a campaign tick must never sit in front of a double opt-in email (M2a §5.3) — one more
   setting to size.
9. **Ruling: per tick, the messages loop caches frozen content (keyed by campaign id and
   `frozen_at`), the sending domain with its DKIM key, the relays with their passwords, and the
   field definitions** — M2a reads the key and password per message, which is 3,000 decrypts a
   second at §9's rate — a relay password changed mid-tick applies from the next tick.
10. **Ruling: the worker system-pauses a sending campaign (with `pause_reason`) when its sending
    domain is gone or fails the gate, or its frozen rendition reports field drift; resume
    re-freezes only when the frozen rendition no longer passes `ready()`** — each is fixable by
    an operator, so the campaign waits rather than fails — the remainder after a drift-resume is
    the re-saved template.
11. **Ruling: a launch refusal (list, segment, template or domain gone; no postal address;
    template no longer compiles) ends a `scheduled` campaign `failed` with `failure_reason`; the
    same refusal in phase 2 of an A/B campaign, which has already sent, is a system pause** —
    nothing sent means nothing to resume, something sent means the operator must decide — a failed
    campaign is re-created, not resumed.
12. **Ruling: cancel touches no message rows; the claim never selects a cancelled campaign** — a
    5M-row update is the cost of the alternative — queued rows stay queued; the view reports them
    as not sent.

### 4.8 Test sends

- **Ruling: a test send goes to 1–5 explicit addresses, editor and above,
  `TEST_SEND_PER_MINUTE = 5` per workspace; it is rendered at request time with the preview's
  sample contact, subject prefixed `[Test] `, Feedback-ID first field `test`, and enqueued as one
  `campaign_test` job per address carrying the rendered parts** — no subscriber's data leaves in
  a test, and `mrml` stays off the worker's per-message path — the test shows sample values, not
  a real person's.
- **Ruling: a test honours suppressions but not consent or audience; its unsubscribe link is
  `{NM_PUBLIC_URL}/u/test`, which answers "no longer valid"** — an address that complained must
  not be mailed even as a test — an operator who suppressed their own address cannot test to it.

### 4.9 A/B

- **Ruling: a variant sets `subject`, `template_id` and optional `from_name`; a campaign has 1–4
  variants. With more than one, `ab_test_percent` (10–100) of the audience is sampled
  deterministically and split evenly; 100 is a pure split with no winner phase** — spec §6 names
  subject, content and sender — none.
- **Ruling: sampling is `(hashtextextended(contact_id::text, seed) & 9223372036854775807) % 100 <
  ab_test_percent`, variant `… (seed + 1) … % n`, seed the campaign id's first eight bytes** —
  deterministic, so a re-run batch assigns the same variant — the cohort is approximately, not
  exactly, the percentage.
- **Ruling: M2c picks no winner automatically. Below 100, the remainder waits until an admin
  posts `…/winner`; phase 2 then resolves the whole audience with that variant and
  `on conflict do nothing`** — without tracking there is no honest metric (delivery is not a
  content signal); M2d adds the auto-winner on clicks — the remainder waits for a human, and
  contacts who joined since phase 1 get the winner.

### 4.10 Issues and acceptance scope

- **Ruling (issue 50): the template list returns `TemplateSummary` (`id, name, kind, created_at,
  updated_at`); item 3 (save compiles inside a transaction) stays open** — the campaign picker is
  a new caller, and the screen reads only those fields — none.
- **Ruling (issue 43): M2c does not wait on it and assumes no answer**: signing stays per domain,
  our https header is always set, `SUPPRESSION_NOTICE` is unchanged. M2c adds question 7 to the
  issue (a real Gmail one-click on a campaign message lands in `consent_log` with
  `source_ref = campaign id`). M2 sign-off waits on all seven — none of them can be answered
  without the live tenancy.
- **Ruling: §9's throughput is proved by `campaign_bench` against the in-process sink:
  ≥ 1,500 messages/s sustained, personalised and DKIM-signed, one worker, on the 8 vCPU
  reference box; launch of 100,000 recipients recorded** — OCI is 300/s (§13.7) and cannot prove
  it — the dev box's number is recorded, not gated.

### 4.11 Schema and lock order

- **Ruling: migration `0033_campaigns_and_messages.sql` adds `campaigns`, `messages` and
  `jobs.campaign_id`** (shape in §5).
- **Ruling: `messages → campaigns` is `on delete no action`; `jobs → campaigns` is `on delete
  cascade`; `workspaces → campaigns` cascades** — a campaign with messages cannot be deleted by
  anything, while a draft's test jobs go with it — none.
- **Ruling: `campaigns` is a new FK parent and not a lock class.** Its only deleter is
  `delete_campaign`, whose `where state = 'draft'` cannot match a campaign with messages or a live
  launch job; every child insert takes `for key share` once, at the end of one statement; no
  worker update of a child writes `campaign_id`. — The ADR's method — the day sent campaigns
  become deletable, it is re-ruled.
- **Ruling: every multi-statement transaction on these tables takes `messages`, then `campaigns`,
  then `jobs`** — the launch batch, the outcome flush, the sweep and the slice hand-off all write
  in that order — a path that cannot is listed in the ADR with its reason.

## 5. Schema (`0033`)

```
campaigns  id uuid pk, workspace_id → workspaces cascade, unique (workspace_id, id),
           name text, state text check (the seven), list_ids uuid[] default '{}',
           segment_id uuid, sending_domain_id uuid, from_local_part text, from_name text,
           reply_to text, variants jsonb array default '[]', ab_test_percent smallint
           check 10..100, winner smallint, schedule_mode text check ('instant','recipient_local'),
           schedule_zone text, schedule_local timestamp, send_at timestamptz,
           launch_job_id uuid, phase smallint default 1 check (1,2), resolve_cursor uuid,
           resolved bool default false, frozen jsonb, frozen_at timestamptz,
           total/sent/failed/unknown/skipped bigint default 0, failure_reason text,
           pause_reason text, created_at, updated_at, launched_at, finished_at
           index campaigns_live (state) where state in ('sending','paused')
messages   id uuid pk (v7), workspace_id, campaign_id, contact_id (no fk), variant smallint,
           status text check ('queued','submitting','sent','failed','unknown','skipped'),
           attempts int ≥ 0, max_attempts int > 0 default 5, run_after timestamptz,
           relay_id uuid, response text, error text, created_at, sent_at,
           unique (campaign_id, contact_id),
           fk (workspace_id, campaign_id) → campaigns no action
           index messages_runnable (campaign_id, run_after, id) where status = 'queued'
           index messages_in_flight (run_after) where status = 'submitting'
jobs       + campaign_id uuid, fk (workspace_id, campaign_id) → campaigns cascade,
           check (kind not in ('campaign_launch','campaign_test') or campaign_id is not null)
```

## 6. Correctness invariants

Each has a test that fails when the behaviour is removed.

1. A contact is submitted at most once per campaign, across retries, crashes, lease expiry and
   two workers.
2. A contact not mailable at its batch's check is never submitted.
3. Every campaign message carries both unsubscribe headers pointing at an absolute https
   `/u/{token}`, and the same URL in the HTML and text body; app-signed, `h=` covers both.
4. Every campaign message carries the brand kit's postal address.
5. Pause stops new submissions within 5 s.
6. `unknown` is never retried or failed over; `Deferred` spends no attempt.
7. Every sent rendition was produced by the full compile pipeline, footer included, and
   classified; nothing is appended after classification.
8. A template or brand-kit edit after launch does not change the campaign.
9. Another workspace's campaign is 404; a message cannot name another workspace's campaign.

## 7. Review Focus — five input classes no happy path exercises

1. **A contact unsubscribes mid-send** (one-click on an earlier message of the same campaign).
   The remaining message must be skipped, and `consent_log.source_ref` must be the campaign.
2. **The template or the brand kit is edited mid-send**, and separately between schedule and
   launch. The first must not reach the campaign; the second must not refuse the launch
   (issue 49).
3. **A relay dies or throttles mid-batch**: a socket broken after `DATA` (`Unknown`, never
   retried, never offered to relay 2), a refused connection (`Deferred`, fails over with a
   token), and `455` (released, attempt intact, bucket drained).
4. **Two workers, and a worker that dies between the relay's 250 and the record.** Exactly one
   submission per message; a stale lease holder cannot mark `submitting`; the sweep makes the
   orphan `unknown`.
5. **A schedule across a DST change**: `America/New_York` 2027-03-14 02:30 (skipped) and
   2027-11-07 01:30 (ambiguous), in both modes, and a recipient-local send whose contacts span
   zones on both sides of the change.

## 8. Acceptance

- An admin schedules a campaign to two lists narrowed by a segment; at `send_at` the worker
  launches; every mailable contact receives exactly one message through a sink relay, with the
  §4.6 headers, the address and the link; a one-click from a received message suppresses the
  contact and attributes the campaign; pause, resume and cancel work from the screen.
- Every §6 invariant has a red-first test; issues 48, 49 and 51 each have theirs.
- `campaign_bench` meets §4.10 on the reference box; its dev-box number is in the PR.
- The full gate is green, serialised: `cargo fmt --check`, `clippy -D warnings`, `cargo test`,
  `sqlx prepare --check`, OpenAPI drift, `cargo deny`, `pnpm build`, `pnpm test`,
  `pnpm check:bundle`, e2e.
- ADR 0005 has a row for every new write path and the enumeration re-run for `campaigns`.
- **Not required for M2c:** issue 43's answers (§4.10).

## 9. Known constraints inherited

- Every M2b constraint (tenancy 404, secrets, container-only cargo, `issues/40`).
- M2a's confirmation path keeps its crash-after-accept window; M2c's write-ahead is not
  retrofitted there.
- OCI: 50,000/day and 300/s; envelope From = header From; 2 MB message limit.
- `issues/27` (lifting a bounce can lose a one-click) is unchanged and still open.
- `issues/33`: the worker pool is now shared by two loops; its default rises to 10.
