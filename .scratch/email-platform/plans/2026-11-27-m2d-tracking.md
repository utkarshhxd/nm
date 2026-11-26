# M2d — open and click tracking, send-time bounces, the campaign report: implementation plan

**Goal:** Every campaign message launched after M2d carries an open pixel and tracked links; a
recipient's mailbox that the relay says does not exist is suppressed at once; the progress view
shows sent, failed, bounced, unique opens, unique clicks, unsubscribed and clicks per link.

**Architecture:** The pixel is frozen into the system footer as a second product-only slot; links
are rewritten per message by the classifier's own serialiser against a per-variant link table built
at freeze. The token names the message only (`base64url(id ‖ mac)`), so events key on the message
id. Two public routes, `/o/{token}` and `/l/{token}/{link}`, write one statement each. A permanent
reply naming the recipient becomes `Outcome::Bounced`, recorded and suppressed in the flush's one
transaction. The report counts per request. Two lanes: **Lane S** (`src/`, `migrations/`, `tests/`,
`docs/`, `.sqlx/`, the generated `web/src/api/` files) and **Lane W** (the rest of `web/`).

**Tech Stack:** Rust (axum, sqlx, Postgres 16, lettre, html5ever, `mrml 6.0.1`), React 19 +
TanStack Router and Query, Vite.

**Spec:** `.scratch/email-platform/specs/2026-11-27-m2d-tracking.md` — cited below as "spec §x".
Where this plan and the spec disagree, the spec wins.

## Lanes and branches

- Integration branch: `m2d-tracking`, cut from `main` once M2c has merged, else from
  `m2c-campaigns-and-send-pipeline`. Lane S works on it directly.
- Lane W works on `m2d-web` in a worktree and merges into the integration branch when W1 is done.
- **Never two implementers in one lane.** Order: S1 → S2 → S3 → S4. W1 runs alongside and merges the
  integration branch into `m2d-web` before its Step 3, which needs S1 and S3.

| ID | Title | Lane | Depends on | Size |
|---|---|---|---|---|
| S1 | Schema `0035` and the send-time hard bounce | S | — | ½ day |
| S2 | The tracked send rendition: pixel slot, link table, render-time rewrite | S | — | ¾ day |
| S3 | The campaign report endpoint | S | S1, S2 | ¼ day |
| S4 | The public tracking routes and the token on every message | S | S1, S2 | ½ day |
| W1 | The report section and the bounced count on the progress view | W | spec §5.2; S1 and S3 at Step 3 | ½ day |

## Global Constraints

- Every tenant table carries `workspace_id`. Another workspace's resource returns **404** — never
  403, and never the data. Every report query names `workspace_id` as well as `campaign_id`.
- Secrets never appear in logs, audit metadata, pause or failure reasons, or API responses. The
  tracking token is a credential: never logged, never in a reason. A log line names a message by id,
  never by address or URL.
- Windows cannot link this project. Every `cargo`, `sqlx` and `cargo deny` command, and
  `pnpm gen:api` (which runs `cargo`), runs **inside the Linux dev container**, compose project
  `-p nm`, via `bash scripts/dev.sh`.
- `.sqlx` is regenerated with `bash scripts/dev.sh cargo sqlx prepare -- --all-targets` by every
  task that adds or changes a `query!`, and committed in that task's commit. New queries follow the
  module's existing choice: `query!` in `suppressions` and `public`, runtime `query` in `campaign`.
- Commits are authored by the configured git user, in plain words, with no co-author or other
  trailers and no mention of any tool. Never push.
- Never stop, modify or remove the containers `oci-email-postgres` or `marketing-automation`.
- Gate commands run with `set -eo pipefail`. In a parallel lane, `cargo fmt --check`, never
  `cargo fmt --all`.
- Per-route gzipped JS budget: **51,200 bytes**. Shell budget: 150 KB.
- URL scheme allowlist, frozen: `{http, https, mailto, tel}`. Only `http` and `https` are tracked.
- No new dependency.
- TDD: the failing test is written and *run* before the implementation. A test that passes on
  broken code is not evidence.

Added for M2d:

- `mrml` never runs on the send path. The pixel is frozen into the footer; the click rewrite runs
  in the classifier's serialiser; nothing is appended to a document after classification.
- A redirect target is only ever an entry of the message's own campaign variant's frozen link
  table, re-checked as `http`/`https` at click. Nothing in the request but the token and the index
  is read.
- The unsubscribe link, and every link under `NM_PUBLIC_URL`, is never rewritten.
- No token, tracked URL or stored tracking row carries an address or a contact value.
- Unique counts come from first-time columns (`opened_at`, `clicked_at`) or row counts, never from
  incremented counters.
- Lock order: the address family (`A, L, S, contacts, M, C`) before the campaign family
  (`messages, campaigns, jobs`). **Each task adds its ADR 0005 path rows in the same commit.**
- The system footer's markup is spec §5.3 exactly.

## Review Focus

