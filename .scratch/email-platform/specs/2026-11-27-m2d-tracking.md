<!-- SPDX-License-Identifier: AGPL-3.0-only -->

# M2d — open and click tracking, send-time bounces, the campaign report

**Milestone:** M2 Sending core, fourth slice. A launch cut: two to three days of work.
**Follows:** M2c (campaigns and the send pipeline). **Precedes:** M3 (feedback loop).
**Sources:** `spec.md` §6 (analytics, A/B), §7 (sending pipeline step 6, public endpoints,
tracking, data model, privacy), §9 (targets); M2c spec §4.2 ruling 5, §4.5, §4.7, §4.11;
ADR 0005; issues 27, 43, 52.

This spec is the decisions. Each is `Ruling: what — why — cost if wrong`.

---

## 1. Intent

An operator who sends a campaign sees what happened to it: how many messages went, failed and
hard-bounced, how many recipients opened and clicked, which links were clicked, and how many
unsubscribed. A recipient whose mailbox does not exist is suppressed the moment the relay says
so, and is never mailed from the workspace again.

## 2. Non-goals

- **Asynchronous bounces and complaints from the relay** (OCI Logging, DSN mailboxes, webhooks).
  They depend on issue 43's live-tenancy questions, still unanswered. M3.
- **Bot and Apple Mail Privacy Protection filtering.** Opens are labelled approximate on screen and
  nothing is filtered. M3.
- **Automatic pause on bad signals** (complaint or bounce rate). M3.
- **A/B automatic winner.** M2c §4.9 promised it for M2d on clicks; it moves out with the rest of
  the engagement logic. The manual winner pick stays, and the report's per-link rows carry the
  variant.
- **Custom tracking domains.** Tracking uses `NM_PUBLIC_URL`.
- **Per-contact activity timelines**, and **analytics beyond the campaign report** (breakdown by
  mailbox provider, time series, workspace dashboards).
- **Switching tracking off.** `spec.md` §7 (Privacy) wants open and click tracking switchable per
  workspace and per campaign. M2d tracks every campaign launched after it ships; the switches are
  a later slice.
- **Tracked links in a `blocks` template's plain-text part.** That part is compiled from the block
  tree, not from the HTML, and keeps the original URLs.
- **A buffered event writer and running totals.** `spec.md` §7 wants opens and clicks buffered in
  memory, and its data model wants dashboards read from running totals. M2d writes each event in
  its request and counts the report per request (rulings 4.2.3 and 4.4.1).
- **Bounce intake for confirmation emails and test sends.** Both record a hard bounce as `failed`
  and suppress nothing.

## 3. Anti-goals

1. A tracked link that sends a recipient somewhere the operator never wrote: an open redirect.
2. A tracked unsubscribe link, or a `List-Unsubscribe` header and a body link that disagree.
3. An email address, or any contact value, in a token, a tracked URL or a stored tracking row.
4. A count that counts one recipient twice.
5. A relay misconfiguration that suppresses the audience.

## 4. Rulings

### 4.1 The tracking token and the public routes

1. **Ruling: one token per message, `base64url(message_id ‖ mac)`, 43 characters, where `mac` is
   the first 16 bytes of HMAC-SHA256 over the message id's 16 bytes under the subkey
   `secrets.derive_key("nm:track-token:v1")`. No TTL and no key id.** — It names the message and
   nothing else, so an erased contact leaves only a uuid (M2c §4.2 ruling 5); it is compact enough
   to repeat on every link; its own subkey means it cannot be replayed as an unsubscribe or confirm
   token, nor those as it. It is deterministic, so every attempt at one message carries the same
   one — a master-key rotation would break every tracked link in mail already sent. Rotation does
   not exist yet; whoever builds `nm keys rotate` keeps this subkey's old key.
2. **Ruling: `GET /o/{token}` is the open pixel and `GET /l/{token}/{link}` the click redirect.**
   `/c/` is already M1d's confirm route (`public::PREFIXES`), so clicks take `/l/`. `spec.md` §7's
   `/t/o/` and `/t/c/` are superseded. Both prefixes join `public::PREFIXES`, so they are never the
   SPA and never cached by a shared proxy. — none.
