# M1d — Consent, the Preference Center, and Signup Forms — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the subscriber's side of the product — sign up, confirm, see what you get, leave — and the append-only record that proves consent.

**Architecture:** Five unauthenticated routes at top-level paths (`/c/`, `/u/`, `/f/`) authenticate by HMAC token instead of a session, following the `issue`/`parse` pattern `src/contacts/selection.rs` already uses. A new `consent_log` table records every membership status change, written by the three existing writers as well as the new ones. The confirmation email is **enqueued** into a `jobs` row; M2 drains it.

**Tech Stack:** Rust 1.96, axum, sqlx 0.9 (offline), Postgres 16, React + TypeScript + Vite, Playwright.

**Spec:** `.scratch/email-platform/specs/2026-11-18-m1d-consent-and-forms.md`

## Global Constraints

- Every tenant table carries `workspace_id`. Asking for another workspace's resource returns **404** — never 403, never the data. **Public endpoints resolve their workspace from the token, never from a path or query parameter.**
- Migrations are append-only once released. `0012_lists.sql` already carries `double_optin boolean not null default true`, `visibility text check (visibility in ('public','private'))`, and `list_memberships.status check (status in ('pending','subscribed','unsubscribed'))`. **M1a decided this. Do not re-model it and do not amend `0012`.**
- Secrets never appear in logs, audit metadata or API responses.
- Windows cannot link Rust here. Every `cargo` / `sqlx` command runs in the Linux dev container: `bash scripts/dev.sh cargo test`. `bash -lc` does not have cargo on PATH.
- Docker: use `nm-postgres-1` and the `--rm` dev container only. `oci-email-postgres`, `marketing-automation` and the two `email-marketing-saas-*` containers belong to other projects — never start, stop, modify or remove them. Compose project is always `-p nm`.
- The e2e harness may only touch databases matching `nm_e2e_[0-9a-f]{12}`.
- `src/http/csrf.rs:45` gates only paths starting with `/api/v1/`, so the public routes are CSRF-exempt by construction. **Public handlers must never read the session cookie**, which a logged-in operator's browser will send anyway.
- Public pages are server-rendered and must work with JavaScript disabled, to WCAG 2.2 AA.
- Never rewrite history on shared branches; never commit directly to `main`.

## Review Focus

Five failure modes the spec implies that no task's happy path exercises. Each has its test added to the task that owns the code.

1. **A logged-in operator opens their own unsubscribe link.** The browser sends a session cookie to a token-authenticated route. The token must decide everything; the cookie must change nothing. (Task 4b)
2. **A contact is deleted between a token being issued and used.** The foreign key is gone but the token still verifies. Expect a plain "this link is no longer valid", not a 500. (Task 4b)
3. **A list is deleted, or flipped to `private`, while someone holds a preference-center link.** Expect the page to render without that list rather than error. (Task 6)
4. **Two one-click unsubscribes arrive at once** — mail clients prefetch, and some send the POST twice. Expect one consent-log row per actual state change, not one per request. (Task 5)
5. **A form submission names an address already suppressed workspace-wide.** Expect the same response as success, no membership written, and nothing disclosed. (Task 9)

---

## File Structure

| File | Responsibility |
|---|---|
| `migrations/0020_consent_log.sql` | `consent_log`, append-only |
| `migrations/0021_consent_requested_action.sql` | widens `consent_log.action` to admit `requested` (Task 4a) |
| `migrations/0022_forms_and_jobs.sql` | `forms`, `form_lists`, `jobs` (Task 8) |
| `migrations/0023_consent_log_paging_index.sql` | `(workspace_id, contact_id, id desc)` on `consent_log` (Task 11) |
| `src/public/mod.rs` | module root, `routes()` |
| `src/public/token.rs` | `Purpose`, `Token`, `issue`, `parse(secrets, raw, expected: Purpose)` |
| `src/public/confirm.rs` | `GET /c/{token}` |
| `src/public/unsubscribe.rs` | `GET /u/{token}`, `POST /u/{token}` |
| `src/public/forms.rs` | `GET /f/{slug}`, `POST /f/{slug}` |
| `src/public/page.rs` | shared server-rendered page shell: `shell`, `esc`, `Cache-Control: no-store` |
| `src/http/rate_limit.rs` | the limiter, keyed on `Workspace(Uuid)` or `Ip(IpAddr)` and bounded (Task 4b) |
| `src/consent/mod.rs` | `Action`, `Source`, `Context`, `record()`, `scrub()` |
| `src/consent/routes.rs` | `GET …/contacts/{id}/consent` (Task 11) |
| `src/forms/mod.rs`, `src/forms/routes.rs` | form model and authenticated CRUD |
| `web/src/routes/forms.tsx`, `form-detail.tsx` | admin UI |
| `tests/consent.rs`, `tests/public_tokens.rs`, `tests/public_pages.rs`, `tests/forms.rs` | Rust integration tests |
| `web/e2e/forms.spec.ts`, `web/e2e/preference-center.spec.ts` | e2e |