1. **Open-redirect abuse** → only a frozen `http`/`https` entry is ever a `Location`; a forged
   token, an unknown message, an index past the table, a query string and a tampered frozen link
   all get the same 404. Pinned to S4 (`the_redirect_target_comes_only_from_the_messages_own_campaign`,
   `a_frozen_link_that_is_not_http_is_never_followed`).
2. **Tracking the unsubscribe link** → the footer link, an operator's hard-coded public-URL link
   and `List-Unsubscribe` stay as they are. Pinned to S2 (`the_unsubscribe_link_is_never_rewritten`)
   and S4 (`a_sent_message_is_tracked_end_to_end`).
3. **An address leaking into tokens or stored text** → the token is the message id and a MAC;
   merge-tag links are not tabled; relay replies are scrubbed; the suppression note is fixed text.
   Pinned to S1 (`relay_text_is_stored_without_the_address`), S2
   (`no_tracked_url_carries_contact_data`) and S4
   (`an_erased_contacts_message_still_redirects_and_keeps_only_ids`).
4. **Double-counting** → two opens, `HEAD`, repeated clicks, a click with no open, a replayed bounce
   flush and a two-list unsubscribe each count once where the spec says once. Pinned to S1, S3, S4.
5. **The bounce path's lock order** → advisory locks in ascending key order, before `messages`.
   Pinned to S1 (`two_flushes_bouncing_the_same_addresses_in_opposite_orders_both_commit`).

---

# Lane S — server

### Task S1: schema `0035` and the send-time hard bounce

Spec §4.5, §4.7, §5.1. Lands the whole migration, so no later task touches `migrations/`.

**Files:**
- Create: `migrations/0035_tracking_and_bounces.sql`
- Modify: `src/relay/transport.rs`, `src/worker/confirmation.rs`, `src/worker/test_send.rs`,
  `src/worker/deliver.rs` (module doc only), `src/campaign/queue.rs`, `src/campaign/send.rs`,
  `src/suppressions/mod.rs`, `src/campaign/mod.rs`, `src/campaign/routes.rs`,
  `docs/adr/0005-one-lock-order-for-every-path.md`, `.sqlx/`
- Modify (generated): `web/src/api/openapi.json`, `web/src/api/schema.d.ts`
- Modify (the three web lines the new count breaks): `web/src/routes/campaign-mock.ts` (`NONE`),
  the `counts` fixtures in `web/src/components/campaign-editor/progress.test.tsx` and
  `web/src/routes/campaigns.test.tsx` — each gains `bounced: 0`
- Test: `tests/relay_transport.rs`, `tests/worker_confirmation.rs`, `tests/campaign_queue.rs`,
  `tests/campaign_send.rs`, `tests/lock_order.rs`; unit tests in `transport.rs` and `queue.rs`

**Interfaces:**
- Consumes: `suppressions::lock_address`, `crypto::suppression_hmac`, `users::normalize_email`,
  `queue::count`, `queue::finish_if_done`.
- Produces:
  ```rust
  // relay/transport.rs
  Outcome::Bounced(String)       // a permanent reply naming the recipient; never offered to another relay
  pub const HARD_BOUNCE_CODES: [&str; 6] = ["5.1.1", "5.1.2", "5.1.3", "5.1.6", "5.1.10", "5.2.1"];
  pub fn hard_bounce_code(reply: &str) -> Option<&'static str>
      // the first whitespace-separated word, trimmed of trailing ':' ',' ';', equal to one of them
  // campaign/queue.rs
  Verdict::Bounced { reason: String, email: String }   // final, recorded from `submitting`
  pub async fn record(db: &PgPool, secrets: &Secrets, outcomes: &[(Claimed, Verdict)]) -> sqlx::Result<()>
  pub fn scrub(reply: &str) -> String  // each whitespace-separated word holding '@' -> "[address]"
  // suppressions/mod.rs
  pub async fn record_bounces(db: &mut PgConnection, secrets: &Secrets,
                              bounces: &[(Uuid, &str, String)]) -> sqlx::Result<u64>
      // (workspace_id, email, note). Normalise (one that will not normalise is skipped and logged
      // without the address), hmac, sort by lock_address's key and dedup, lock each, insert
      // scope 'address', reason 'bounced', source 'send', note, on conflict do nothing. Rows written.
  // campaign/mod.rs
  pub struct CampaignCounts { /* … */ pub bounced: i64 }
  CampaignCounts::new(total, sent, failed, bounced, unknown, skipped)  // queued subtracts bounced
  ```

Rules the tests hold:
- `0035` is spec §5.1 exactly: `messages_status_check` dropped and re-added with `'bounced'`;
  `messages.opened_at`, `opens`, `clicked_at`, `clicks`; `campaigns.bounced`; `message_clicks` with
  no foreign key and `message_clicks_by_campaign`; `consent_log_by_source_ref`. Re-state
  `comment on column messages.status` with `bounced`. A comment on `message_clicks` says why it has
  no foreign key.
- `transport::refused`: in the `e.is_permanent()` arm, `hard_bounce_code(&reason)` decides
  `Bounced` against `Failed`. The module's classification table gains the row.