3. **Ruling: one per-IP limiter for both routes, `track_limit`, keyed like `public_limit` (the /64,
   `MAX_KEYS`), `TRACK_PER_MINUTE = 600`, with its own map.** Over budget, the pixel still answers
   its GIF and records nothing; a click answers `429` with `Retry-After` and the `GET /u/` refusal
   page. — A burst after a send must not spend the confirm and unsubscribe budget (60/min), and a
   link scanner or an office behind one address makes far more than 60. — Behind a reverse proxy
   with `NM_TRUST_PROXY_HEADERS` off, every click on the internet shares one 600/min budget: M1d's
   known trap, now reaching links. Apple's image proxies may exceed it and lose opens.
4. **Ruling: the pixel answers byte-identically for a forged, an unknown and a real token: `200`,
   `Content-Type: image/gif`, the 43-byte transparent GIF89a, `Cache-Control: no-store`, and no
   `RateLimit` headers, whether or not the request was within budget.** — An
   image that fails differently is a validity oracle, and an error image shows a broken-image icon
   in the recipient's mail. — none.
5. **Ruling: a click redirects (`302`, `Location`) only when the MAC verifies, the message exists,
   and `link` indexes an `http` or `https` URL in that message's own campaign variant's frozen link
   table (ruling 4.3.3). Every other case — bad MAC, no such message, index past the table, a
   stored URL that does not re-parse as `http`/`https` — answers the same `404` page with no
   `Location`.** Nothing in the request but the token and the index is read; a query string is
   ignored. — There is no URL in the token to forge, and the table is written only by the freeze.
   — A recipient whose client truncated the link sees "This link is not valid".
6. **Ruling: a `HEAD` is answered as the `GET` would be and records nothing.** — Link scanners
   probe with `HEAD`; a probe is not a person. — none.

### 4.2 Open tracking

1. **Ruling: the pixel's markup is frozen into the system footer, immediately after the
   unsubscribe link; its token is a second product-only slot, `tracking_token`, filled at render.**
   Spec M2c §4.5 ruling 7's fixed footer gains one element (exact markup in §5.3). — The footer is
   already inside the classified document and already refused inside a conditional
   (`RenderError::FooterInBlock`), so every message carries the pixel and nothing is appended after
   classification (M2c invariant 7). `mrml` never runs on the send path. — A campaign launched
   before M2d has no pixel and is not tracked.
2. **Ruling: the slot is the product's alone, exactly as `unsubscribe_token` is.** Only
   `tag::Registry::system_footer()` knows it; `from_definitions` skips a custom field keyed
   `tracking_token` (issues/44); `Contact::value` answers it before `attributes`. A message with no
   tracking token (a test send, a preview) fills it with `test`, which the pixel answers like any
   unknown token. — none.
3. **Ruling: an open is one statement on the `messages` row, in the request: `opens = opens + 1,
   opened_at = coalesce(opened_at, now())`.** — `shortcut: no in-memory buffer (spec.md §7); add
   one when §9's 5,000 req/s is measured, in M7.`

### 4.3 Click tracking

1. **Ruling: links are rewritten at render time, inside the classifier's serialiser, not at
   freeze.** The serialiser already rewrites every static URL attribute per message
   (`static_url`: canonicalise, or `#` for a refused scheme), so tracking is one branch in the
   function that owns static URLs, emitting through the same `escape_attr`. Rewriting at freeze
   would mean splicing a slot into the operator's markup before classification, and html5ever
   reports no byte offsets — `footer.rs` needed a probe trick to find one position; every link
   would need a second serialiser. The link table is still built at freeze, by the same predicate
   over the same stored document, so a render cannot track a link the freeze did not see. — A URL
   parse per link per message, which `static_url` already pays.
2. **Ruling: a link is tracked when it is the `href` of an `a` or `area` element, holds no merge-tag
   marker, parses as an absolute `http` or `https` URL, and does not start with
   `{NM_PUBLIC_URL}/`.** Never rewritten: the unsubscribe link (it holds a marker and sits under the
   public URL — either rule excludes it), anything else under the public URL (an operator's
   hard-coded `/u/` or confirm link), `mailto:`, `tel:`, `#` anchors, relative URLs, links built
   from merge tags (their table entry would hold contact data), and hrefs inside `<!--[if mso]>`
   comments (a comment is not a tag to the tokenizer, so Outlook desktop's copy of an `mj-button`
   link stays direct). — Outlook desktop button clicks go uncounted.