---

## Task 1: The public token

**Files:**
- Create: `src/public/mod.rs`, `src/public/token.rs`, `tests/public_tokens.rs`
- Modify: `src/lib.rs` (add `pub mod public;`)

**Interfaces:**
- Consumes: `crate::crypto::{Secrets, tokens}`, `crate::error::{ApiError, ApiResult}`.
- Produces:
  - `pub enum Purpose { Confirm, Unsubscribe, Preference }`
  - `pub struct Token { pub workspace_id: Uuid, pub contact_id: Uuid, pub purpose: Purpose, pub list_id: Option<Uuid>, pub issued_at: i64, pub nonce: String }`
  - `pub fn issue(secrets: &Secrets, token: &Token) -> String`
  - `pub fn parse(secrets: &Secrets, raw: &str, expected: Purpose) -> ApiResult<Token>`
  - `pub const PUBLIC_DOMAIN: &str = "nm:public-token:v1";`

Follow `src/contacts/selection.rs:166-200` exactly in shape: `base64url(json).base64url(hmac)`, `secrets.derive_key(PUBLIC_DOMAIN)`, `tokens::constant_time_eq`. `list_id` is `Some` only for `Confirm`. `expected` is the purpose of the route calling: a genuine token for another purpose is refused as `link-invalid`, so every route passes its own constant.

TTLs, checked in `parse` before any database access: `Confirm` **14 days**, `Preference` **90 days**, `Unsubscribe` **never expires** (RFC 8058 — an email sits in an inbox for years).

- [ ] **Step 1: Write the failing tests** in `tests/public_tokens.rs`

```rust
#[test] fn a_token_round_trips_through_issue_and_parse()
#[test] fn a_token_with_one_byte_altered_is_refused()        // flip a byte in the payload AND in the mac
#[test] fn a_token_signed_with_another_secret_is_refused()
#[test] fn a_confirm_token_older_than_14_days_is_refused()   // assert ApiError::Unprocessable("link-expired")
#[test] fn a_preference_token_older_than_90_days_is_refused()
#[test] fn an_unsubscribe_token_two_years_old_is_accepted()  // the RFC 8058 case
#[test] fn the_purpose_is_part_of_what_is_signed()           // re-serialise with a changed purpose, expect refusal
```

- [ ] **Step 2: Run them and watch each fail**

Run: `bash scripts/dev.sh cargo test --test public_tokens`
Expected: FAIL, unresolved import `nm::public`.

- [ ] **Step 3: Implement `token.rs`**

Refusals are `ApiError::Unprocessable("link-invalid")` and `ApiError::Unprocessable("link-expired")` — two codes, because the pages say different things.

- [ ] **Step 4: Run them and watch each pass**

- [ ] **Step 5: Mutation check** — change `derive_key(PUBLIC_DOMAIN)` to the selection domain and confirm `a_token_signed_with_another_secret_is_refused` reds; restore byte-for-byte.

- [ ] **Step 6: Commit** `git add src/public src/lib.rs tests/public_tokens.rs && git commit -m "feat: signed tokens for the public pages"`

---

## Task 2: The consent log

**Files:**
- Create: `migrations/0020_consent_log.sql`, `src/consent/mod.rs`, `tests/consent.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces:
  - `pub enum Action { Subscribed, Confirmed, Unsubscribed, Resubscribed, Suppressed }`
  - `pub enum Source { Form, Import, Api, Operator, OneClick, PreferenceCenter, Bounce, Complaint }`
  - `pub struct Context { pub ip: Option<IpAddr>, pub user_agent: Option<String>, pub source_ref: Option<String> }` — no `Debug`: `ip` and `user_agent` are personal data
  - `pub async fn record(tx: &mut PgConnection, workspace_id: Uuid, contact_id: Uuid, list_id: Option<Uuid>, action: Action, source: Source, cx: &Context) -> ApiResult<()>`
  - `pub fn scrub(e: sqlx::Error) -> ApiError` — applied inside both writers, so a failed insert never logs the evidence columns

`record` returns `ApiResult` and scrubs its own error inside, so no call site adds a `.map_err`. Task 4a added `Action::Requested` and `Context.token_jti`, folded the last bulk writer's consent insert into its own statement, and **deleted `record_many`**: with it gone, no caller can pass one action for rows that do not share one.

Columns exactly as spec §7.1. Append-only: no `updated_at`, and the table gets a comment saying rows are never updated or deleted. Index `(workspace_id, contact_id, created_at desc)`.

- [ ] **Step 1: Write the failing tests** in `tests/consent.rs`

```rust
#[test] fn a_recorded_consent_row_carries_every_column_it_was_given()
#[test] fn a_consent_row_for_another_workspace_cannot_be_written()  // composite FK refuses
#[test] fn consent_rows_for_a_contact_come_back_newest_first()
```

- [ ] **Step 2: Run and watch fail.** Run: `bash scripts/dev.sh cargo test --test consent`
- [ ] **Step 3: Write `0020_consent_log.sql` and `record()`.** Both foreign keys composite, carrying `workspace_id`, as `0012_lists.sql:31-32` does.
- [ ] **Step 4: Run and watch pass.**
- [ ] **Step 5: Commit** `feat: an append-only consent log`

---

## Task 3: Every existing membership writer records consent

**Files:**
- Modify: `src/lists/mod.rs`, `src/contacts/selection.rs`, `src/contacts/import/staging.rs`
- Test: `tests/consent.rs`

**Interfaces:** Consumes Task 2's `record()`.

This is spec invariant 8 and it is the task most likely to be done shallowly. Find **every** write of `list_memberships.status` in the tree — `grep -rn "list_memberships" src/` — and give each one a consent row in the same transaction.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn subscribing_through_the_lists_api_writes_a_consent_row()        // Source::Operator
#[test] fn subscribing_through_the_contacts_api_writes_a_consent_row()     // Source::Api
#[test] fn an_import_that_subscribes_writes_one_consent_row_per_contact()  // Source::Import, source_ref = import id
#[test] fn every_membership_status_write_in_the_tree_has_a_consent_row()
```