- `confirmation::handle` and `test_send::handle` record `Bounced` exactly as `Failed`.
  `deliver::submit` needs no change: `Bounced` is a decided outcome and returns at once.
- `send::verdict` gains the recipient's address, which `deliver` holds:
  `(Outcome::Bounced(reason), _)` → `Verdict::Bounced { reason, email }`. `reason_of` gains the
  arm. The flusher passes `&ctx.secrets` to `record`.
- `record`, one transaction: first `record_bounces` for every `Bounced` verdict, note
  `format!("hard bounce {code} at send, campaign {campaign_id}")`; then the `messages` update with
  `bounced` from `submitting`; then `count` (slot `bounced`) and `finish_if_done`. `response` and
  `error` of every verdict pass through `scrub` before binding.
- `finish_if_done`: `total = sent + failed + bounced + unknown + skipped`.

- [ ] **Step 1: Write the failing tests.**
  ```rust
  // relay/transport.rs (unit)
  fn hard_bounce_codes_name_the_recipient_and_nothing_else()
      // Some: "permanent error (550): 5.1.1 no such user", "…: 5.1.10 null MX",
      //       "…: 5.2.1 mailbox disabled", "…: 5.1.1: unknown"
      // None: "…(535): 5.7.8 authentication failed", "…(554): 5.7.1 rejected as spam",
      //       "…(550): 5.1.7 bad sender", "…(552): 5.3.4 too big", "…(550): no such user",
      //       "…(550): 5.1.11 x"
  // tests/relay_transport.rs — `a_550_is_failed` is replaced by the first two; `text()` gains the arm
  fn a_550_naming_the_mailbox_is_bounced()          // "550 5.1.1 no such user here", text kept
  fn a_550_naming_no_mailbox_is_failed()            // "554 5.7.1 rejected as spam"
  fn a_451_naming_the_mailbox_is_still_transient()  // "451 4.1.1 try later"
  // tests/worker_confirmation.rs — amend
  fn a_permanent_refusal_does_not_fail_over_to_the_next_relay()
      // now Outcome::Bounced(_); the job's failed_at is set; no suppressions row
  // campaign/queue.rs (unit)
  fn scrub_removes_every_address_and_keeps_the_rest()
      // "550 5.1.1 <alice+x@example.test>: no such user" -> "550 5.1.1 [address] no such user"
  // tests/campaign_queue.rs — every `record(&pool, …)` call gains the secrets
  fn a_bounce_is_recorded_counted_and_suppressed_once()
      // status bounced; campaigns.bounced 1, failed 0; one suppressions row: reason bounced,
      // source "send", note "hard bounce 5.1.1 at send, campaign {id}". Replaying the same flush
      // changes no count and adds no row
  fn a_campaign_whose_last_message_bounced_finishes()
  fn relay_text_is_stored_without_the_address()
      // Sent "250 ok <alice@example.test>", Failed "550 <alice@example.test> refused":
      // neither messages.response nor messages.error contains "alice@"
  fn an_address_already_blocked_keeps_its_row()     // an unsubscribed_all row stays the only row; flush commits
  // tests/campaign_send.rs
  fn a_550_naming_the_mailbox_bounces_and_suppresses_the_address()
      // contact dead@example.test, sink "550 5.1.1 <dead@example.test>: no such user":
      // message bounced, error holds no "dead@example.test", suppression reason bounced,
      // campaign sent; `queue::eligible` for a second campaign on the same list drops the contact
  fn a_4xx_and_an_unnamed_5xx_suppress_nothing()
      // "451 4.1.1 later" -> queued, attempts 1; "554 5.7.1 spam" -> failed; suppressions empty
  // tests/lock_order.rs — own_pool, settle, blocked; never a #[sqlx::test] pool for contenders
  fn two_flushes_bouncing_the_same_addresses_in_opposite_orders_both_commit()
      // a third connection holds X's advisory lock; flush 1 bounces [X, Y], flush 2 [Y, X];
      // settle until both wait; release; both commit, no 40P01. Red first: drop the sort
  fn a_bounce_flush_and_a_one_click_for_the_same_address_both_commit()   // both arrival orders
  ```
- [ ] **Step 2: Run.** `bash scripts/dev.sh cargo test --lib -- relay::transport campaign::queue && bash scripts/dev.sh cargo test --test relay_transport --test worker_confirmation --test campaign_queue --test campaign_send --test lock_order`
  Expected: FAIL (`Outcome::Bounced` and `record_bounces` not defined; `bounced` column missing).
- [ ] **Step 3: Write `0035`.**
- [ ] **Step 4: Implement** the transport arm, the two worker arms, `scrub`, `record_bounces`,
  `record`, `count`, `finish_if_done`, `verdict`, `CampaignCounts` and every select that builds
  `Campaign` or `CampaignSummary` (`grep -n skipped src/campaign`).
- [ ] **Step 5: Run** Step 2, then `bash scripts/dev.sh cargo test --test campaign_launch --test worker_pool --test campaigns`.
  Expected: PASS. `two_flushes_…` five runs in a row.
