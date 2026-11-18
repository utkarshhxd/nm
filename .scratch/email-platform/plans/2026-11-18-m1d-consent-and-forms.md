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

1. **A logged-in operator opens their own unsubscribe link.** The browser sends a session cookie to a token-authenticated route. The token must decide everything; the cookie must change nothing. (Task 3)
2. **A contact is deleted between a token being issued and used.** The foreign key is gone but the token still verifies. Expect a plain "this link is no longer valid", not a 500. (Task 4)
3. **A list is deleted, or flipped to `private`, while someone holds a preference-center link.** Expect the page to render without that list rather than error. (Task 6)
4. **Two one-click unsubscribes arrive at once** — mail clients prefetch, and some send the POST twice. Expect one consent-log row per actual state change, not one per request. (Task 5)
5. **A form submission names an address already suppressed workspace-wide.** Expect the same response as success, no membership written, and nothing disclosed. (Task 9)

---

## File Structure

| File | Responsibility |
|---|---|
| `migrations/0020_consent_log.sql` | `consent_log`, append-only |
| `migrations/0021_forms_and_jobs.sql` | `forms`, `jobs` |
| `src/public/mod.rs` | module root, `routes()` |
| `src/public/token.rs` | `Purpose`, `Token`, `issue`, `parse` |
| `src/public/confirm.rs` | `GET /c/{token}` |
| `src/public/unsubscribe.rs` | `GET /u/{token}`, `POST /u/{token}` |
| `src/public/forms.rs` | `GET /f/{slug}`, `POST /f/{slug}` |
| `src/public/page.rs` | shared server-rendered page shell |
| `src/consent/mod.rs` | `Action`, `Source`, `record()` |
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
  - `pub fn parse(secrets: &Secrets, raw: &str) -> ApiResult<Token>`
  - `pub const PUBLIC_DOMAIN: &str = "nm:public-token:v1";`

Follow `src/contacts/selection.rs:166-200` exactly in shape: `base64url(json).base64url(hmac)`, `secrets.derive_key(PUBLIC_DOMAIN)`, `tokens::constant_time_eq`. `list_id` is `Some` only for `Confirm`.

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
  - `pub struct Context { pub ip: Option<IpNetwork>, pub user_agent: Option<String>, pub source_ref: Option<String> }`
  - `pub async fn record(tx: &mut PgConnection, workspace_id: Uuid, contact_id: Uuid, list_id: Option<Uuid>, action: Action, source: Source, cx: &Context) -> sqlx::Result<()>`

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
- Modify: `src/lists/routes.rs`, `src/contacts/routes.rs`, `src/contacts/import/staging.rs`
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

## Task 4: Confirming a pending membership

**Files:**
- Create: `src/public/confirm.rs`, `src/public/page.rs`, `tests/public_pages.rs`
- Modify: `src/public/mod.rs`, `src/http/mod.rs` (merge `crate::public::routes()`)

**Interfaces:**
- Consumes: Task 1's `parse`, Task 2's `record()`.
- Produces: `pub fn routes() -> Router<AppState>`; `page::shell(title: &str, body: Markup) -> Response`.

`GET /c/{token}`: parse → `Purpose::Confirm` or refuse → move that one membership `pending` → `subscribed`, set `subscribed_at`, record `Action::Confirmed` with `Source::Form`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn confirming_a_pending_membership_subscribes_it()
#[test] fn confirming_twice_confirms_once_and_says_already_confirmed()      // invariant 5; one consent row, not two
#[test] fn an_expired_confirm_token_offers_to_resend_without_saying_whether_the_address_exists()  // invariant 6
#[test] fn a_confirm_token_for_a_deleted_contact_says_the_link_is_no_longer_valid()  // Review Focus 2 — not a 500
#[test] fn a_session_cookie_on_a_public_route_changes_nothing()             // Review Focus 1
#[test] fn an_unsubscribe_token_is_refused_by_the_confirm_route()
```

- [ ] **Step 2: Run and watch each fail.**
- [ ] **Step 3: Implement.** The page shell is server-rendered HTML with no JavaScript. Re-use the design system's tokens by inlining the few rules needed; do not load the SPA bundle.
- [ ] **Step 4: Run and watch pass.**
- [ ] **Step 5: Commit** `feat: double opt-in confirmation`

---

## Task 5: One-click unsubscribe

**Files:** Create `src/public/unsubscribe.rs`; modify `src/public/mod.rs`; test `tests/public_pages.rs`.

**Interfaces:** Consumes Task 1, Task 2, Task 4's `page::shell`.

`POST /u/{token}` with body `List-Unsubscribe=One-Click` (RFC 8058) unsubscribes the contact from **every** list in that workspace and records one `Action::Unsubscribed` row per list actually changed, `Source::OneClick`. Returns 200 with an empty body. No login. Idempotent.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn one_click_unsubscribes_from_every_list_in_the_workspace()
#[test] fn one_click_completes_in_under_five_seconds()                  // invariant 3, assert elapsed
#[test] fn one_click_twice_writes_consent_rows_only_for_real_changes()  // Review Focus 4
#[test] fn one_click_never_touches_another_workspaces_membership()      // invariant 4
#[test] fn an_unsubscribe_token_two_years_old_still_works()
```