The last one is the real gate: count `list_memberships` rows written by a fixture exercising all three writers, count `consent_log` rows, assert equal. A writer added later with no consent row fails it.

- [ ] **Step 2: Run and watch each fail.**
- [ ] **Step 3: Add the `record()` calls.** Same transaction as the status write, always — a consent row that can outlive a rolled-back membership is worse than none.
- [ ] **Step 4: Run and watch pass.**
- [ ] **Step 5: Mutation check** — delete the `record()` call in the importer and confirm the counting test reds; restore.
- [ ] **Step 6: Commit** `feat: the three existing membership writers record consent`

---

## Task 4a: The consent model tells "asked" from "agreed"

**Files:**
- Create: `migrations/0021_consent_requested_action.sql`
- Modify: `src/consent/mod.rs`, `src/lists/mod.rs`, `src/contacts/selection.rs`, `src/contacts/import/staging.rs`
- Test: `tests/consent.rs`

**Interfaces:**
- Consumes: Task 2's `record()`.
- Produces:
  - `Action::Requested` — a membership written `pending`: the person was asked, and has not agreed.
  - `Context.token_jti: Option<String>` — the nonce of the public token that authorised the change. `record` writes the `consent_log.token_jti` column; operator and API paths pass `None`. Tasks 4b, 5 and 6 pass the parsed token's `nonce`.

A log that records `pending` as `subscribed` cannot prove consent, and a log that records *that* someone confirmed but not *which link they clicked* loses the how (spec §4, first anti-goal). Both are fixed here so every public route after this task only fills the field.

Each writer decides its action from the status it wrote: `pending` records `Requested`, `subscribed` records `Subscribed`. `selection::subscribe` takes one `status`, so it maps that to one `Action` in Rust. `lists::add_member` already knows its status from `double_optin`.

**Resolved.** The importer decides its action in its own SQL (`case` over the status it wrote) with its consent insert folded into its statement, and `record_many` is deleted. That is the only shape that can express a per-row action, and it removes the last UUID round-trip.

- [ ] **Step 1: Write the failing tests** in `tests/consent.rs`

```rust
#[test] fn adding_to_a_double_optin_list_through_the_lists_api_records_requested()
#[test] fn subscribing_a_selection_to_a_double_optin_list_records_requested()
#[test] fn an_import_records_requested_for_a_double_optin_list_and_subscribed_for_a_single_optin_one()  // one import, both kinds: the per-row case
#[test] fn adding_to_a_single_optin_list_still_records_subscribed()
#[test] fn record_writes_the_token_jti_it_was_given()
#[test] fn an_operator_or_api_write_leaves_token_jti_null()
```

Any existing assertion in `tests/consent.rs` that expects `subscribed` for a membership written `pending` changes with this task; find them with `grep -n '"subscribed"' tests/consent.rs`.

- [ ] **Step 2: Run and watch each fail.** Run: `bash scripts/dev.sh cargo test --test consent`
- [ ] **Step 3: Write `0021_consent_requested_action.sql`** widening the `consent_log.action` check to admit `requested`. Do not amend `0020`.
- [ ] **Step 4: Add `Action::Requested` and `Context.token_jti`**, and write the column in `record`.
- [ ] **Step 5: Decide each writer's action** as above, and settle the open item against Task 3's landed code.
- [ ] **Step 6: Run and watch pass.**
- [ ] **Step 7: Mutation check** — make the importer record `Subscribed` for its `pending` rows and confirm the mixed-import test reds; restore.
- [ ] **Step 8: Commit** `feat: the consent log tells asked from agreed`