- [ ] **Step 6: Regenerate.** `bash scripts/dev.sh cargo sqlx prepare -- --all-targets`,
  `bash scripts/dev.sh pnpm -C web gen:api`, the three web lines, then `pnpm -C web typecheck`.
- [ ] **Step 7: ADR 0005.** Re-run the enumeration against a database migrated from nothing to
  `0035`: unchanged, `message_clicks` has no foreign key; say so beside "as of `0033`". Add the
  section **"M2d: the two families"**: the address family comes before the campaign family; every
  path that takes both already does so (the public form and erasure reach `jobs` last), and the
  bounce flush is the first to take `A` and `S` and then `messages`. Change the `record` row:
  > `campaign::queue::record` (M2d) | `A` once per bounced address in ascending lock-key order, and
  > the `suppressions` insert for each (`suppressions::record_bounces`), then `messages`, then
  > `campaigns` as before. One transaction. A flush with no bounce takes no `A` and no `S`. Raced by
  > `two_flushes_bouncing_the_same_addresses_in_opposite_orders_both_commit` and
  > `a_bounce_flush_and_a_one_click_for_the_same_address_both_commit`
- [ ] **Step 8: Commit.** `git commit -m "Record a hard bounce at send and suppress the address in the same transaction"`

---

### Task S2: the tracked send rendition — pixel slot, link table, render-time rewrite

Spec §4.2.1–2, §4.3.1–4, §5.3. **Security core**: it changes the classified document and what the
serialiser writes into URL attributes.

**Files:**
- Modify: `src/template/mod.rs`, `src/template/footer.rs`, `src/template/render.rs`,
  `src/template/tag.rs`, `src/campaign/mod.rs` (`prepare`)
- Modify (one new field each): every `Contact {` literal (14 sites:
  `grep -rn "unsubscribe_token:" src tests`), every `SystemFooter {` literal
  (`grep -rn "SystemFooter {" src tests`), every `substitute(` call. `campaign/send.rs` and
  `campaign/routes.rs` (`render_for`) pass `tracking_token: None` until S4.
- Test: unit tests in `template/mod.rs`, `render.rs`, `footer.rs`; `tests/campaign_send.rs`

**Interfaces:**
- Consumes: `check_static_url`, `escape_attr`, `scan` (classify and substitute modes),
  `footer::insertion_point`, `Frozen::from_column`.
- Produces:
  ```rust
  // template/mod.rs
  pub const TRACKING_TOKEN: &str = "tracking_token";
  impl Stored { pub fn links(&self) -> &[String] }
      // `links` on Stored and Raw: #[serde(default, skip_serializing_if = "Vec::is_empty")]
  // template/footer.rs
  pub struct SystemFooter<'a> { pub public_url: &'a url::Url, pub postal_address: &'a str,
                                pub prior_links: &'a [String] }  // the variant's table before a re-freeze
  // template/render.rs
  pub struct Contact<'a> { /* … */ pub tracking_token: Option<&'a str> }
      // None: a test send or a preview. Links untouched, the pixel slot renders "test"
  pub(super) fn tracked_url(href: &str, base: &str) -> Option<String>
      // spec §4.3.2: check_static_url is Allowed, scheme http|https, and the canonical form does
      // not start with "{base}/". Returns the canonical form
  pub(super) fn tracked_links(html: &str, base: &str, prior: &[String]) -> Vec<String>
      // `prior`, then each new tracked_url of a static a/area href, in document order
  pub fn substitute(html: &str, nodes: &[Node], contact: &Contact<'_>,
                    links: Option<(&str, &[String])>) -> Result<String, RenderError>
      // (base, table). A static a/area href whose tracked_url is at position i of the table, with
      // contact.tracking_token Some(t), is written "{base}/l/{t}/{i}" through escape_attr
  // template/tag.rs
  Registry::system_footer()   // knows UNSUBSCRIBE_TOKEN and TRACKING_TOKEN, nothing else
  ```

Rules the tests hold:
- The footer is spec §5.3 exactly; the pixel's slot is a second `Field` from
  `Registry::system_footer()`, after the unsubscribe slot.
- `from_definitions` skips a custom field keyed `tracking_token`; `field_kinds` skips it;
  `Contact::value` answers it before `attributes`, as `tracking_token.unwrap_or("test")`.
- `build` (send renditions only): `links = tracked_links(&compiled.html, footer.base(),
  footer.prior_links)`. A template's own rendition has none, so `templates.compiled` serialises
  exactly as before.
- `Ready::render` passes `links = self.0.unsubscribe.as_deref().and_then(|p| p.strip_suffix("/u/"))
  .map(|b| (b, self.0.links.as_slice()))`. A rendition with an empty table renders untracked.
- The link counter runs in the tag serialiser, so a link inside a dropped conditional branch is
  still seen and every later index holds.
- `prepare` reads the campaign's current `frozen` (when it parses) and passes variant `i`'s
  `links()` as `prior_links`; `&[]` when there is none.
- The `NoUnsubscribeToken` and `FooterLost` guards are unchanged.