- [ ] **Step 2: Run and watch each fail.**
- [ ] **Step 3: Implement.** One `UPDATE … WHERE status <> 'unsubscribed' RETURNING list_id` so the consent rows follow from what actually changed, not from what was asked.
- [ ] **Step 4: Run and watch pass.**
- [ ] **Step 5: Mutation check** — drop `WHERE status <> 'unsubscribed'` and confirm the idempotence test reds; restore.
- [ ] **Step 6: Commit** `feat: RFC 8058 one-click unsubscribe`

---

## Task 6: The preference center

**Files:** Modify `src/public/unsubscribe.rs`; test `tests/public_pages.rs`.

`GET /u/{token}` renders the page: the workspace's `visibility = 'public'` lists, **plus any non-public list this contact is already on**, each with a checkbox reflecting `status = 'subscribed'`. `POST /u/{token}` with a non-one-click body applies the checkboxes.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn the_page_shows_public_lists_and_the_private_ones_this_contact_is_on()   // invariant 10
#[test] fn a_list_deleted_after_the_link_was_sent_is_simply_absent()               // Review Focus 3
#[test] fn a_list_flipped_to_private_still_shows_for_a_contact_already_on_it()     // Review Focus 3
#[test] fn unticking_a_box_unsubscribes_and_records_preference_center_as_the_source()
#[test] fn ticking_a_box_on_a_double_optin_list_sets_pending_not_subscribed()
#[test] fn the_page_renders_in_under_200ms_for_a_contact_on_50_lists()   // spec §9
```

The latency test is a floor, not a benchmark: assert elapsed on one warm request against a seeded
50-list fixture. It exists to catch an N+1 across lists, which is the only way this page gets slow.

The last is the subtle one: re-subscribing through the preference center to a `double_optin` list must go back through confirmation.

- [ ] **Step 2: Run and watch each fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run and watch pass.**
- [ ] **Step 5: Commit** `feat: the preference center`

---

## Task 7: An unsubscribe is never silently reversed

**Files:** Modify `src/contacts/import/staging.rs`, `src/contacts/routes.rs`, `src/lists/routes.rs`; test `tests/consent.rs`.

Spec invariant 1, and the milestone's anti-goal. A write that would move a membership from `unsubscribed` to `subscribed` or `pending` is refused for every source except `OneClick` and `PreferenceCenter` — the subscriber acting on their own token.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn an_import_naming_an_unsubscribed_contact_leaves_them_unsubscribed()
#[test] fn the_contacts_api_cannot_resubscribe_an_unsubscribed_contact()
#[test] fn the_lists_api_cannot_resubscribe_an_unsubscribed_contact()
#[test] fn a_form_submission_cannot_resubscribe_an_unsubscribed_contact()
#[test] fn the_preference_center_can_resubscribe_because_the_person_asked()
```

- [ ] **Step 2: Run and watch each fail.** At least one will already pass — find out why before changing anything, and say so in the report. A guard that is already enforced elsewhere is this project's most common source of a test that proves nothing.
- [ ] **Step 3: Implement.** Prefer one `WHERE status <> 'unsubscribed'` at each writer over a shared helper nothing else uses.
- [ ] **Step 4: Run and watch pass.**
- [ ] **Step 5: Mutation check each guard separately**, and report which mutation redded which test.
- [ ] **Step 6: Commit** `fix: an unsubscribe is never reversed except by the subscriber`

---

## Task 8: Forms — the model and the authenticated API