---

## Task 4b: The public surface's foundation, and `GET /c/{token}`

Shares no file with Task 4a; the two may run in parallel. Task 5 onward depends on both.

**Files:**
- Create: `src/public/confirm.rs`, `src/public/page.rs`, `tests/public_pages.rs`
- Modify: `src/public/mod.rs`, `src/http/mod.rs` (merge `crate::public::routes()`), `src/http/rate_limit.rs`, `tests/consent.rs` (the writer array)

**Interfaces:**
- Consumes: Task 1's `parse(secrets, raw, expected)`, Task 2's `record()`, Task 4a's `Context.token_jti`.
- Produces:
  - `pub fn routes() -> Router<AppState>`
  - `page::shell(title: &str, body: String) -> Response` — sets `Cache-Control: no-store`
  - `page::esc(s: &str) -> String` — escapes `& < > " '`
  - `RateLimiter::check` keyed on an enum, `Workspace(Uuid)` or `Ip(IpAddr)`, over a map with an explicit capacity bound.

**The limiter first.** `RateLimiter` is `Mutex<HashMap<Uuid, Window>>`, and the comment above its `retain` justifies an unbounded map by saying `check` is reached only after `authorize` succeeds. An IP-keyed limiter on unauthenticated routes makes that sentence false: an IPv6 /64 supplies unbounded distinct keys while `retain` walks the map under a `std::sync::Mutex` on an async worker. This task changes the key to the enum, gives the map a capacity bound with a stated eviction rule, and rewrites that comment in the same commit. It also adds a per-IP constant beside `IMPORT_PER_MINUTE`, with its reasoning in a comment. Tasks 5, 6 and 9 inherit a limiter that is already correct; all five public routes use it (spec §7.2).

**The page shell.** No templating dependency is added: `Cargo.toml` is curated and Task 13 runs `check-spdx.sh` and `check:licenses`. `shell` takes the body as a `String`, and every operator-supplied value that reaches it (the preference center renders list names) goes through `esc`. `shell` sets `Cache-Control: no-store` because `src/http/security_headers.rs` applies it only to `/api*`, `/healthz` and `/readyz`, and these pages show which lists a named person is on behind a reverse proxy.

`GET /c/{token}`: rate-limit by IP, then parse → `Purpose::Confirm` or refuse → move that one membership `pending` → `subscribed`, set `subscribed_at`, record `Action::Confirmed` with `Source::Form` and `token_jti` set to the token's nonce.

- [ ] **Step 1: Write the failing tests**

```rust
// tests/public_pages.rs
#[test] fn confirming_a_pending_membership_subscribes_it()
#[test] fn confirming_twice_confirms_once_and_says_already_confirmed()      // invariant 5; one consent row, not two
#[test] fn the_confirmation_row_carries_the_token_nonce_as_its_jti()
#[test] fn an_expired_confirm_token_offers_to_resend_without_saying_whether_the_address_exists()  // invariant 6
#[test] fn a_confirm_token_for_a_deleted_contact_says_the_link_is_no_longer_valid()  // Review Focus 2 — not a 500
#[test] fn an_unsubscribe_token_is_refused_by_the_confirm_route()
#[test] fn a_session_cookie_on_a_public_route_changes_nothing()             // Review Focus 1; cheap, but cannot fail unless the bug is written
#[test] fn no_handler_under_src_public_names_cookiejar()                    // Review Focus 1's real proof: walk src/public as the writer-list walk in tests/consent.rs does
#[test] fn every_public_page_is_no_store()
#[test] fn the_confirm_route_is_refused_past_the_per_ip_limit()

// unit tests beside the code
#[test] fn esc_escapes_each_of_the_five_characters()                        // page.rs: one assertion per & < > " '
#[test] fn esc_escapes_a_string_containing_all_five()                       // page.rs
#[test] fn the_limiter_map_never_grows_past_its_cap_however_many_ips_arrive()  // rate_limit.rs: N distinct IPs, N over the cap
```

- [ ] **Step 2: Run and watch each fail.**
- [ ] **Step 3: Change the limiter** — the enum key, the capacity bound and eviction rule, the per-IP constant, and the rewritten comment at the `retain` shortcut — and watch the bound test pass.
- [ ] **Step 4: Commit** `fix: the rate limiter is keyed by workspace or IP and bounded`
- [ ] **Step 5: Implement `page.rs`.** Server-rendered HTML with no JavaScript. Re-use the design system's tokens by inlining the few rules needed; do not load the SPA bundle.
- [ ] **Step 6: Implement `confirm.rs`** and merge `crate::public::routes()` into `src/http/mod.rs`.
- [ ] **Step 7: Add `public/confirm.rs` to the writer array** in `tests/consent.rs`, in the same commit as the writer. A red there is the fixture working.
- [ ] **Step 8: Run and watch pass.**
- [ ] **Step 9: Mutation check** — lift the capacity bound and confirm the bound test reds; make `esc` skip `'` and confirm the five-character test reds; restore both.
- [ ] **Step 10: Commit** `feat: the public page shell and double opt-in confirmation`