3. **Ruling: each send rendition carries `links`, the distinct canonical URLs its tracked hrefs
   point at, in first-appearance order; a link's index is its URL's position. At render, a static
   `a` or `area` href is rewritten exactly when its canonical URL is in the table, and the base is
   the rendition's own frozen unsubscribe prefix less `/u/`, so a rendition frozen before M2d (empty
   table) renders untracked and the base cannot drift from the footer's. A re-freeze (resume
   after field drift, M2c §4.7.10) starts from the previous rendition's table for the same variant
   and only appends.** — An index in mail already sent must keep naming the same URL after a
   resume; without this a click on an earlier message could land on a different link. Two hrefs to
   one URL share an index, so the report's "clicks per link" is per URL. — none.
4. **Ruling: the tracked URL is `{NM_PUBLIC_URL}/l/{token}/{index}`.** — none.
5. **Ruling: a click is one statement, in the request: the `messages` row (`clicks = clicks + 1,
   clicked_at = coalesce(clicked_at, now()), opened_at = coalesce(opened_at, now())`), then an
   upsert of `message_clicks (message_id, link)` (`clicks = clicks + 1, last_at = now()`). The
   lookup before it is a plain read.** A click implies an open, so a recipient whose client blocks
   images still counts as opened; `opens` is not incremented by a click. — Same shortcut as 4.2.3.

### 4.4 Counting

1. **Ruling: unique opens and unique clicks are counts of `messages` rows whose `opened_at` /
   `clicked_at` is set, for the campaign, computed per report request. Per link, clicks are
   `sum(message_clicks.clicks)` and unique clicks `count(*)`, by `(variant, link)`.** A unique
   figure is never derived from an incremented counter. — `shortcut: the report reads messages per
   request; move to running totals (spec.md §7 `campaign_stats`) when a report is measured slow,
   which is M3's work.` A 5M-message campaign's report is a multi-second read.
2. **Ruling: unsubscribed is `count(distinct contact_id)` of `consent_log` rows with `action =
   'unsubscribed'` and `source_ref` equal to the campaign id.** The one-click and the preference
   centre already write `source_ref` from the unsubscribe token's `ref_id` (M2c R6). A contact who
   left two lists counts once. — Erasure cascades `consent_log`, so an erased contact's unsubscribe
   drops out of the count.

### 4.5 Send-time hard bounces

1. **Ruling: a hard bounce is a permanent (5xx) SMTP reply whose text carries one of the enhanced
   status codes that name the recipient's mailbox or domain: `5.1.1`, `5.1.2`, `5.1.3`, `5.1.6`,
   `5.1.10`, `5.2.1` (RFC 3463, RFC 7505), matched as a whitespace-separated word.** The transport
   classifies it as a new `Outcome::Bounced`. Every other 5xx stays `Failed` — `535` authentication,
   `5.7.x` policy, `552`/`5.3.4` size, `5.1.7`/`5.1.8` sender, a bare `550` with no enhanced code.
   A 4xx is never a bounce, whatever its enhanced code. — lettre does not say which command a reply
   answered, and a relay that refuses every message (bad credentials, a blocked sender) must never
   turn into a suppressed audience (anti-goal 5). — A server that rejects a dead mailbox with a bare
   `550` records `failed` and suppresses nothing.
2. **Ruling: a bounce is recorded by the flush that records its outcome, in one transaction:
   `suppressions::record_bounces` first, then the `messages` update to `bounced` (from
   `submitting`, fenced by the claim token like every verdict), then `campaigns.bounced`.** The
   suppression is a workspace `address` row, reason `bounced`, source `send`, note
   `hard bounce {code} at send, campaign {campaign_id}`, `on conflict do nothing` (an address
   already blocked keeps the row it has, issues/27). A suppression is written even when the
   message's row has meanwhile been swept `unknown`: the relay's answer is a fact about the address.
   — none.
3. **Ruling: `finish_if_done` checks `total = sent + failed + bounced + unknown + skipped`.** — A
   campaign whose last message bounced must finish. — none.
4. **Ruling: confirmation emails and test sends map `Outcome::Bounced` exactly as they map
   `Failed`, and suppress nothing.** — none.
5. **Ruling: OCI's `250` suppression notice stays `failed`.** It is not a 5xx, and what it means is
   one of issue 43's open questions. — An address on OCI's own list is retried by every campaign
   until M3.