**Files:** Create `migrations/0021_forms_and_jobs.sql`, `src/forms/mod.rs`, `src/forms/routes.rs`, `tests/forms.rs`; modify `src/http/mod.rs`, `src/openapi.rs`, `src/lib.rs`.

**Interfaces:**
- Produces: `GET/POST /api/v1/workspaces/{ws}/forms`, `GET/PATCH/DELETE …/forms/{id}`, all `Role::Editor` except `GET` which is `Role::Viewer`.
- `struct Form { id, workspace_id, slug, name, list_ids: Vec<Uuid>, fields: Vec<FormField>, redirect_url: Option<String>, headline: String, button_label: String, created_at, updated_at }`
- `struct FormField { target: ContactField, required: bool, label: String }`

**`ContactField` today lives in `src/contacts/import/mod.rs`** and is the importer's mapping
target. Reusing it couples forms to the import module; defining a second identical enum duplicates
the list of writable contact fields. **Decide and say which you chose in your report** — moving it
up to `src/contacts/mod.rs` and re-exporting is the third option and probably the right one, but
it touches the importer, so it is the implementer's call with the diff in front of them.

`jobs` ships in this migration: `id`, `workspace_id`, `kind`, `payload jsonb`, `run_after timestamptz`, `attempts int`, `created_at`. No worker — M2 owns that.

Validation: at least one list; `slug` matches `^[a-z0-9][a-z0-9-]{1,62}$`, unique per workspace; every `list_id` belongs to the workspace (composite FK, as `0012` does).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn a_form_needs_at_least_one_list()
#[test] fn a_form_naming_another_workspaces_list_is_refused()
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

**Files:** Create `src/public/forms.rs`; modify `src/public/mod.rs`; test `tests/public_pages.rs`.

`GET /f/{slug}` renders the form; `POST /f/{slug}` submits it. **The response is byte-identical for a new address, an existing subscriber, a suppressed address, a honeypot trip and a rate-limit refusal** (invariants 7 and 9) — a redirect to `redirect_url`, or the built-in "check your email" page.

Per list: `double_optin` → `pending` membership + a `jobs` row of kind `confirmation_email` carrying a `Purpose::Confirm` token; otherwise `subscribed` directly. Consent row either way, `Source::Form`, `source_ref` the form id, with the submitter's IP and user agent.

Honeypot: a field named `website`, hidden in CSS, must be empty.

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn a_new_address_on_a_double_optin_list_is_pending_and_enqueues_a_job()
#[test] fn a_new_address_on_a_single_optin_list_is_subscribed_with_no_job()
#[test] fn one_submission_to_a_form_naming_both_kinds_does_both()           // spec §7.3
#[test] fn the_response_is_identical_for_new_existing_and_suppressed()      // invariant 7, compare bytes
#[test] fn a_suppressed_address_gets_no_membership_and_no_job()             // Review Focus 5
#[test] fn a_honeypot_submission_answers_exactly_as_a_real_one()            // invariant 9
#[test] fn the_enqueued_job_carries_a_token_that_the_confirm_route_accepts()
#[test] fn a_submission_completes_in_under_500ms_excluding_the_job()    // spec §9
```

- [ ] **Step 2: Run and watch each fail.**
- [ ] **Step 3: Implement.** Add the per-IP limit to `src/http/rate_limit.rs` with its reasoning in a comment, as `IMPORT_PER_MINUTE` has.
- [ ] **Step 4: Run and watch pass.**
- [ ] **Step 5: Mutation check** — make the suppressed branch return a different status, confirm the byte-identical test reds; restore.
- [ ] **Step 6: Commit** `feat: the hosted signup form`

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

**Files:** Modify `web/src/routes/contact-detail.tsx`, `web/src/routes/lists.tsx`; create `src/consent/routes.rs`; modify `src/http/mod.rs`, `src/openapi.rs`.

`GET /api/v1/workspaces/{ws}/contacts/{id}/consent` — `Role::Viewer`, cursor-paginated as `src/pagination/` does — and a panel on the contact detail screen. The list editor gains `visibility` and `description`.

- [ ] **Step 1: Write the failing tests:** `another_workspaces_consent_history_is_404`, and an e2e asserting a confirmed contact shows "confirmed" with its source.
- [ ] **Step 2: Run and watch fail.**
- [ ] **Step 3: Implement and regenerate the contract.**
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