---

## Task 5: One-click unsubscribe

**Files:** Create `src/public/unsubscribe.rs`; modify `src/public/mod.rs`, `src/public/token.rs`, `tests/public_tokens.rs`, `tests/consent.rs` (the writer array); test `tests/public_pages.rs`.

**Interfaces:** Consumes Task 1's `parse(secrets, raw, Purpose::Unsubscribe)`, Task 2's `record()`, Task 4a's `Context.token_jti`, and Task 4b's `page::shell` and per-IP limiter.

`POST /u/{token}` with body `List-Unsubscribe=One-Click` (RFC 8058) unsubscribes the contact from **every** list in that workspace and records one `Action::Unsubscribed` row per list actually changed, `Source::OneClick`, `token_jti` set to the token's nonce. Returns 200 with an empty body. No login. Idempotent. Rate-limited per IP.

**Both methods of `/u/{token}` parse `Purpose::Unsubscribe`** (spec §6.3: one page, three doors, and the RFC 8058 POST lands on the same token). `Purpose::Preference`, its 90-day TTL and `a_preference_token_older_than_90_days_is_refused` are deleted in this task: no route issues or accepts that purpose, and a 90-day TTL on this link would break spec §6.2's "unsubscribe tokens do not expire".

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn one_click_unsubscribes_from_every_list_in_the_workspace()
#[test] fn one_click_completes_in_under_five_seconds()                  // invariant 3, assert elapsed
#[test] fn two_overlapping_one_click_posts_write_consent_rows_only_for_real_changes()  // Review Focus 4: issued concurrently, not in sequence
#[test] fn one_click_never_touches_another_workspaces_membership()      // invariant 4
#[test] fn an_unsubscribe_token_two_years_old_still_works()
#[test] fn a_one_click_consent_row_carries_the_token_nonce_as_its_jti()
#[test] fn one_click_is_refused_past_the_per_ip_limit()
```

- [ ] **Step 2: Run and watch each fail.**
- [ ] **Step 3: Implement.** One `UPDATE … WHERE status <> 'unsubscribed' RETURNING list_id` so the consent rows follow from what actually changed, not from what was asked.
- [ ] **Step 4: Add `public/unsubscribe.rs` to the writer array** in `tests/consent.rs`, in the same commit as the writer.
- [ ] **Step 5: Delete `Purpose::Preference`**, its TTL arm and its test.
- [ ] **Step 6: Run and watch pass.**
- [ ] **Step 7: Mutation check** — drop `WHERE status <> 'unsubscribed'` and confirm the overlapping-POSTs test reds; restore.
- [ ] **Step 8: Commit** `feat: RFC 8058 one-click unsubscribe`

---

## Task 6: The preference center

**Files:** Modify `src/public/unsubscribe.rs`; test `tests/public_pages.rs`.

`GET /u/{token}` parses `Purpose::Unsubscribe` and renders the page: the workspace's `visibility = 'public'` lists, **plus any non-public list this contact is already on**, each with a checkbox reflecting `status = 'subscribed'`. `POST /u/{token}` with a non-one-click body applies the checkboxes. Both methods are rate-limited per IP and record `token_jti` as the token's nonce. The preference center is `Action::Resubscribed`'s writer: here the subscriber is the one acting, through their own token.

**Ticking a box here subscribes, even on a double-opt-in list — it does not go to `pending`.** Double opt-in exists to prove the address belongs to the person using it, and a valid token delivered to that address is that proof. Asking them to confirm again would be asking them to prove the same thing twice, and would leave a `pending` row for someone who has plainly agreed. So the row written is `subscribed` and the action recorded is `Resubscribed` (or `Subscribed` for a list they were never on), never `Requested`. One test asserts this for a double-opt-in list.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn the_page_shows_public_lists_and_the_private_ones_this_contact_is_on()   // invariant 10
#[test] fn a_list_deleted_between_the_page_and_the_post_does_not_fail_the_post()   // Review Focus 3: render, delete the list, then POST the rest
#[test] fn a_list_flipped_to_private_still_shows_for_a_contact_already_on_it()     // Review Focus 3
#[test] fn unticking_a_box_unsubscribes_and_records_preference_center_as_the_source()
#[test] fn ticking_a_box_for_a_list_the_contact_left_records_resubscribed()
#[test] fn ticking_a_box_on_a_double_optin_list_subscribes_outright_and_never_writes_pending()
#[test] fn a_preference_center_change_carries_the_token_nonce_as_its_jti()
#[test] fn the_preference_center_is_refused_past_the_per_ip_limit()
#[test] fn the_page_renders_in_under_200ms_for_a_contact_on_50_lists()   // spec §9
```

