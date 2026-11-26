# M2c — campaigns and the send pipeline: implementation plan

**Goal:** An admin sends a scheduled, optionally A/B-split campaign to lists narrowed by a segment;
each mailable contact gets exactly one signed message with a working one-click unsubscribe and the
postal address, and the send can be tested, paused, resumed, cancelled and watched.

**Architecture:** `messages` is the per-recipient queue, claimed with `0026`'s protocol shape and
written ahead (`submitting`) before SMTP; `jobs` carries only control work (`campaign_launch`,
`campaign_test`). Content is compiled once per variant at launch ("freeze") with a product-owned
footer inside the classified document, so the send path only substitutes. Two lanes: **Lane R**
(`src/`, `migrations/`, `tests/`, `docs/`, `.scratch/`) and **Lane W** (`web/`), joined by the
spec's rulings (`.scratch/email-platform/specs/2026-11-26-m2c-campaigns-and-send-pipeline.md`
§4 and §5) and, at W3, by the generated `schema.d.ts`.

**Tech Stack:** Rust (axum, sqlx, Postgres 16, lettre, `chrono-tz`, html5ever, `mrml 6.0.1`),
React 19 + TanStack Router, Vite.

**Spec:** `.scratch/email-platform/specs/2026-11-26-m2c-campaigns-and-send-pipeline.md` — cited
below as "spec §x". Where this plan and the spec disagree, the spec wins.

## Lanes and branches

- Integration branch: `m2c-campaigns-and-send-pipeline`. Lane R works on it directly.
- Lane W works on `m2c-web` in a worktree, merged into the integration branch after each W task.
- **Never two implementers in one lane.** Order: R1 → R2 → R3 → R4 → R5 → R6 → R7; W1 → W2 →
  (R3 merged) W3.

| ID | Title | Lane | Depends on |
|---|---|---|---|
| R1 | Schema `0033` and the foreign-key graph | R | — |
| R2 | The send rendition: footer, unsubscribe slot, freeze, subject tags (issues 48, 49, 51) | R | — |
| R3 | Campaign API, schedule and DST, test-send enqueue, template summary (issue 50) | R | R1, R2 |
| R4 | The message queue: claim, write-ahead, record, sweep, eligibility | R | R1 |
| R5 | Launch: freeze and audience resolution | R | R2, R3, R4 |
| R6 | The send loop, relay deferral, pause, test-send delivery | R | R2, R4, R5 |
| R7 | ADR 0005 re-run, lock-order races, benchmark, issue records | R | R3–R6 |
| W1 | Campaign model, mock source, list screen | W | contract |
| W2 | Campaign editor | W | W1 |
| W3 | Progress view, real API, e2e | W | W2, R3 |

## Global Constraints

Copied verbatim from M2b:

- Every tenant table carries `workspace_id`. Another workspace's resource returns **404** — never
  403, and never the data.
- Secrets never appear in logs, audit metadata or API responses.
- Windows cannot link this project. Every `cargo`, `sqlx` and `cargo deny` command runs **inside the
  Linux dev container**, compose project `-p nm`, via `scripts/dev.sh`.
- Never stop, modify or remove the containers `oci-email-postgres` or `marketing-automation`.
- Gate commands run with `set -eo pipefail`. Without it, `cmd | tail` reports `tail`'s status and a
  failing build exits 0.
- In a parallel lane, use `cargo fmt --check`, never `cargo fmt --all` — the latter rewrites the
  other lane's half-written files.
- Per-route gzipped JS budget: **51,200 bytes**. Shell budget: 150 KB.
- The merge-tag filter list is frozen at `upper`, `lower`, `title`, `date:<fmt>`. Adding one is a
  spec change, not an implementation choice.
- URL scheme allowlist, frozen: `{http, https, mailto, tel}`.
- No new dependency without a `deny.toml` licence check. `mrml`'s six loader features stay denied.
- TDD: the failing test is written and *run* before the implementation. A test that passes on
  broken code is not evidence.

Added for M2c:

- `mrml` never runs on the send path. It runs at save and at freeze (launch, test send, campaign
  preview), always in `spawn_blocking` and outside any transaction.
- A message is marked `submitting` in a committed statement before SMTP. A `submitting` row is
  never claimed. `unknown` is final and never offered to another relay.
- The unsubscribe URL is `{NM_PUBLIC_URL}/u/{token}` and nothing else: absolute, https, never
  built from merge-tag output, token from `public::token::issue` with `ref_id = campaign_id`.
- Every campaign message carries the brand kit's postal address and a body unsubscribe link,
  both inside the classified document. Nothing is appended after classification.