6. **Ruling: every relay reply `record` stores on a message (`response`, `error`, every verdict) has
   each whitespace-separated word containing `@` replaced by `[address]`.** Relays echo the
   recipient (`550 5.1.1 <alice@example.com>: no such user`), and `messages` outlives erasure
   (M2c §4.2 ruling 5). — none; rows written before M2d keep what they have.
7. **Ruling: the lock order across both families is the address family first, then the campaign
   family: `A, L, S, contacts, M, C`, then `messages, campaigns, jobs`.** `record` takes `A` once
   per bounced address in ascending lock-key order (the `lock_address` SHORTCUT's bulk rule), then
   `S`, then `messages`, then `campaigns`. No path takes a campaign-family row and then an
   address-family one. — A second multi-address writer that locks out of key order deadlocks with
   this one.

### 4.6 The campaign report

1. **Ruling: `GET /api/v1/workspaces/{workspace_id}/campaigns/{id}/report`, viewer and above,
   answers `CampaignReport` (§5.2). Another workspace's campaign is `404`, never `403`, never
   data.** It runs `campaign::load` first, then reads with `workspace_id` and `campaign_id` in every
   predicate. — none.
2. **Ruling: the report lists every link in every variant's frozen table, with zeros where nobody
   clicked, ordered by `(variant, link)`.** A draft, a scheduled campaign and one launched before
   M2d answer zeros and an empty list. — none.
3. **Ruling: the progress view shows the report once the campaign has launched (`launched_at` is
   set), fetched when the view opens and on a Refresh button, never polled.** The opens line says
   the figure is approximate. — none.

### 4.7 Schema and lock order

1. **Ruling: migration `0035_tracking_and_bounces.sql`** (shape in §5.1).
2. **Ruling: `message_clicks` has no foreign key.** It is written only from a `messages` row in the
   same statement, and nothing deletes `messages`. A key to `messages` would make it a twelfth
   parent; a key to `campaigns` a third child edge. `0035` therefore adds no edge, and
   `the_foreign_key_graph_is_the_one_the_adr_rules_on` stays as it is. — none.
3. **Ruling: the open and the click are leaves.** One statement each, on one `messages` row (and,
   for a click, then one `message_clicks` row, which nothing references), writing no key column and
   asking for nothing after. — none.

## 5. Shapes

### 5.1 Schema (`0035`)

```
messages        status check gains 'bounced' (messages_status_check, dropped and re-added)
                + opened_at timestamptz, opens int not null default 0,
                + clicked_at timestamptz, clicks int not null default 0
campaigns       + bounced bigint not null default 0
message_clicks  message_id uuid, link int, workspace_id uuid not null, campaign_id uuid not null,
                variant smallint not null, clicks int not null default 1,
                first_at timestamptz not null default now(), last_at timestamptz not null default now(),
                primary key (message_id, link)                  -- no foreign keys (ruling 4.7.2)
                index message_clicks_by_campaign (campaign_id)
consent_log     index consent_log_by_source_ref (workspace_id, source_ref) where source_ref is not null
```

Re-adding the status check scans `messages` under an exclusive lock, and the `consent_log` index
blocks its writes while it builds. Both tables are small at launch.

`Stored` (the JSON in `campaigns.frozen`'s `variants[i].rendition`) gains `links: [string]`,
serialised only when non-empty and read with a default, so a rendition frozen before M2d parses.
The click lookup reads it as `frozen #>> array['variants', variant, 'rendition', 'links', link]`.

### 5.2 Wire

```
CampaignCounts  + bounced: i64.   queued = total - sent - failed - bounced - unknown - skipped
GET …/campaigns/{id}/report  -> 200 CampaignReport   operation_id report_campaign
CampaignReport  { sent: i64, failed: i64, bounced: i64, unique_opens: i64, unique_clicks: i64,
                  unsubscribed: i64, links: LinkReport[] }
LinkReport      { variant: i16, link: i32, url: string, clicks: i64, unique_clicks: i64 }
                  variant and link are 0-based; ordered by (variant, link)
```

### 5.3 The footer, exact

```
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0"><tr><td align="center" style="padding:16px 24px;font-family:Arial,Helvetica,sans-serif;font-size:12px;line-height:18px;color:#6b7280;">{ADDRESS}<a href="{BASE}/u/{UNSUBSCRIBE_TOKEN}" style="color:#6b7280;text-decoration:underline;">Unsubscribe</a><img src="{BASE}/o/{TRACKING_TOKEN}" width="1" height="1" alt="" style="display:block;width:1px;height:1px;border:0;"></td></tr></table>
```

`{ADDRESS}` and `{BASE}` are unchanged from M2c. The plain-text part gains nothing.

## 6. Correctness invariants

Each has a test that fails when the behaviour is removed.

1. A click redirects only to an `http`/`https` URL in the frozen link table of that message's own
   campaign and variant; anything else is the same `404` with no `Location`.
2. The unsubscribe link is never rewritten: `List-Unsubscribe` and the body link are the same
   `/u/` URL, and no tracked link points under `NM_PUBLIC_URL`.
3. No token, tracked URL or `message_clicks` row carries an address or a contact value, and no
   relay reply stored on a message carries an address.
4. A message adds at most one to unique opens and one to unique clicks, however often it is opened,
   clicked, probed with `HEAD`, or its flush replayed.
5. A hard bounce is `bounced` and suppressed in one transaction; a 4xx, or a 5xx with no
   destination code, never suppresses.
6. The pixel's response is byte-identical for a forged, an unknown and a real token.
7. Another workspace's report is `404`.
8. A re-freeze keeps every earlier link index naming the same URL.
9. M2c's invariants still hold; `campaign_send`, `campaign_queue`, `campaign_launch` and
   `lock_order` stay green.

## 7. Review Focus — the five likeliest failures

1. **Open-redirect abuse.** `/l/{valid}/0?url=https://evil.test`; `/l/{forged}/0`;
   `/l/{valid}/999`; campaign A's token with an index only campaign B has; a frozen link edited in
   the database to `javascript:alert(1)` or `//evil.test`; an operator link to
   `https://mail.example.test.evil.test/`, which is not under the public URL and is tracked.
2. **Tracking the unsubscribe link.** The footer link; an operator's hard-coded
   `{NM_PUBLIC_URL}/u/x`; the `List-Unsubscribe` header; the plain-text part; an `mj-button` whose
   href is the public URL.
3. **An address leaking into tokens or stored text.** A contact `alice+x@example.test`; a relay
   reply echoing `<alice+x@example.test>`; an href `https://x.test/?e={{ email }}`; the suppression
   note; `message_clicks` after the contact is erased.
4. **Double-counting.** Two opens; a `HEAD` then a `GET`; two clicks on one link; one click on each
   of two links; a click with no open; a replayed flush carrying a bounce; a contact unsubscribing
   from two lists; a bounced message read as both `failed` and `bounced`.
5. **The bounce path's lock order.** Two flushes bouncing addresses X and Y in opposite orders; a
   bounce flush and a one-click for the same address, both arrival orders; a bounce flush and a
   system pause.

## 8. Acceptance

- A campaign sent through a sink relay carries a pixel and tracked links; fetching the pixel and a
  link through the app records one unique open and one unique click on that link, and the report
  and the progress view show them.
- A sink answering `550 5.1.1` leaves the message `bounced`, the address suppressed with reason
  `bounced`, the campaign finished, and the next campaign skipping that contact.
- Every §6 invariant has a red-first test.
- ADR 0005 has a row for every new or changed write path and states the cross-family order; the
  enumeration is re-run against `0035`.
- The full gate is green, serialised: `cargo fmt --check`, `clippy -D warnings`, `cargo test`,
  `sqlx prepare --check`, OpenAPI drift, `cargo deny`, `pnpm build`, `pnpm test`,
  `pnpm check:bundle`, e2e.
- `campaign_bench` re-run once; its number is in the PR, not gated.

## 9. Known constraints inherited

- Every M2c constraint (tenancy 404, secrets, container-only cargo, OCI's 50,000/day and 300/s).
- Issue 43: OCI accepts and reports bounces asynchronously, so a send through OCI may record few or
  no send-time bounces. The feature is proved against the sink.
- Issue 52: Gmail clips a message over 102 KB and the pixel goes with the footer, so opens of large
  messages are undercounted.
- Issue 27: a bounce lands on an address already blocked for another reason as nothing.
- M2c §4.10's throughput target now includes a token MAC per message and a table lookup per link.