The latency test is a floor, not a benchmark: assert elapsed on one warm request against a seeded
50-list fixture. It exists to catch an N+1 across lists, which is the only way this page gets slow.

The double-opt-in test is the subtle one, and it asserts the **opposite** of what the rest of this milestone does: everywhere else a `double_optin` list produces `pending`, and here it must not. Assert both halves — the membership is `subscribed`, and no `consent_log` row for that contact and list has action `requested`. Asserting only the first would pass if a stray `requested` row were also written.

- [ ] **Step 2: Run and watch each fail.**
- [ ] **Step 3: Implement.** The writes are in `src/public/unsubscribe.rs`, which Task 5 already added to the writer array in `tests/consent.rs`; confirm it is there and add nothing.
- [ ] **Step 4: Run and watch pass.**
- [ ] **Step 5: Commit** `feat: the preference center`

---

## Task 7: An unsubscribe is never silently reversed

**Files:** Modify `src/lists/mod.rs`, `src/contacts/selection.rs` (a doc comment only); test `tests/consent.rs`.

Spec invariant 1, and the milestone's anti-goal. A write that would move a membership from `unsubscribed` to `subscribed` or `pending` is refused for every source except `OneClick` and `PreferenceCenter` — the subscriber acting on their own token.

Of the five membership writers exactly one breaks this: `lists::add_member` revives an unsubscribed membership with an `on conflict … do update … where list_memberships.status = 'unsubscribed'` arm, on the reasoning that an admin answers for one named person. An operator asserting consent on a subscriber's behalf is what consent law forbids, and spec §8.1 says an API call naming an unsubscribed address leaves the membership `unsubscribed`. **This task deletes that arm** and the `Action::Resubscribed` branch beside it in `add_member`, and rewrites the assertion in `tests/consent.rs` that expects `["resubscribed", "subscribed", "unsubscribed"]` (the add after an unsubscribe now records nothing). `Action::Resubscribed` stays: Task 6's preference center writes it, which is why this task runs after Task 6. The other four writers already satisfy the invariant: `selection::subscribe` and the importer end in `on conflict … do nothing`, and `selection::unsubscribe` and `lists::unsubscribe_member` already carry `status <> 'unsubscribed'`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn an_import_naming_an_unsubscribed_contact_leaves_them_unsubscribed()
#[test] fn the_contacts_api_cannot_resubscribe_an_unsubscribed_contact()
#[test] fn the_lists_api_cannot_resubscribe_an_unsubscribed_contact()
#[test] fn the_preference_center_can_resubscribe_because_the_person_asked()
#[test] fn a_confirmation_link_that_predates_an_unsubscribe_does_not_revive_the_membership()  // /c/ is a writer too
```

- [ ] **Step 2: Run and watch them.** Exactly one fails: `the_lists_api_cannot_resubscribe_an_unsubscribed_contact`. The other four already pass — the import and contacts API because their writers end in `do nothing`, the preference center because Task 6 shipped it, and the confirm route because `src/public/confirm.rs` already guards on `status = 'pending'`. Say so in the report; that is expected, not a guard that proves nothing. The confirm-route test is here because Task 4b's review found that `confirm.rs` *claims* invariant 1 in a comment and nothing held it — a claim with no test is how this branch keeps shipping untrue statements.
- [ ] **Step 3: Implement.** Delete the revival arm and branch in `lists::add_member`, and rewrite the one `tests/consent.rs` assertion. No other writer changes.
- [ ] **Step 3b: Rewrite the doc comment on `selection::subscribe`** that calls `lists::add_member`'s revival of an unsubscribed membership deliberate. It becomes false with this task. Make it say what is then true: no writer revives an unsubscribed membership, and only the subscriber acting through their own token can, via the preference center.
- [ ] **Step 4: Run and watch pass.**
- [ ] **Step 5: Mutation check** — restore the revival arm and confirm `the_lists_api_cannot_resubscribe_an_unsubscribed_contact` reds; then drop `status <> 'unsubscribed'` from `lists::unsubscribe_member` and report which test, if any, reds.
- [ ] **Step 6: Commit** `fix: an unsubscribe is never reversed except by the subscriber`

---

## Task 8: Forms — the model and the authenticated API

**Files:** Create `migrations/0022_forms_and_jobs.sql`, `src/forms/mod.rs`, `src/forms/routes.rs`, `tests/forms.rs`; modify `src/http/mod.rs`, `src/openapi.rs`, `src/lib.rs`.

**Interfaces:**
- Produces: `GET/POST /api/v1/workspaces/{ws}/forms`, `GET/PATCH/DELETE …/forms/{id}`, all `Role::Editor` except `GET` which is `Role::Viewer`.
- `struct Form { id, workspace_id, slug, name, list_ids: Vec<Uuid>, fields: Vec<FormField>, redirect_url: Option<String>, headline: String, button_label: String, created_at, updated_at }` — `list_ids` is the Rust-side projection of the `form_lists` rows.
- `struct FormField { target: FormTarget, required: bool, label: String }`
- `enum FormTarget { Email, FirstName, LastName, Timezone, Custom(String) }`

The forms module owns `FormTarget` rather than reusing the importer's `ContactField`: that enum carries `Lists`, so a public form field targeting it would let a submitter name the lists they join, and it has no custom-field case, which spec §7.3 requires.

The migration creates `forms`, `form_lists (workspace_id, form_id, list_id)` and `jobs`. `form_lists` carries a composite foreign key onto `lists (workspace_id, id)`, as `0012_lists.sql:31-32` does, because Postgres has no foreign key on an array element. `jobs`: `id`, `workspace_id`, `kind`, `payload jsonb`, `run_after timestamptz`, `attempts int`, `created_at`. No worker — M2 owns that.

Validation: at least one list; `slug` matches `^[a-z0-9][a-z0-9-]{1,62}$`, unique per workspace; every `list_id` belongs to the workspace, which `form_lists`' composite foreign key enforces.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn a_form_needs_at_least_one_list()
#[test] fn a_form_naming_another_workspaces_list_is_refused()   // the form_lists composite FK refuses
#[test] fn a_form_field_targeting_lists_is_refused()
#[test] fn a_form_field_may_target_a_custom_field()
#[test] fn two_forms_in_one_workspace_cannot_share_a_slug()
#[test] fn the_same_slug_in_two_workspaces_is_fine()
#[test] fn another_workspaces_form_is_404_not_403()
#[test] fn a_viewer_may_read_forms_and_an_editor_may_write_them()
```