- Multi-statement transactions on these tables lock `messages`, then `campaigns`, then `jobs`.
  **Each task adds its ADR 0005 path rows in the same commit** (the ADR's standing obligation).
- New queries follow the module's existing choice of `query!` or runtime `query`; when `query!`,
  refresh the offline cache with `cargo sqlx prepare -- --all-targets`.
- Commit messages in plain words, no co-author or tool trailers. Never push.

## Review Focus

1. **A contact unsubscribes mid-send** via the one-click link of an earlier message in the same
   campaign → the remaining message is `skipped`, never submitted; `consent_log.source_ref` is the
   campaign id. Pinned to R4 (`eligible_drops_…`) and R6 (`a_one_click_mid_send_…`).
2. **Template or brand kit edited mid-send, and between schedule and launch** → the first never
   reaches the campaign; the second does not refuse the launch. Pinned to R2 and R5.
3. **A relay dies or throttles mid-batch** → broken after `DATA` is `unknown`, never retried, never
   offered to relay 2; a refused connection fails over with a token; `455` releases with the
   attempt intact and drains the bucket. Pinned to R6.
4. **Two workers, and a worker that dies between the relay's 250 and the record** → one
   submission per message; a stale lease holder cannot mark `submitting`; the sweep makes the
   orphan `unknown`. Pinned to R4 and R6.
5. **A schedule across DST** (`America/New_York` 2027-03-14 02:30 skipped, 2027-11-07 01:30
   ambiguous), in both modes, and recipient-local contacts on both sides. Pinned to R3 and R5.

---

# Lane R — server

### Task R1: schema `0033` and the foreign-key graph

**Files:**
- Create: `migrations/0033_campaigns_and_messages.sql`
- Modify: `tests/lock_order.rs` (`the_foreign_key_graph_is_the_one_the_adr_rules_on`),
  `docs/adr/0005-one-lock-order-for-every-path.md` (the catalog output block and the parent table)
- Test: `tests/campaigns.rs` (new)

**Interfaces:**
- Produces: tables `campaigns`, `messages`, column `jobs.campaign_id`, exactly as spec §5, with
  these constraint names, which later tasks and tests match on: `campaigns_workspace_id_id_key`,
  `messages_campaign_contact_key`, `messages_campaign_fk` (no action), `jobs_campaign_fk`
  (cascade), `jobs_campaign_shape`; indexes `campaigns_live`, `messages_runnable`,
  `messages_in_flight`.

- [ ] **Step 1: Write the failing tests** in `tests/campaigns.rs`:
  ```rust
  fn a_second_message_for_one_contact_in_one_campaign_is_refused() // 23505, messages_campaign_contact_key
  fn a_message_cannot_name_another_workspaces_campaign()            // 23503, messages_campaign_fk
  fn a_campaign_with_messages_cannot_be_deleted()                   // 23503 on delete from campaigns
  fn a_campaign_job_must_name_its_campaign()                        // 23514, jobs_campaign_shape
  fn deleting_a_draft_takes_its_fresh_test_jobs_with_it()           // a campaign_test job created now
                                                                    // is gone; jobs_throttle_retention
                                                                    // admits the cascade
  ```
  And extend the expected edge list in `the_foreign_key_graph_is_the_one_the_adr_rules_on` with
  `"campaigns -> jobs (c)"`, `"campaigns -> messages (a)"`, `"workspaces -> campaigns (c)"`.
- [ ] **Step 2: Run.** `bash scripts/dev.sh cargo test --test campaigns --test lock_order the_foreign_key_graph`
  Expected: FAIL, relation `campaigns` does not exist; graph test missing three edges.
- [ ] **Step 3: Write `0033`.** Comment each column whose meaning is not its name
  (`run_after` is the lease, `frozen` is read only through `Frozen::from_column`), and add
  `comment on column jobs.campaign_id`. All indexes now: migrations run in a transaction, so no
  `concurrently` later.
- [ ] **Step 4: Run again.** Expected: PASS.
- [ ] **Step 5: Update the ADR's catalog block** with the query's real output against a database
  migrated from nothing (the ADR's own commands), and add `campaigns` to the parent table as
  "ruled in R7". Eleven parents.
- [ ] **Step 6: Commit.** `git commit -m "Add campaigns and messages tables and the campaign link on jobs"`

---

### Task R2: the send rendition — footer, unsubscribe slot, freeze, subject tags

Closes issues 48, 49 and 51 (spec §4.5). **Security core**: it adds a slot to the classified
document.

**Files:**
- Create: `src/template/footer.rs`
- Modify: `src/template/mod.rs`, `src/template/mjml.rs`, `src/template/render.rs`,
  `src/template/text.rs`, `src/template/tag.rs`, `src/template/routes.rs` (stored preview),
  `src/mail/mod.rs`, `src/worker/confirmation.rs` (new `Outgoing` fields), every
  `render::Contact { … }` literal (11 sites; `grep -rn "Contact {" src tests`)
- Modify: issues `48`, `49`, `51` — `Status: closed by M2c R2 (<sha>)`
- Test: unit tests in the files above; `tests/templates.rs`

**Interfaces:**
- Consumes: `compile`, `fingerprint`, `stored_field_names`, `Stored::from_column`,
  `Stored::ready`, `mjml::BrandKit`, `tag::Registry`.
- Produces:
  ```rust
  // template/mod.rs
  pub const UNSUBSCRIBE_TOKEN: &str = "unsubscribe_token";
  pub struct SystemFooter<'a> { pub public_url: &'a url::Url, pub postal_address: &'a str }
  pub fn compile_for_send(authored: Authored<'_>, brand: &BrandKit, defs: &[Definition],
      keep_archived: &BTreeSet<String>, footer: &SystemFooter<'_>) -> Result<Stored, CompileError>
  pub enum FreezeError { Drift(Vec<Drift>), Compile(CompileError) }   // thiserror, Display = an operator sentence
  pub fn freeze(stored: Option<&serde_json::Value>, authored: Authored<'_>, brand: &BrandKit,
      defs: &[Definition], footer: &SystemFooter<'_>) -> Result<Stored, FreezeError>
  impl Stored { pub fn fingerprint(&self) -> &str }
  // template/render.rs — one new field on the existing struct
  pub struct Contact<'a> { /* … */ pub unsubscribe_token: &'a str }
  // template/mjml.rs — one new arm
  CompileError::FooterHidden
  // template/tag.rs
  impl Registry { pub fn system_footer() -> Registry }   // knows UNSUBSCRIBE_TOKEN and nothing else
  // template/text.rs
  pub fn compile_subject(source: &str, registry: &Registry) -> Result<Vec<Node>, TagError>
  pub fn render_subject(nodes: &[Node], contact: &Contact<'_>) -> String
  // mail/mod.rs — two new fields on Outgoing
  pub reply_to: Option<&'a str>, pub message_id: Uuid   // Message-ID is <{message_id.simple()}@{message_id_domain}>
  ```

Rules the tests hold (spec §4.5):
- The footer is compiled with `Registry::system_footer()`; its slot indices continue after the
  operator document's, and its marked HTML is inserted **after `mrml`, before `classify`**, at the
  byte offset of the real `</body>` end-tag token from html5ever's tokenizer (end of document when
  there is none). Classification then runs over the whole document.
- Footer HTML, exact: `<table role="presentation" width="100%" cellpadding="0" cellspacing="0"
  border="0"><tr><td align="center" style="padding:16px 24px;font-family:Arial,Helvetica,sans-serif;font-size:12px;line-height:18px;color:#6b7280;">{ADDRESS}<a
  href="{BASE}/u/{SLOT}" style="color:#6b7280;text-decoration:underline;">Unsubscribe</a></td></tr></table>`.
  `{ADDRESS}` is the address escaped for text, then `\n` → `<br>`, then `<br>` (contract §7's
  order), and is empty for a `blocks` template. `{BASE}` is `public_url` without its trailing `/`.
- Text part: a `blocks` template's compiled text gets `\n\nUnsubscribe: {BASE}/u/{SLOT}`; a raw
  template's text keeps coming from the substituted HTML.
- `freeze`: `Stored::from_column` + `ready(fingerprint(now), defs)`; `FieldsChanged` →
  `FreezeError::Drift`; every other outcome (Ok, `Stale`, `MarkerFormat`, `Malformed`, `TooDeep`,
  no stored column) compiles fresh with `keep_archived = stored_field_names(stored)`.
- Stored preview (`routes::preview`, no body): `Stale` and `MarkerFormat` recompile in memory and
  serve; `FieldsChanged`, `Malformed`, `TooDeep` keep their 409. Still writes no row.
- `render_subject` maps every char in `char::is_control` to a space.

- [ ] **Step 1: Write the failing tests.**
  ```rust
  // mod.rs — issue 51 and 48, red first
  fn a_raw_html_send_rendition_carries_the_postal_address_and_an_absolute_unsubscribe_link()
      // footer address "1 Main St\nTown", public_url https://mail.example.test, token "TOK":
      // html has "1 Main St<br>Town" and href="https://mail.example.test/u/TOK";
      // text has "https://mail.example.test/u/TOK" and "1 Main St"
  fn a_raw_mjml_send_rendition_carries_both()
  fn a_blocks_send_rendition_has_the_address_once_and_the_link()   // "1 Main St" occurs once
  fn the_footer_sits_before_the_real_end_of_body_not_a_commented_one()
      // "<html><body><p>x</p><!-- </body> --></body></html>": footer after the comment
  fn a_document_ending_inside_an_unclosed_comment_or_script_is_footer_hidden()
  fn an_operator_cannot_name_the_unsubscribe_slot()                // UnknownField("unsubscribe_token")
  fn a_token_cannot_leave_its_path_component()                     // token "a/../b?x=1#y": href starts
      // with "https://mail.example.test/u/" and contains no raw '?' or '#' after it
  // mod.rs — issue 49, red first
  fn a_brand_kit_edit_does_not_stop_a_freeze()     // stored with accent #2563eb, freeze with #ff0000:
                                                   // Ok, html contains "#ff0000"
  fn field_drift_still_refuses_a_freeze()          // kind Text -> Select: FreezeError::Drift
  // text.rs
  fn a_subject_tag_renders_as_plain_text_without_line_breaks()
      // first_name "A\r\nBcc: x@y.test", subject "Hi {{ first_name }}" -> "Hi A  Bcc: x@y.test"
  // mail/mod.rs
  fn the_message_id_is_the_one_asked_for()          // <{id.simple()}@send.example.test>
  fn reply_to_is_on_the_wire_and_in_signed_headers()
  // tests/templates.rs — issue 49, red first
  fn previewing_a_stored_template_after_a_brand_kit_edit_is_200()   // today 409 template-stale
  fn previewing_a_stored_template_after_a_field_kind_change_is_still_409()
  ```
- [ ] **Step 2: Run.** `bash scripts/dev.sh cargo test --lib template:: mail:: && bash scripts/dev.sh cargo test --test templates preview`
  Expected: FAIL (`compile_for_send` not defined; preview 409).
- [ ] **Step 3: Implement** `footer.rs`, `compile_for_send`, `freeze`, the slot, the preview
  change, `compile_subject`/`render_subject`, and the two `Outgoing` fields (confirmation passes
  `None` and `Uuid::now_v7()`; `SIGNED_HEADERS` gains `"Reply-To"` — `dkim::signed_fields`
  already signs only the fields present — and `every_signed_header_is_on_the_wire` builds its
  fixture with a reply-to; update `dkim.rs`'s spelled-out `h=` list to match). `Contact::value`
  answers `UNSUBSCRIBE_TOKEN` from the new field; preview passes `"preview"`.
- [ ] **Step 4: Run** the Step 2 commands, then `bash scripts/dev.sh cargo test --lib`.
  Expected: PASS, including the M2b golden fingerprint unchanged.
- [ ] **Step 5: ADR row.** `template::routes::preview_template` stays "nothing" — say that a
  recompile in memory writes no row.
- [ ] **Step 6: Commit.** `git commit -m "Compile a send rendition with the postal address and unsubscribe link inside the classified document"`

---

### Task R3: campaign API, schedule and DST, test-send enqueue, template summary

**Files:**
- Create: `src/campaign/mod.rs`, `src/campaign/routes.rs`, `src/campaign/schedule.rs`
- Modify: `src/lib.rs`, `src/http/mod.rs` (merge in `http::routes()`, never after the CSRF layer),
  `src/openapi.rs`, `src/audit/mod.rs` (actions), `src/http/rate_limit.rs`, `src/app.rs`
  (`test_send_limit`), `src/template/routes.rs` (list returns `TemplateSummary`),
  `src/sending_domains/mod.rs` + `src/worker/confirmation.rs` (`not_passing` moves),
  `web/src/api/openapi.json` + `web/src/api/schema.d.ts` (via `pnpm gen:api`)
- Modify: `issues/50` — item 1 closed by R3
- Test: `tests/campaigns.rs`; unit tests in `schedule.rs`

**Interfaces:**
- Consumes: R1 tables; R2 `freeze`, `SystemFooter`, `compile_subject`, `render_subject`,
  `Ready::render`; `brand::load`, `custom_fields::definitions_for`, `segments::load_rules`,
  `sending_domains` gate (`not_passing` — move it to `sending_domains` as `pub fn not_passing(&SendingDomain) -> Vec<&'static str>`).
- Produces (wire shapes are the contract's §2; these are the Rust names):
  ```rust
  // campaign/mod.rs
  pub enum State { Draft, Scheduled, Sending, Paused, Sent, Cancelled, Failed } // snake_case, sqlx text
  pub enum Mode { Instant, RecipientLocal }                                      // snake_case
  pub struct Variant { pub subject: String, pub template_id: Uuid, pub from_name: Option<String> }
  pub struct Campaign { /* the row */ }                    pub async fn load(db, ws: Uuid, id: Uuid) -> ApiResult<Campaign>
  pub struct CampaignView, CampaignSummary, Counts, Schedule, Estimate, CampaignPreview
  pub struct Frozen { pub from: String, pub reply_to: Option<String>, pub sending_domain_id: Uuid,
                      pub variants: Vec<FrozenVariant> }                      // Serialize only
  pub struct FrozenVariant { pub subject: Vec<Node>, pub from_name: Option<String>, pub rendition: Stored }
  impl Frozen { pub fn from_column(v: &Value) -> Result<Frozen, StoredError> }  // depth-checks subject too
  pub enum Refusal { PostalAddress, DomainUnverified(String), Missing { field: String, what: &'static str },
                     Template { variant: usize, error: String }, Drift { variant: usize, fields: Vec<String> } }
  pub async fn prepare(db: &PgPool, public_url: &url::Url, c: &Campaign) -> Result<Frozen, Refusal>
      // the one validation: lists, segment, domain + gate, local part, address, every variant frozen
  pub const MAX_VARIANTS: usize = 4;
  // campaign/schedule.rs
  pub fn resolve(local: NaiveDateTime, zone: chrono_tz::Tz) -> DateTime<Utc>          // RFC 5545
  pub fn earliest(local: NaiveDateTime) -> DateTime<Utc>
  pub fn zone_instants(local: NaiveDateTime) -> (Vec<String>, Vec<DateTime<Utc>>)    // every TZ_VARIANTS
  // http/rate_limit.rs
  pub const TEST_SEND_PER_MINUTE: u32 = 5;  pub const MAX_TEST_ADDRESSES: usize = 5;
  ```
  Routes, all under `/api/v1/workspaces/{workspace_id}`: `GET,POST /campaigns`;
  `GET,PATCH,DELETE /campaigns/{id}`; `POST /campaigns/{id}/{preview|estimate|test|schedule|unschedule|pause|resume|cancel|winner}`.
  Roles and transitions: spec §4.1. Refusal mapping: `PostalAddress` → 409
  `postal-address-required`; `DomainUnverified` → 409 `sending-domain-unverified`; `Missing` and
  `Template` → 422 on `field` / `variants[{i}].template_id`; `Drift` → 409 `template-stale`;
  wrong state → 409 `campaign-state`.

Handler shapes the ADR rows describe:
- `create` / `update` (draft only): 422 on a missing list, segment, domain or template, a local
  part that is not a dot-atom, a reply-to that is not an address, 0 or more than `MAX_VARIANTS`
  variants, a percentage outside 10–100 with more than one variant, or a subject that does not
  `compile_subject`. No freeze here: that is `prepare`'s.
- `schedule`: `prepare` (outside the transaction), then one transaction: `update campaigns … where
  state = 'draft'` (sets schedule fields, `send_at`, `launch_job_id`), then `insert into jobs
  (kind 'campaign_launch', campaign_id, payload '{}', run_after send_at)`. `send_at` is `now()`
  when `local` is null, `resolve(local, zone)` for `instant`, `earliest(local)` for
  `recipient_local` (422 unless in the future; 422 with more than one variant).
- `unschedule`: `state = 'draft', launch_job_id = null, send_at = null where state = 'scheduled'`.
  The orphaned job no-ops (R5).
- `winner`: `where state in ('sending','paused') and phase = 1 and resolved and
  ab_test_percent < 100` → `winner, phase = 2, resolved = false, resolve_cursor = null,
  launch_job_id`, then insert the phase-2 `campaign_launch` job, run now.
- `resume`: `where state = 'paused'` → `sending`, clearing `pause_reason`; when
  `Frozen::from_column(frozen)` no longer passes `ready(rendition.fingerprint(), defs)` for some
  variant, `prepare` again first and store it with a new `frozen_at` (a `prepare` refusal is the
  answer, and the campaign stays paused). R6 adds the `finish_if_done` call after the update.
- `test`: `prepare`, render variant `v` with the preview sample contact and token `"test"`,
  subject `"[Test] " + render_subject`, then one `campaign_test` job per address, payload
  `{to, subject, html, text, from, from_name, reply_to, sending_domain_id}`. Answer `202
  {queued}`.
- `estimate`: `select count(distinct contact_id)` over `mailable_list_members` for `list_ids`,
  narrowed by the segment, under `set local statement_timeout = '2s'`; on `57014` answer
  `{count: null, exact: false}`. The limiter that backs `PREVIEW_PER_MINUTE`.
- `preview`: `prepare`, then render the variant for the sample contact; `template_preview_limit`.

- [ ] **Step 1: Write the failing tests.**
  ```rust
  // schedule.rs
  fn a_skipped_local_time_moves_forward_by_the_gap()       // 2027-03-14T02:30 New_York -> 2027-03-14T07:30Z
  fn an_ambiguous_local_time_takes_the_first_occurrence()  // 2027-11-07T01:30 New_York -> 2027-11-07T05:30Z
  fn the_earliest_instant_is_the_easternmost_zone()        // 2027-01-10T09:00 -> 2027-01-09T19:00Z
  fn every_zone_a_contact_may_hold_has_an_instant()        // len == chrono_tz::TZ_VARIANTS.len()
  // tests/campaigns.rs
  fn another_workspaces_campaign_is_404()                  // GET, PATCH, DELETE, schedule, test
  fn every_mutating_verb_is_role_gated()   // viewer 403: create, patch, delete, test, pause;
                                           // editor 403: schedule, unschedule, resume, cancel, winner;
                                           // editor 200: pause
  fn content_is_editable_only_in_draft()                   // PATCH scheduled -> 409 campaign-state
  fn repeating_a_transition_is_200_and_a_wrong_one_is_409()
  fn scheduling_without_a_postal_address_is_409()
  fn scheduling_from_an_unverified_unacknowledged_domain_is_409()
  fn scheduling_a_template_that_no_longer_compiles_names_the_variant()  // 422 variants[0].template_id
  fn a_subject_naming_an_unknown_field_is_422()
  fn recipient_local_with_two_variants_is_422()
  fn recipient_local_too_late_for_the_earliest_zone_is_422()
  fn scheduling_enqueues_one_launch_job_at_send_at()       // kind, campaign_id, run_after == send_at
  fn a_dst_schedule_echoes_the_resolved_instant()          // instant mode: the first skipped 02:30 in
      // America/New_York after today (found in-test by stepping days through chrono-tz, so the test
      // never ages out) -> send_at is 03:30 local, the hour after
  fn a_test_send_queues_one_job_per_address_for_the_sample_contact()
      // payload.html contains "Sam" and "/u/test", no seeded contact's name; subject starts "[Test] "
  fn a_sixth_address_is_422_and_a_sixth_test_in_a_minute_is_429()
  fn the_campaign_preview_shows_the_footer()               // "/u/preview" and the address
  fn the_estimate_counts_a_contact_on_two_lists_once_and_skips_the_suppressed()
  fn deleting_is_draft_only()
  fn the_template_list_is_a_summary()                      // items have no "tree" or "source" key
  ```
- [ ] **Step 2: Run.** `bash scripts/dev.sh cargo test --test campaigns && bash scripts/dev.sh cargo test --lib campaign::schedule`
  Expected: FAIL, module `campaign` not found.
- [ ] **Step 3: Implement** `schedule.rs`, `mod.rs`, `routes.rs`; audit actions
  `campaign.created|updated|deleted|scheduled|unscheduled|paused|resumed|cancelled|winner_chosen|test_sent`
  (metadata names, never addresses or content); `TemplateSummary { id, name, kind, created_at, updated_at }`.
- [ ] **Step 4: Run** the Step 2 commands, then the full `cargo test`. Expected: PASS.
- [ ] **Step 5: `pnpm -C web gen:api`** and commit the regenerated `openapi.json` and
  `schema.d.ts`. This is the hand-off to W3.
- [ ] **Step 6: ADR rows** for every handler above: `create_campaign` (the row, `for key share`
  on `workspaces`), `update_campaign` / `pause` / `unschedule` / `cancel` / `resume` (the row
  alone, no key column), `delete_campaign` (the row; the no-action check reads `messages`, which a
  draft has none of; cascades into `jobs`), `schedule` and `winner` (`campaigns` then the new
  `jobs` row, which takes `for key share` on that same campaign and its workspace), `test`
  (`jobs` inserts only).
- [ ] **Step 7: Commit.** `git commit -m "Add the campaign API: drafts, scheduling in a named timezone, test sends and A/B winner"`

---

### Task R4: the message queue — claim, write-ahead, record, sweep, eligibility

The exactly-once core. No SMTP here.

**Files:**
- Create: `src/campaign/queue.rs`
- Modify: `src/worker/mod.rs` (`backoff` becomes `pub(crate)`)
- Test: `tests/campaign_queue.rs`

**Interfaces:**
- Consumes: R1 tables; `worker::backoff`, `worker::JOB_LEASE`.
- Produces:
  ```rust
  pub const CAMPAIGN_BATCH: i64 = 500;
  pub const MESSAGE_MAX_ATTEMPTS: i32 = 5;
  #[doc(hidden)] pub const CLAIM: &str;
  pub struct Claimed { pub id: Uuid, pub workspace_id: Uuid, pub campaign_id: Uuid,
                       pub contact_id: Uuid, pub variant: i16, pub attempts: i32, pub max_attempts: i32 }
  pub async fn claim(db: &PgPool, campaigns: &[Uuid], batch: i64, lease: Duration) -> sqlx::Result<Vec<Claimed>>
  pub async fn mark_submitting(db: &PgPool, m: &Claimed) -> sqlx::Result<bool>
      // where id = $1 and status = 'queued' and attempts = $2  (the claim's count is the fence)
  pub struct Recipient { pub message_id: Uuid, pub email: String, pub first_name: String,
                         pub last_name: String, pub timezone: Option<String>, pub attributes: Value }
  pub async fn eligible(db: &PgPool, ids: &[Uuid]) -> sqlx::Result<Vec<Recipient>>
  pub enum Verdict { Sent { relay: Uuid, response: String }, Retry(String), Deferred(String),
                     Failed(String), Unknown(String), Skipped(String) }
  pub async fn record(db: &PgPool, outcomes: &[(Claimed, Verdict)]) -> sqlx::Result<()>
  pub async fn release(db: &PgPool, ids: &[Uuid], wait: Duration, reason: &str) -> sqlx::Result<()>
  pub async fn sweep(db: &PgPool) -> sqlx::Result<u64>
  pub async fn finish_if_done(db: &mut PgConnection, campaign_id: Uuid) -> sqlx::Result<bool>
  ```

The claim (spec §4.7.1–2), `$1` campaign ids, `$2` per-campaign limit
`ceil(batch / campaigns.len())`, `$3` lease seconds:
```sql
update messages m set attempts = m.attempts + 1, run_after = now() + make_interval(secs => $3)
 where m.id in (select x.id from unnest($1::uuid[]) as c(id)
                cross join lateral (select id from messages
                                     where campaign_id = c.id and status = 'queued'
                                       and run_after <= now() and attempts < max_attempts
                                     order by run_after, id
                                     for update skip locked limit $2) x)
 returning m.id, m.workspace_id, m.campaign_id, m.contact_id, m.variant, m.attempts, m.max_attempts
```
If Postgres refuses the locking clause inside the lateral, claim per campaign, one statement each;
`the_claim_is_the_protocol` holds whichever is used.

`record` is one transaction: one `update … from unnest(…)` over the messages, each verdict
predicated on the state it leaves (`Skipped` from `queued`; every other from `submitting`), then
one counter update per campaign from the `returning` rows, then `finish_if_done`. `Retry` sets
`queued`, `run_after = now() + backoff(attempts)`, or `failed` when `attempts >= max_attempts`.
`Deferred` sets `queued`, `attempts - 1`, `run_after = now() + 5 s`. `finish_if_done`: `state =
'sent', finished_at = now()` where `state = 'sending' and resolved and (ab_test_percent is null or
ab_test_percent = 100 or phase = 2) and total = sent + failed + unknown + skipped`. `eligible` is
one query: `messages` ⋈ `campaigns` ⋈ `contacts`, `exists` in `mailable_list_members` with
`list_id = any(campaigns.list_ids)`. `sweep`: `submitting` with `run_after <= now()` → `unknown`,
`error = 'the sending worker stopped mid-send; not retried'`, counters.

- [ ] **Step 1: Write the failing tests** (Review Focus 1 and 4):
  ```rust
  fn the_claim_is_the_protocol()            // CLAIM contains "for update skip locked", "status = 'queued'",
                                            // "attempts < max_attempts", run_after advanced in the same statement
  fn two_concurrent_claims_are_disjoint()
  fn only_the_named_campaigns_are_claimed()
  fn a_batch_is_shared_fairly_across_campaigns()          // 2 x 1,000 queued, batch 500 -> 250 each
  fn a_stale_lease_holder_cannot_mark_submitting()        // claim (1), expire, reclaim (2): mark(1) false, mark(2) true
  fn a_submitting_row_is_never_claimed()
  fn an_expired_submitting_row_is_swept_to_unknown_and_counted()
  fn a_late_outcome_after_the_sweep_does_not_overwrite_unknown()
  fn a_retry_spends_an_attempt_and_the_fifth_fails()
  fn a_deferral_and_a_release_spend_no_attempt()
  fn recording_moves_the_counters_and_finishes_the_campaign()
  fn an_awaiting_winner_campaign_does_not_finish()        // ab 20, phase 1, all recorded -> still sending
  fn eligible_drops_a_member_who_unsubscribed_or_was_suppressed_after_launch()
  fn eligible_drops_a_deleted_contact()
  ```
- [ ] **Step 2: Run.** `bash scripts/dev.sh cargo test --test campaign_queue` — Expected: FAIL,
  `campaign::queue` not found.
- [ ] **Step 3: Implement `queue.rs`.**
- [ ] **Step 4: Run.** Expected: PASS, five runs in a row.
- [ ] **Step 5: ADR rows**: `queue::claim`, `mark_submitting`, `release` (`messages` rows alone,
  no key column); `record` and `sweep` (`messages`, then `campaigns` by primary key).
- [ ] **Step 6: Commit.** `git commit -m "Add the message queue with a write-ahead submitting state"`

---

### Task R5: launch — freeze and audience resolution

**Files:**
- Create: `src/campaign/launch.rs`
- Modify: `src/worker/mod.rs` (the jobs tick also claims `campaign_launch`, batch 1)
- Test: `tests/campaign_launch.rs`

**Interfaces:**
- Consumes: R3 `prepare`, `Frozen`, `schedule::zone_instants`, `schedule::resolve`; R4
  `finish_if_done`; `worker::{claim, complete, fail, Claimed}`; `segments::load_rules`,
  `segments::compile::push_rules`.
- Produces:
  ```rust
  pub const LAUNCH_KIND: &str = "campaign_launch";
  pub const LAUNCH_BATCH: i64 = 10_000;
  pub const LAUNCH_SLICE: Duration = Duration::from_secs(5);
  pub async fn handle(ctx: &Ctx, job: &worker::Claimed, slice: Duration)   // records on the job itself
  pub struct Resolution { /* campaign, list_ids, segment, phase, percent, variants, winner, seed,
                             default_at, zones: Option<(Vec<String>, Vec<DateTime<Utc>>)> */ }
  pub struct Batch { pub scanned: u64, pub inserted: u64, pub last: Option<Uuid> }
  pub async fn resolve_batch(db: &PgPool, r: &Resolution, after: Option<Uuid>) -> sqlx::Result<Batch>
  ```

Flow (spec §4.2, §4.3, §4.9):
1. Load the campaign. `launch_job_id != job.id`, or state not in (`scheduled`, `sending`,
   `paused`): complete the job, do nothing.
2. `scheduled`: `prepare`. `Err` → `state = 'failed', failure_reason = refusal` (Display), job
   completed. `Ok` → `state = 'sending', frozen, frozen_at, launched_at` where `state =
   'scheduled' and launch_job_id = job.id`.
3. Batches until `LAUNCH_SLICE` is spent or a batch scans fewer than `LAUNCH_BATCH`. Each batch
   is one transaction: the insert-select (contacts by ascending id after the cursor, mailable on
   any of `list_ids`, narrowed by the segment, `on conflict do nothing`), then `update campaigns
   set resolve_cursor = greatest(…), total = total + inserted where id and launch_job_id = job.id
   and state in ('sending','paused')`; zero rows → roll back and stop. Phase 1 with sampling
   keeps a contact when `(hashtextextended(id::text, seed) & 9223372036854775807) % 100 <
   percent` and sets `variant = ((hashtextextended(id::text, seed + 1) & 9223372036854775807)
   % n)`; phase 2 sets `variant = winner`; no A/B sets 0. `run_after` is `now()` (instant) or
   the contact's zone instant from the arrays, else the campaign zone's.
4. Done: `resolved = true`, `finish_if_done`. Not done: in the last batch's transaction, complete
   this job, insert the next `campaign_launch` job (run now) and set `launch_job_id` to it.
5. A phase-2 refusal (segment or list gone) → `state = 'paused', pause_reason`.

- [ ] **Step 1: Write the failing tests** (Review Focus 2 and 5):
  ```rust
  fn a_launch_freezes_every_variant_and_moves_to_sending()
  fn the_audience_is_mailable_members_narrowed_by_the_segment_each_once()
  fn unsubscribed_unconfirmed_and_suppressed_members_are_not_resolved()
  fn a_slice_that_runs_out_of_time_hands_off_and_resumes_without_duplicates() // slice ZERO, 25,000 contacts
  fn rerunning_a_batch_inserts_nothing_twice()                               // total unchanged
  fn a_launch_after_unschedule_does_nothing()
  fn a_deleted_list_fails_the_launch_and_names_it()
  fn a_brand_kit_edit_between_schedule_and_launch_does_not_refuse_it()       // issue 49 at launch
  fn a_template_edit_after_launch_does_not_reach_the_campaign()              // frozen unchanged
  fn ab_sampling_is_deterministic_and_near_the_percent()                     // 10,000 contacts, 20%:
                                                                             // cohort 1,800..=2,200, identical rerun
  fn phase_two_sends_the_winner_to_everyone_not_already_resolved()
  fn recipient_local_sets_run_after_from_each_contacts_zone()
      // local 2027-03-14T09:00, campaign zone America/New_York: Asia/Tokyo contact -> 2027-03-14T00:00Z,
      // New_York contact (after the spring-forward) -> 13:00Z, null zone -> 13:00Z.
      // Fixed dates are safe here: insert the scheduled row directly, since launch never checks
      // that send_at is in the future.
  fn an_empty_audience_finishes_as_sent_with_zero()
  ```
- [ ] **Step 2: Run.** `bash scripts/dev.sh cargo test --test campaign_launch` — Expected: FAIL.
- [ ] **Step 3: Implement `launch.rs` and the jobs-tick claim.** `prepare` already runs `mrml` in
  `spawn_blocking`; nothing here holds a transaction across it.
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: ADR row**: `campaign::launch` — per batch `messages` (new rows, `for key share`
  on the campaign at statement end), then `campaigns`; at hand-off then `jobs` (complete, insert).
- [ ] **Step 6: Commit.** `git commit -m "Launch a campaign: freeze its content and resolve its audience in resumable batches"`

---

### Task R6: the send loop, relay deferral, pause, test-send delivery

**Files:**
- Create: `src/campaign/send.rs`, `src/worker/deliver.rs` (`submit` and `signed` moved out of
  `confirmation.rs`), `src/worker/test_send.rs`
- Modify: `src/relay/transport.rs` (`Outcome::Deferred`), `src/relay/bucket.rs` (`drain`),
  `src/worker/mod.rs` (loops), `src/worker/confirmation.rs`, `src/config.rs`,
  `src/campaign/routes.rs` (resume calls `finish_if_done`), `tests/relay_transport.rs`
  (`a_refused_connection_is_transient` becomes `…_is_deferred`)
- Test: `tests/campaign_send.rs`

**Interfaces:**
- Consumes: R2 `Ready::render`, `render_subject`, `mail::build`; R3 `Frozen::from_column`; R4
  everything; `worker::debit`; `public::token::issue`.
- Produces:
  ```rust
  // relay/transport.rs
  Outcome::Deferred { reason: String, cool_down: Duration }    // 455, or never connected
  pub const DEFER_COOL_DOWN: Duration = Duration::from_secs(60);
  pub const DAILY_COOL_DOWN: Duration = Duration::from_secs(3_600); // reply contains "per day"
  // relay/bucket.rs
  #[doc(hidden)] pub const DRAIN: &str;  // update relay_buckets set tokens = 0, refilled_at = now() + $2 where relay_id = $1
  pub async fn drain(db: &PgPool, relay_id: Uuid, cool_down: Duration) -> sqlx::Result<()>
  // worker/deliver.rs
  pub struct Creds;  impl Creds { pub async fn load(ctx: &Ctx, workspace_id: Uuid, relays: &[Relay]) -> Creds }
  pub async fn submit(ctx: &Ctx, workspace_id: Uuid, relays: &[Relay], paid_for: Option<Uuid>,
                      creds: &Creds, envelope: &Envelope, bytes: &[u8]) -> (Outcome, Option<Uuid>)
      // Deferred falls through to the next relay like Transient, after drain(); Unknown never does
  pub async fn signed(domain: &SendingDomain, key: Option<&dkim::SigningKey>, m: &Message) -> Result<Vec<u8>, Outcome>
  // campaign/send.rs
  pub struct States;  impl States { pub async fn refresh(&self, db: &PgPool) -> sqlx::Result<()>;
      pub fn sending(&self) -> Vec<Uuid>; pub fn is_sending(&self, id: Uuid) -> bool;
      pub fn frozen_at(&self, id: Uuid) -> Option<DateTime<Utc>> }
  pub const STATE_REFRESH: Duration = Duration::from_secs(1);
  pub const FLUSH_EVERY: Duration = Duration::from_millis(250);  pub const FLUSH_AT: usize = 100;
  pub struct CampaignWorker;  impl CampaignWorker {
      pub fn new(ctx: Arc<Ctx>, states: Arc<States>, concurrency: usize) -> Self;
      pub fn with_budget(self, b: Duration) -> Self;  pub async fn tick(&self) -> sqlx::Result<usize> }
  pub async fn system_pause(db: &PgPool, campaign_id: Uuid, reason: &str) -> sqlx::Result<()>
  // worker/test_send.rs
  pub const TEST_KIND: &str = "campaign_test";
  pub async fn handle(ctx: &Ctx, job: &worker::Claimed, paid_for: Option<Uuid>) -> Outcome
  // config.rs
  pub worker_campaign_concurrency: usize   // NM_WORKER_CAMPAIGN_CONCURRENCY, default 32
  // worker_pool_size default 5 -> 10
  ```

The tick (spec §4.7): `sweep`; claim for `States::sending()` with `CAMPAIGN_BATCH` and
`JOB_LEASE`; `eligible` → the rest `Skipped("not mailable since launch")`; per campaign, the
per-tick caches of spec §4.7.9, the domain gate and `ready(rendition.fingerprint(), defs)` — a
failure is `system_pause` and a release of that campaign's messages; `debit` per workspace,
release the unpaid with `RATE_LIMITED_WAIT`; then up to `worker_campaign_concurrency` sends, each:
deadline check → `is_sending` (else release) → `mark_submitting` (false → drop silently) →
render with a token `{workspace_id, contact_id, purpose: Unsubscribe, ref_id: Some(campaign_id),
nonce: message id}` → `mail::build` with `message_id = messages.id`, `feedback_id =
"{campaign_id}:{variant}:{workspace_id}:nm"`, the same URL in the header → `signed` → `submit`
→ verdict to the flusher. Map: `Sent` → `Sent`, `Transient` → `Retry`, `Deferred` → `Deferred`,
`Failed` → `Failed`, `Unknown` → `Unknown`. Confirmation's `handle` maps `Deferred` exactly as it
maps `Transient` today. `run` starts the jobs loop, the messages loop and the `States` refresher
on the worker's own pool.

- [ ] **Step 1: Write the failing tests** with `common::smtp_sink` (Review Focus 1, 3, 4):
  ```rust
  fn every_recipient_gets_one_message_with_its_own_link_address_and_headers()
      // N=20: 20 sink messages, distinct To; List-Unsubscribe <https://mail.example.test/u/…> equals
      // the body link; List-Unsubscribe-Post; Message-ID <{messages.id simple}@brand.test>;
      // Feedback-ID "{campaign}:0:{ws}:nm"; the postal address in both parts
  fn an_app_signed_campaign_message_signs_both_unsubscribe_headers()   // h= has both
  fn the_one_click_link_in_a_campaign_message_works_and_is_attributed()
      // POST that URL, body "List-Unsubscribe=One-Click": 200; suppression live;
      // consent_log.source_ref = campaign id, token_jti = message id
  fn a_one_click_mid_send_skips_the_rest_of_that_campaign()
  fn two_workers_send_each_message_once()                  // two CampaignWorkers tick together: 200 distinct
  fn an_unknown_outcome_is_never_retried_or_failed_over()  // relay 1 AfterData::Drop, relay 2 healthy:
                                                           // relay 2 receives 0; status unknown
  fn a_refused_connection_fails_over_and_takes_a_token_from_the_second_relay()
  fn a_455_releases_with_the_attempt_intact_and_drains_the_bucket()
      // attempts back to 0; relay_buckets.refilled_at >= now() + 55 s
  fn pause_stops_new_submissions_within_five_seconds()
      // sink delays each DATA 300 ms, concurrency 2, 50 queued; pause at t; the sink count at
      // t + 5 s equals the count at t + 8 s
  fn a_deleted_sending_domain_pauses_the_campaign_with_a_reason()
  fn field_drift_mid_send_pauses_and_a_resume_after_a_resave_refreezes()
  fn a_test_send_job_is_delivered_as_a_test_and_respects_suppression()
      // subject starts "[Test] ", Feedback-ID starts "test:"; a suppressed address -> failed, reason
  fn a_confirmation_is_not_held_behind_a_campaign()        // 1,000-message campaign on a slow sink:
                                                           // a confirmation job completes within 3 s
  // tests/relay_transport.rs
  fn a_refused_connection_is_deferred()
  fn a_455_is_deferred_for_a_minute_and_a_daily_455_for_an_hour()
  ```
- [ ] **Step 2: Run.** `bash scripts/dev.sh cargo test --test campaign_send --test relay_transport`
  — Expected: FAIL.
- [ ] **Step 3: Implement** in this order: `Outcome::Deferred` and `drain`; move `submit` and
  `signed` into `deliver.rs` unchanged except the `Creds` cache, the relay returned, and `drain`
  on `Deferred`; `send.rs`; `test_send.rs` (the confirmation handler's step order, from the
  payload, with `unsubscribe_url = {public_url}/u/test` and `feedback_id =
  "test:{campaign_id}:{workspace_id}:nm"`); the loops in `run`; config.
- [ ] **Step 4: Run** Step 2, then `bash scripts/dev.sh cargo test --test worker_pool --test worker_confirmation`.
  Expected: PASS; M2a's suites unchanged except the renamed transport test.
- [ ] **Step 5: ADR rows**: `relay::bucket::drain` (`relay_buckets` update arm only, no key
  column, nothing on `relays`); `campaign::send::system_pause` (the `campaigns` row alone);
  `worker::test_send::handle` (as `confirmation::handle`).
- [ ] **Step 6: Commit.** `git commit -m "Send campaign messages: per-batch eligibility, write-ahead submission, relay deferral and pause"`

---

### Task R7: ADR 0005 re-run, lock-order races, benchmark, issue records

**Files:**
- Modify: `docs/adr/0005-one-lock-order-for-every-path.md`, `tests/lock_order.rs`,
  `.scratch/email-platform/issues/41-…md`, `43-…md`, `50-…md`
- Create: `tests/campaign_bench.rs`

**Interfaces:**
- Consumes: every path row R2–R6 added; `queue::record`, `launch::resolve_batch`, `bucket::DRAIN`.
- Produces: the `campaigns` ruling in the ADR; three race tests; the benchmark.

- [ ] **Step 1: Re-run the enumeration** with the ADR's catalog query against a database migrated
  from nothing. Write the `campaigns` ruling (spec §4.11): not a class, because the only deleter
  matches drafts only and every child insert asks for nothing after its statement. Read every
  R2–R6 path row against its code; fix the row where they differ and name the row in the commit.
- [ ] **Step 2: Write the race tests** with `own_pool`, `settle` and `blocked` (never a
  `#[sqlx::test]` pool for the contenders):
  ```rust
  fn a_launch_batch_and_a_pause_do_not_deadlock()        // both orders commit
  fn an_outcome_flush_and_a_pause_do_not_deadlock()      // both orders commit
  fn a_bucket_drain_takes_no_lock_on_its_relay()         // relays row held for update: drain completes;
                                                         // control: a first take's insert arm blocks
  ```
- [ ] **Step 3: Run.** `bash scripts/dev.sh cargo test --test lock_order` five times. Expected:
  PASS every time. A test that passes on the first try is checked by reordering its two
  statements and watching it fail.
- [ ] **Step 4: Write the benchmark**, `#[ignore]`d:
  `sends_at_the_section_nine_rate_to_a_sink` — 100,000 contacts on one list, a `blocks` template
  with three merge tags, an app-signed domain, a sink relay rated 1,000,000/min, one worker with
  `worker_campaign_concurrency = 64` and `worker_pool_size = 16`. Print launch seconds and
  sustained messages/s (first submission to last record). Assert ≥ 1,500/s only when
  `NM_BENCH_REFERENCE=1`.
- [ ] **Step 5: Run.** `bash scripts/dev.sh cargo test --release --test campaign_bench -- --ignored --nocapture`
  Expected: completes; record the two numbers for the PR.
- [ ] **Step 6: Issues.** 41: `Status: open — campaign half closed by M2c; the transactional
  default remains, for M5`. 43: add question 7 (a Gmail one-click on a real campaign message
  lands in `consent_log` with `source_ref` = the campaign id) and "M2c merged without these; M2
  sign-off waits on all seven". 50: item 1 closed by R3, item 3 open.
- [ ] **Step 7: Commit.** `git commit -m "Re-run the lock-order enumeration for campaigns and add the send throughput benchmark"`

---

# Lane W — web

The contract is the spec's rulings
(`.scratch/email-platform/specs/2026-11-26-m2c-campaigns-and-send-pipeline.md` §4 and §5).
Where code and contract disagree, the
contract wins; a change goes back to the controller.

### Task W1: campaign model, mock source, list screen

**Files:**
- Create: `web/src/routes/campaign-model.ts`, `web/src/routes/campaign-source.ts`,
  `web/src/routes/campaigns.tsx`
- Modify: `web/src/router.tsx` (`campaigns`, `campaigns/$campaignId`, lazy),
  `web/src/routes/workspace-layout.tsx` (nav entry "Campaigns")
- Test: `web/src/routes/campaign-model.test.ts`, `web/src/routes/campaigns.test.tsx`

**Interfaces:**
- Produces (TypeScript names W2 and W3 use; shapes are contract §2):
  ```ts
  // campaign-model.ts
  export type { CampaignState, Mode, Variant, Schedule, Counts, Campaign, CampaignSummary,
                Estimate, CampaignPreview, Option } from contract §2
  export function canEdit(s: CampaignState): boolean                        // draft only
  export function actionsFor(s: CampaignState, role: Role, c: Campaign): Action[]   // contract §4
  export function awaitingWinner(c: Campaign): boolean
  // campaign-source.ts
  export type CampaignSource = { list, get, create, update, remove, preview, estimate, test,
                                 schedule, unschedule, pause, resume, cancel, winner, options }
  export function mockCampaignSource(seed?: Partial<MockState>): CampaignSource   // in-memory, contract §5
  export let campaignSource: CampaignSource                                     // the mock until W3
  ```

- [ ] **Step 1: Write the failing tests.** `actionsFor` for every state × role against contract
  §4's table; the list renders name, state badge, `send_at` and `sent / total`; "New campaign"
  creates a draft and navigates to it; an editor sees no "Send" anywhere on the list.
- [ ] **Step 2: Run.** `pnpm -C web exec playwright test --config playwright.unit.config.ts src/routes/campaign`
  Expected: FAIL.
- [ ] **Step 3: Implement.** Follow `brand-kit-source.ts` for the seam and `templates.tsx` for the
  list.
- [ ] **Step 4: Run, then `pnpm -C web build && pnpm -C web check:bundle`.** Expected: PASS,
  route under 51,200 bytes.
- [ ] **Step 5: Commit and merge into the integration branch.** `git commit -m "Add the campaign list and the campaign model behind a mock source"`

### Task W2: campaign editor

**Files:**
- Create: `web/src/routes/campaign-detail.tsx`, `web/src/components/campaign-editor/audience.tsx`,
  `content.tsx`, `sender.tsx`, `schedule.tsx`, `test-send.tsx`
- Test: `web/src/routes/campaign-detail.test.tsx`

**Interfaces:**
- Consumes: W1's model and `CampaignSource`.
- Produces: `CampaignDetail` route component; for `state !== "draft"` it renders
  `<CampaignProgress>` (W3) — until W3, a placeholder with the state badge.

Behaviour (contract §4–§6): sections Audience (lists multi-select, optional segment, the
estimate with "too many to count quickly" when `exact` is false), Content (1–4 variants:
subject, template picker from `options().templates`, optional from-name; A/B percentage 10–100
when more than one), Sender (domain, local part, name, reply-to), Schedule (now / at a
`datetime-local` in a zone defaulting to the workspace zone; mode; recipient-local disabled with
the reason when there is more than one variant), Test send (1–5 addresses), Preview (iframe of
`preview().html`, plain-text tab). "Send" (admin only) opens a confirm dialog naming the
estimate, the resolved `send_at` and the From address. Every 409/422 shows the server's problem
detail through `problemMessage`, on the field it names when it names one.

- [ ] **Step 1: Write the failing tests:** each section edits and saves through `update`; a
  second variant reveals the percentage and disables recipient-local with its reason; the send
  dialog shows the estimate, the resolved time and the From; a `409 postal-address-required`
  shows its detail and a link to the brand kit; a `422` on `variants[0].template_id` lands on that
  variant's picker; an editor sees "Send test" but no "Send".
- [ ] **Step 2: Run.** `pnpm -C web exec playwright test --config playwright.unit.config.ts src/routes/campaign-detail`
  Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, then build and `check:bundle`.** Expected: PASS, under budget.
- [ ] **Step 5: Commit and merge.** `git commit -m "Add the campaign editor: audience, content and A/B, sender, schedule, test send and preview"`

### Task W3: progress view, real API, e2e

**Blocks on R3.** Merge the integration branch into `m2c-web` first.

**Files:**
- Create: `web/src/components/campaign-editor/progress.tsx`, `web/e2e/campaigns.spec.ts`
- Modify: `web/src/routes/campaign-source.ts` (real `campaignSource` on the generated client; the
  mock stays for tests), `web/src/routes/campaign-detail.tsx`, `web/src/routes/templates.tsx`
  (list rows are `TemplateSummary`)
- Test: `web/src/components/campaign-editor/progress.test.tsx`, the e2e

**Interfaces:**
- Consumes: `schema.d.ts` from R3; W1/W2.
- Produces: `CampaignProgress({ campaign, source })`.

Behaviour (contract §6): counts bar (sent, failed, uncertain = `unknown` labelled "Uncertain —
never retried", skipped, queued); polls `get` every 2 s while `sending`, stops otherwise;
Pause (editor+), Resume / Cancel / Pick winner (admin) per `actionsFor`; `pause_reason` and
`failure_reason` shown verbatim; awaiting winner shows the variants and "no engagement data until
tracking ships" (spec §4.9); Resume on a recipient-local campaign warns that overdue recipients
are sent at once.

- [ ] **Step 1: Write the failing tests:** progress renders each state from mock data; polling
  starts on `sending` and stops on `paused`; a winner pick posts the variant index; the uncertain
  label is present.
- [ ] **Step 2: Write the failing e2e:** create a campaign, fill audience, content and sender
  against seeded lists, a template, a verified-acknowledged domain and a brand kit with an
  address; send a test (202 shown); schedule for tomorrow (state "Scheduled", the resolved time
  shown); unschedule (back to draft); a viewer sees no action buttons.
- [ ] **Step 3: Run.** Unit as W2; `pnpm -C web exec playwright test e2e/campaigns.spec.ts`.
  Expected: FAIL.
- [ ] **Step 4: Implement**, swap in the real source, adapt `templates.tsx` to the summary type.
- [ ] **Step 5: Run unit, e2e, `pnpm -C web typecheck`, build and `check:bundle`.** Expected: PASS.
- [ ] **Step 6: Commit, then merge `m2c-web` into the integration branch.** `git commit -m "Add the campaign progress view and wire the campaign screens to the API"`

---

## Finishing

1. Merge `m2c-web`; remove the worktree.
2. Full gate, serialised, inside the container, `set -eo pipefail`: `cargo fmt --check`,
   `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo sqlx prepare --check --
   --all-targets`, OpenAPI drift, `cargo deny check`, `pnpm install --frozen-lockfile`,
   `pnpm build`, `cargo build --release`, `pnpm test`, `pnpm check:bundle`, e2e.
3. Whole-branch review on the most capable model, lensed by spec §6 and §7.
4. Fix wave; PR with the benchmark numbers; merge to `main`.