- [ ] **Step 1: Write the failing tests.** Fixtures: `public_url` `https://mail.example.test`,
  unsubscribe token `TOK`, tracking token `TRK`.
  ```rust
  fn a_send_rendition_carries_the_pixel_right_after_the_unsubscribe_link()
      // html contains `Unsubscribe</a><img src="https://mail.example.test/o/TRK" width="1" height="1" alt=""`
  fn a_message_without_a_tracking_token_has_a_test_pixel_and_untouched_links()  // src ".../o/test"
  fn an_operator_cannot_name_the_tracking_slot()           // UnknownField("tracking_token")
  fn a_custom_field_keyed_tracking_token_neither_fills_nor_drifts_the_slot()
  fn http_links_are_tracked_in_first_appearance_order_and_nothing_else_is()
      // raw html hrefs: https://a.test/x, mailto:a@b.test, tel:+100, #top, /relative,
      // https://mail.example.test/u/abc, https://mail.example.test.evil.test/, {{ website }},
      // https://a.test/x again, http://b.test ->
      // links() == ["https://a.test/x", "https://mail.example.test.evil.test/", "http://b.test/"];
      // both a.test hrefs -> https://mail.example.test/l/TRK/0, evil -> /l/TRK/1, b -> /l/TRK/2;
      // every other href byte-identical to the render with tracking_token None
  fn the_unsubscribe_link_is_never_rewritten()
      // a document whose only link is the footer's: href="https://mail.example.test/u/TOK" kept,
      // no "/l/" anywhere, render Ok
  fn an_mj_button_link_is_tracked()                        // mj-button href https://a.test/buy -> /l/TRK/0
  fn a_link_in_a_dropped_branch_keeps_every_later_index()
      // {{# vip }}<a href="https://a.test/vip">…{{/ vip }}<a href="https://b.test/"> for a non-vip
      // contact -> b.test is /l/TRK/1
  fn a_link_built_from_a_merge_tag_is_neither_tracked_nor_stored()  // https://x.test/?e={{ email }}
  fn no_tracked_url_carries_contact_data()
      // email alice+x@example.test, first_name Alice: no /o/ or /l/ URL holds "alice", "%40" or "@"
  fn a_refreeze_keeps_every_earlier_link_at_its_index()
      // prior ["https://old.test/", "https://b.test/"], document links b and c ->
      // links == [old, b, c]; b -> /l/TRK/1, c -> /l/TRK/2
  fn a_rendition_frozen_before_tracking_renders_untracked()  // its JSON without "links": no /l/
  fn a_template_rendition_has_no_links_key()               // serde_json of `compile`'s Stored
  // tests/campaign_send.rs
  fn a_resume_that_refreezes_keeps_every_earlier_link_index()
      // extends the M2c drift-resume fixture: frozen variant 0's links after resume start with
      // the links before it
  ```
- [ ] **Step 2: Run.** `bash scripts/dev.sh cargo test --lib template:: && bash scripts/dev.sh cargo test --test campaign_send a_resume_that_refreezes`
  Expected: FAIL.
- [ ] **Step 3: Implement** the footer, the slot, `tracked_url`, `tracked_links`, the serialiser
  branch, `links` on `Stored`, `prepare`'s prior table, and the literal sites.
- [ ] **Step 4: Run** Step 2, then `bash scripts/dev.sh cargo test --lib && bash scripts/dev.sh cargo test --test templates --test campaigns --test campaign_send --test campaign_launch`.
  Expected: PASS, the M2b golden fingerprint unchanged.
- [ ] **Step 5: ADR 0005.** No row changes: no statement changed. Say so in the commit body.
- [ ] **Step 6: Commit.** `git commit -m "Freeze a tracking pixel and a link table into the send rendition and rewrite tracked links at render"`

---

### Task S3: the campaign report endpoint

Spec §4.4, §4.6, §5.2.

**Files:**
- Create: `src/campaign/report.rs`, `tests/campaign_report.rs`
- Modify: `src/campaign/mod.rs` (`pub mod report;`), `src/campaign/routes.rs` (route and handler),
  `src/openapi.rs` (the path, `CampaignReport`, `LinkReport`),
  `docs/adr/0005-one-lock-order-for-every-path.md`
- Modify (generated): `web/src/api/openapi.json`, `web/src/api/schema.d.ts`

**Interfaces:**
- Consumes: S1's columns and `message_clicks`; S2's `Stored::links`; `campaign::load`,
  `authorize`, `Frozen::from_column`.
- Produces:
  ```rust
  // campaign/report.rs
  #[derive(Debug, Serialize, ToSchema)]
  pub struct CampaignReport { pub sent: i64, pub failed: i64, pub bounced: i64,
      pub unique_opens: i64, pub unique_clicks: i64, pub unsubscribed: i64,
      pub links: Vec<LinkReport> }
  #[derive(Debug, Serialize, ToSchema)]
  pub struct LinkReport { pub variant: i16, pub link: i32, pub url: String,
      pub clicks: i64, pub unique_clicks: i64 }
  pub async fn report(db: &PgPool, c: &Campaign) -> sqlx::Result<CampaignReport>
  // campaign/routes.rs
  GET /api/v1/workspaces/{workspace_id}/campaigns/{id}/report   operation_id "report_campaign"
  pub async fn report_campaign(…) -> ApiResult<Json<CampaignReport>>   // Viewer; load() is the 404
  ```