- [ ] **Step 2: Run and watch each fail.**
- [ ] **Step 3: Implement, then regenerate the contract.** `bash scripts/dev.sh cargo run -q -- openapi > web/src/api/openapi.json` then `bash scripts/dev.sh pnpm --dir web gen:schema`.
- [ ] **Step 4: Run and watch pass.**
- [ ] **Step 5: Commit** `feat: signup forms and the job queue`

---

## Task 9: Forms — the public page and submission

**Files:** Create `src/public/forms.rs`; modify `src/public/mod.rs`, `tests/consent.rs` (the writer array); test `tests/public_pages.rs`.

`GET /f/{slug}` renders the form; `POST /f/{slug}` submits it. Both are rate-limited per IP with Task 4b's limiter. **The response is byte-identical for a new address, an existing subscriber, a suppressed address, a honeypot trip and a rate-limit refusal** (invariants 7 and 9) — a redirect to `redirect_url`, or the built-in "check your email" page.

**A rate-limited submission renders that same success page. It does not answer `429` with `RateLimit-*` headers**, as every other limited route in this tree does. That departure is deliberate and required by spec §7.2 and §8.9: the route must not disclose anything, and a distinguishable refusal tells a scraper it has been seen.

Per list: `double_optin` → `pending` membership + one `jobs` row of kind `confirmation_email` per double-opt-in list, each carrying its own `Purpose::Confirm` token (`parse` requires `list_id` on exactly the confirm tokens, so one token cannot cover two lists); otherwise `subscribed` directly. A form on two double-opt-in lists therefore sends two emails, honestly: they are two consents. Consent row either way, `Source::Form`, `source_ref` the form id, with the submitter's IP and user agent.