`report`: `sent`, `failed`, `bounced` from the row; three plain reads, runtime `query_as`, each
with `workspace_id = $1`:
1. `count(*) filter (where opened_at is not null)`, `count(*) filter (where clicked_at is not
   null)` from `messages where workspace_id = $1 and campaign_id = $2`.
2. `variant, link, sum(clicks)::bigint, count(*)` from `message_clicks` with the same predicate,
   `group by variant, link`.
3. `count(distinct contact_id)` from `consent_log where workspace_id = $1 and source_ref = $2 and
   action = 'unsubscribed'`, `$2` the campaign id as text.

`links`: every variant's `links()` from the frozen column, joined to read 2 by `(variant, index)`,
zeros where absent, ordered by `(variant, link)`. No frozen column, or one that does not parse
(logged by campaign id): empty.

- [ ] **Step 1: Write the failing tests** in `tests/campaign_report.rs`:
  ```rust
  fn another_workspaces_campaign_report_is_404()          // body has no "unique_opens"
  fn a_viewer_reads_the_report()
  fn a_draft_reports_zeros_and_no_links()
  fn the_report_counts_each_message_once()
      // a frozen campaign with links [a, b] and three messages: m1 opened (opens 3) and clicked,
      // message_clicks (m1,0,clicks 2), (m1,1,clicks 1); m2 opened (opens 1); m3 nothing ->
      // unique_opens 2, unique_clicks 1, links [{0,0,a,2,1}, {0,1,b,1,1}]
  fn every_frozen_link_is_listed_with_zeros_when_unclicked()
  fn bounced_is_its_own_count()                            // row: sent 5, failed 1, bounced 2
  fn an_unsubscribe_from_two_lists_counts_once()
      // two consent_log rows for one contact with source_ref = campaign id, one for another
      // campaign -> unsubscribed 1
  fn rows_carrying_another_workspace_are_not_counted()
      // a message_clicks row with this campaign id and another workspace_id is not in the report
  ```
- [ ] **Step 2: Run.** `bash scripts/dev.sh cargo test --test campaign_report` — Expected: FAIL.
- [ ] **Step 3: Implement** `report.rs`, the handler, the route and the OpenAPI entries.
- [ ] **Step 4: Run** Step 2, then `bash scripts/dev.sh cargo test --test campaigns`. Expected: PASS.
- [ ] **Step 5: Regenerate.** `bash scripts/dev.sh pnpm -C web gen:api`; commit both files. This is
  the hand-off to W1.
- [ ] **Step 6: ADR row.** `campaign::routes::report_campaign` | **nothing.** Three plain reads and
  the `load` before them; no transaction.
- [ ] **Step 7: Commit.** `git commit -m "Add the campaign report: sends, bounces, unique opens and clicks, unsubscribes and clicks per link"`

---

### Task S4: the public tracking routes and the token on every message

Spec §4.1, §4.2.3, §4.3.5.

**Files:**
- Create: `src/public/track.rs`, `tests/tracking.rs`
- Modify: `src/public/mod.rs` (`pub mod track;`, `PREFIXES` gains `/o` and `/l`, merge
  `track::routes()`, its prefix test), `src/http/web_ui.rs`
  (`the_subscribers_pages_are_never_the_spa`), `src/http/rate_limit.rs`, `src/app.rs`,
  `src/campaign/send.rs` (`deliver`), `docs/adr/0005-one-lock-order-for-every-path.md`, `.sqlx/`
- Test: `tests/tracking.rs`, `tests/campaign_send.rs`; unit tests in `track.rs`

**Interfaces:**
- Consumes: `crypto::tokens::{hmac_sha256, constant_time_eq}`, `Secrets::derive_key`,
  `RequestMeta`, `page::{notice, with, UNKNOWN_IP}`, `template::render::check_static_url`, S2's
  `Contact::tracking_token`, S3's report (the end-to-end test).
- Produces:
  ```rust
  // public/track.rs
  pub const TRACK_DOMAIN: &str = "nm:track-token:v1";
  pub fn issue(secrets: &Secrets, message_id: Uuid) -> String   // 43 chars: base64url(id ‖ mac[..16])
  pub fn parse(secrets: &Secrets, raw: &str) -> Option<Uuid>     // constant time; no database
  pub const PIXEL: [u8; 43];                                     // transparent GIF89a
  pub fn routes() -> Router<AppState>                            // GET /o/{token}, GET /l/{token}/{link}
  // http/rate_limit.rs
  pub const TRACK_PER_MINUTE: u32 = 600;
  // app.rs
  pub track_limit: Arc<RateLimiter>                              // RateLimiter::per_minute(TRACK_PER_MINUTE)
  ```

Handler shapes:
- `open`: spend `track_limit` for the client IP. When the method is `GET`, the request is within
  budget and `parse` names a message: `update messages set opens = opens + 1, opened_at =
  coalesce(opened_at, now()) where id = $1`, an error logged by message id. Always answer `200`,
  `Content-Type: image/gif`, `PIXEL`, and no `RateLimit` headers, so every answer is the same bytes.
- `click`: over budget → `429` with `Retry-After`, the `GET /u/` refusal shape. `parse` fails,
  `link` is not a `u32`, or `select c.frozen #>> array['variants', m.variant::text, 'rendition',
  'links', $2] from messages m join campaigns c on c.id = m.campaign_id and c.workspace_id =
  m.workspace_id where m.id = $1` is absent or null, or `check_static_url` is not `Allowed` with
  scheme `http`/`https` → one `404` page, "This link is not valid", no `Location`. Otherwise, on
  `GET` only:
  ```sql
  with m as (update messages set clicks = clicks + 1, clicked_at = coalesce(clicked_at, now()),
                                 opened_at = coalesce(opened_at, now())
              where id = $1 returning workspace_id, campaign_id, variant)
  insert into message_clicks (message_id, link, workspace_id, campaign_id, variant)
  select $1, $2, workspace_id, campaign_id, variant from m
  on conflict (message_id, link) do update set clicks = message_clicks.clicks + 1, last_at = now()
  ```
  An error is logged by message id and the redirect still answers. Then `302`, `Location` the
  re-serialised URL, with the quota headers.
- `deliver`: `let tracking = track::issue(&ctx.secrets, m.id);` and
  `tracking_token: Some(&tracking)`.

- [ ] **Step 1: Write the failing tests.**
  ```rust
  // public/track.rs (unit)
  fn a_token_is_43_characters_and_round_trips_to_its_message()
  fn a_token_with_any_byte_changed_is_refused()
  fn an_unsubscribe_token_is_not_a_tracking_token()
  // public/mod.rs and http/web_ui.rs: "/o", "/o/x", "/l", "/l/x/0" owned and never the SPA;
  // "/login" and "/options" are not
  // tests/tracking.rs
  fn the_pixel_records_the_first_open_once_and_counts_every_open()   // two GETs: opens 2, opened_at kept
  fn the_pixel_answers_the_same_for_a_forged_an_unknown_and_a_real_token()
      // /o/garbage, /o/{issue(random uuid)}, /o/{real}: status, content-type, cache-control and body
      // equal; only the real one moves its row
  fn a_head_records_nothing()                     // HEAD /o and HEAD /l/{t}/0: opens 0, clicks 0; /l still 302
  fn a_click_redirects_to_the_frozen_link_and_records_it_once_per_message_and_link()
      // GET /l/{t}/0 twice, /l/{t}/1 once: exact Locations; messages.clicks 3, opens 0,
      // clicked_at and opened_at set; message_clicks (0: 2), (1: 1)
  fn a_forged_token_an_unknown_message_and_an_index_past_the_table_are_the_same_404()
      // also /l/{t}/abc and /l/{t}/999: 404, no Location, one body
  fn the_redirect_target_comes_only_from_the_messages_own_campaign()
      // /l/{t}/0?url=https://evil.test -> the frozen link; campaign A has 1 link and B 3:
      // A's token with index 2 -> 404
  fn a_frozen_link_that_is_not_http_is_never_followed()
      // frozen links edited in the database to "javascript:alert(1)" and "//evil.test" -> 404
  fn the_tracking_routes_spend_their_own_per_ip_budget()
      // 600 pixel GETs with a forged token from 198.51.100.20 (no database work), then:
      // a click -> 429 with Retry-After; a real pixel -> 200 GIF and opens unchanged;
      // GET /u/{token} from the same address -> 200
  fn an_erased_contacts_message_still_redirects_and_keeps_only_ids()
      // erase the contact through the API; the click is 302; row_to_json of its messages and
      // message_clicks rows contains no "@"
  // tests/campaign_send.rs
  fn a_sent_message_is_tracked_end_to_end()
      // blocks template with one mj-button link, through the sink: the message holds one /o/ URL
      // and /l/{t}/0; List-Unsubscribe equals the body's /u/ link and no /l/ URL leads under
      // /u/; GET both through the app; the report says unique_opens 1, unique_clicks 1,
      // links[0].clicks 1
  fn no_url_in_a_sent_message_carries_the_recipients_address()  // recipient alice+x@example.test
  ```
- [ ] **Step 2: Run.** `bash scripts/dev.sh cargo test --lib -- public:: http:: && bash scripts/dev.sh cargo test --test tracking --test campaign_send`
  Expected: FAIL.
- [ ] **Step 3: Implement** `track.rs`, the prefixes, the limiter, `AppState`, `deliver`.
- [ ] **Step 4: Run** Step 2, then `bash scripts/dev.sh cargo test --test public_pages --test public_tokens --test http_foundation`.
  Expected: PASS.
- [ ] **Step 5: Regenerate** `.sqlx`; then run the benchmark once and keep its number for the PR:
  `bash scripts/dev.sh cargo test --release --test campaign_bench -- --ignored --nocapture`.