Honeypot: a field named `website`, hidden in CSS, must be empty.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn a_new_address_on_a_double_optin_list_is_pending_and_enqueues_a_job()
#[test] fn a_new_address_on_a_single_optin_list_is_subscribed_with_no_job()
#[test] fn one_submission_to_a_form_naming_both_kinds_does_both()           // spec §7.3; asserts one jobs row per double-opt-in list
#[test] fn the_response_is_identical_for_new_existing_and_suppressed()      // invariant 7, compare bytes
#[test] fn a_suppressed_address_gets_no_membership_and_no_job()             // Review Focus 5
#[test] fn a_honeypot_submission_answers_exactly_as_a_real_one()            // invariant 9
#[test] fn a_rate_limited_submission_answers_exactly_as_a_real_one()        // invariant 9: the success page, not a 429
#[test] fn a_form_submission_cannot_resubscribe_an_unsubscribed_contact()   // spec invariant 1; moved here from Task 7
#[test] fn the_enqueued_job_carries_a_token_that_the_confirm_route_accepts()
#[test] fn a_submission_completes_in_under_500ms_excluding_the_job()    // spec §9
```

- [ ] **Step 2: Run and watch each fail.**
- [ ] **Step 3: Implement.** Apply Task 4b's per-IP limiter to both methods, rendering the success page on a refusal.
- [ ] **Step 4: Add `public/forms.rs` to the writer array** in `tests/consent.rs`, in the same commit as the writer.
- [ ] **Step 5: Run and watch pass.**
- [ ] **Step 6: Mutation check** — make the suppressed branch return a different status, confirm the byte-identical test reds; restore.
- [ ] **Step 7: Verify the redirect against `form-action 'self'` in a real browser.** `src/http/security_headers.rs` sets `form-action 'self'` on every response, and whether a 302 after a form POST to an external `redirect_url` is checked against it differs by browser. Run it through the e2e harness in Chromium and Firefox. If either refuses, the route renders a success page carrying a link to `redirect_url` instead of issuing the 302. Do not widen `form-action`: that weakens the policy for every page to fix one route.
- [ ] **Step 8: Commit** `feat: the hosted signup form`

---

## Task 10: Admin UI — forms

**Files:** Create `web/src/routes/forms.tsx`, `web/src/routes/form-detail.tsx`; modify `web/src/router.tsx`, `web/src/routes/workspace-layout.tsx`.

Follow `contacts.tsx` and `lists.tsx` for table, dialog and error patterns; use the house components (`Dialog`, `Select`, `Checkbox`, `EmptyState`, `ErrorState`, `TableScroller`). The embed snippet is a read-only `<textarea>` with a copy button, containing a plain HTML `<form>` posting to `/f/{slug}` — no JavaScript required of the embedder.

- [ ] **Step 1: Write the failing e2e** in `web/e2e/forms.spec.ts`: create a form, see the snippet, edit it, delete it.
- [ ] **Step 2: Run and watch it fail.** Run: `bash scripts/dev.sh pnpm --dir web e2e -- forms.spec.ts`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run and watch pass, and check the log for `did not run`** — a serial-mode failure prints it for every test after the first, which reads as a pass in a summary.
- [ ] **Step 5: Commit** `feat: the forms admin screens`

---

## Task 11: Admin UI — consent history and list visibility

**Files:** Create `migrations/0023_consent_log_paging_index.sql`, `src/consent/routes.rs`; modify `web/src/routes/contact-detail.tsx`, `web/src/routes/lists.tsx`, `src/http/mod.rs`, `src/openapi.rs`.

`GET /api/v1/workspaces/{ws}/contacts/{id}/consent` — `Role::Viewer`, cursor-paginated as `src/pagination/` does — and a panel on the contact detail screen. The list editor gains a `visibility` control; `description` is already shipped end to end.

The route orders by `id desc`, not `created_at`: a membership write and its consent row share a transaction, and both set-based writers insert a whole batch in one statement, so `created_at` ties and cannot page stably. UUID v7 is time-ordered, so `id desc` is also newest-first. Migration `0023` adds `(workspace_id, contact_id, id desc)` to serve it.

- [ ] **Step 1: Write the failing tests:** `another_workspaces_consent_history_is_404`, `consent_history_pages_without_skipping_or_repeating_rows_that_share_a_timestamp`, and an e2e asserting a confirmed contact shows "confirmed" with its source.
- [ ] **Step 2: Run and watch fail.**
- [ ] **Step 3: Implement, and regenerate the contract** — the new consent route changes it; the list editor does not.
- [ ] **Step 4: Run and watch pass.**
- [ ] **Step 5: Commit** `feat: consent history and public list settings`

---

## Task 12: The public pages are accessible and survive the harness

**Files:** Create `web/e2e/preference-center.spec.ts`; modify `src/public/page.rs`.

- [ ] **Step 1: Write the failing e2e:** confirm a pending membership, open the preference center, untick a list, assert the change; run the axe pass on all three public pages at 100% and 200% zoom, in both themes.
- [ ] **Step 2: Run and watch fail.**
- [ ] **Step 3: Fix the markup** until axe is clean: a `<label>` per checkbox, a visible focus ring, a live region for the result, and a `<title>` per page.
- [ ] **Step 4: Run and watch pass**, checking for `did not run`.
- [ ] **Step 5: Verify with JavaScript disabled** — `page.context().setJavaScriptEnabled(false)` — that the preference center still submits. This is the spec's constraint and nothing else tests it.
- [ ] **Step 6: Commit** `feat: the public pages meet WCAG 2.2 AA and work without JavaScript`

---

## Task 13: The full gate

- [ ] **Step 1:** `bash scripts/dev.sh cargo fmt --check`, `clippy --all-targets -- -D warnings`, `cargo test`, `cargo sqlx prepare --check -- --all-targets`, `bash scripts/check-spdx.sh`.
- [ ] **Step 2:** `pnpm --dir web typecheck`, `build`, `test`, `check:bundle`, `check:licenses`.
- [ ] **Step 3:** Regenerate and diff the contract: the generator's own exit status first, then the diff — **never `cargo run … | diff -`**, because a pipeline's status is the last command's.
- [ ] **Step 4:** `bash scripts/dev.sh pnpm --dir web e2e`, and grep the log for `did not run`.
- [ ] **Step 5: Commit** any regenerated artifacts.