- [ ] **Step 6: ADR rows.**
  > `public::track::open` | the `messages` row alone, one statement on the pool. Writes `opens`
  > and `opened_at`, no key column. A leaf
  >
  > `public::track::click` | one statement on the pool: the `messages` row (`clicks`,
  > `clicked_at`, `opened_at`, no key column), then the `message_clicks` row, which has no foreign
  > key and which nothing references. The lookup before it is a plain read. A leaf
- [ ] **Step 7: Commit.** `git commit -m "Serve the open pixel and the click redirect, and give every campaign message its tracking token"`

---

# Lane W — web

The wire shapes are spec §5.2. Where code and spec disagree, the spec wins; a change goes back to
the spec first.

### Task W1: the report section and the bounced count on the progress view

**Files:**
- Create: `web/src/components/campaign-editor/report.tsx`,
  `web/src/components/campaign-editor/report.test.tsx`
- Modify: `web/src/components/campaign-editor/progress.tsx`,
  `web/src/components/campaign-editor/progress.test.tsx`, `web/src/routes/campaign-model.ts`,
  `web/src/routes/campaign-source.ts`, `web/src/routes/campaign-mock.ts`,
  `web/src/routes/campaign-stub.ts`

**Interfaces:**
- Consumes: `schema.d.ts` from S1 and S3; `Section`, `Problem`, `Button`, `campaignsKey`.
- Produces:
  ```ts
  // campaign-model.ts
  export type Report = Present<S["CampaignReport"]>;
  export type LinkReport = Present<S["LinkReport"]>;
  // Counts' comment: queued = total - sent - failed - bounced - unknown - skipped
  // campaign-source.ts — CampaignSource gains
  report: (ws: string, id: string) => Promise<Res<Report>>;   // GET `${C}/report`
  // campaign-mock.ts — MockState gains `reports: Record<string, Report>`; a campaign with none
  // answers zeros and no links; another workspace's id is the 404 problem
  // report.tsx
  export function CampaignReport(props: { ws: string; id: string; variantCount: number;
                                          source: CampaignSource; slug: string }): JSX.Element
  ```

Behaviour:
- `progress.tsx`: `COUNTS` gains `["bounced", "Bounced (no such mailbox; now suppressed)",
  "bg-danger opacity-60"]` after `failed`. `<CampaignReport>` renders below Progress when
  `c.launched_at !== null`. The winner picker's line becomes "Each variant's link clicks are in the
  report below."
- `report.tsx`: a `Section` titled "Report"; a `useQuery` keyed `[...campaignsKey(ws), id,
  "report"]`, no `refetchInterval`; a `dl` of "Sent", "Failed", "Bounced", "Opened (approximate)",
  "Clicked", "Unsubscribed"; under it "Opens are approximate: some mail apps load every image,
  others load none."; a table captioned "Clicks by link" with columns "Variant" (only when
  `variantCount > 1`, shown 1-based), "Link" (the URL as text, `wrap-anywhere`), "Clicks", "Unique
  clicks"; "No tracked links in this campaign." when `links` is empty; a "Refresh" button that
  refetches. A refusal shows through `Problem`, and no numbers.

- [ ] **Step 1: Write the failing tests** (against spec §5.2 shapes; they compile once Step 3's
  merge brings the generated types):
  - `report.test.tsx`: "report shows sent, failed, bounced, opened, clicked and unsubscribed from
    the source"; "report lists clicks per link, with a variant column only for two or more
    variants"; "report says there are no tracked links when the table is empty"; "report asks the
    source again on Refresh and never on a timer"; "report shows the problem, not numbers, when the
    source answers 404".
  - `progress.test.tsx`: "progress lists bounced as its own count and queued leaves it out";
    "progress shows no report before launch"; "progress's winner picker points at the report".
- [ ] **Step 2: Run.** `pnpm -C web exec playwright test --config playwright.unit.config.ts src/components/campaign-editor`
  Expected: FAIL.
- [ ] **Step 3: Merge the integration branch into `m2d-web`** once S1 and S3 are on it, so
  `schema.d.ts` carries `bounced`, `CampaignReport` and `LinkReport`.
- [ ] **Step 4: Implement** the model types, the source method, the mock and the stub route, the
  component, and the progress changes.
- [ ] **Step 5: Run** Step 2, then `pnpm -C web typecheck && pnpm -C web build && pnpm -C web check:bundle`.
  Expected: PASS, the campaign route under 51,200 bytes.
- [ ] **Step 6: Commit and merge `m2d-web` into the integration branch.** `git commit -m "Show the campaign report and the bounced count on the progress view"`

---

## Finishing

1. Merge `m2d-web`; remove the worktree.
2. Full gate, serialised, inside the container, `set -eo pipefail`: `cargo fmt --check`,
   `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo sqlx prepare --check --
   --all-targets`, OpenAPI drift, `cargo deny check`, `pnpm install --frozen-lockfile`,
   `pnpm build`, `cargo build --release`, `pnpm test`, `pnpm check:bundle`, e2e.
3. Whole-branch review against spec §6 and §7.
4. Fix wave; PR with the benchmark number; merge to `main`.
