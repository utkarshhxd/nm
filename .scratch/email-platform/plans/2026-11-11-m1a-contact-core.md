# M1a Contact Core Implementation Plan

**Goal:** A workspace can hold contacts with typed custom fields, organise them into lists and tags with per-list subscription status, block addresses with workspace and instance-wide suppressions, erase a contact for GDPR, and act on a million-row selection — through the REST API and a web UI that stays usable at ten million rows.

**Architecture:**
- New Rust modules by domain: `contacts`, `lists`, `tags`, `suppressions`, each with `mod.rs` plus `routes.rs`, merged into the app router exactly as `workspaces` is.
- Tenancy is structural: composite foreign keys `(workspace_id, id)` make a cross-workspace row unrepresentable, and every handler still calls `auth::authorize` first.
- Eligibility ("may we email this") is one SQL view plus one single-address lookup. Nothing stores it.
- Frontend gains a token layer and hand-built primitives; the contacts table is virtualised over keyset pagination.

**Tech Stack:** Unchanged from M0, plus the `pg_trgm` Postgres extension and one new frontend dependency, `@tanstack/react-virtual@3.14.26` (MIT).

**Spec:** `.scratch/email-platform/specs/2026-11-11-m1a-contact-core.md`. Read it before starting; it records why several of these decisions are what they are. The product spec is `.scratch/email-platform/spec.md`; M0's conventions are in `.scratch/email-platform/m0-decisions.md`.

## Global Constraints

Everything in M0's Global Constraints still applies. In addition:

**Environment**
- Windows cannot link Rust programs. Every `cargo` / `sqlx` / `cargo deny` command runs inside the Linux dev container: `bash scripts/dev.sh <command>`.
- Run the full suite as `bash scripts/dev.sh cargo test -- --test-threads=4`. Unthrottled it exhausts the WSL2 VM and kills the Docker engine.
- Never stop, modify or remove the containers `oci-email-postgres` or `marketing-automation`. They belong to other projects. Always use compose project `-p nm`.

**After any change to an API route or its types**
- Regenerate the OpenAPI document and the TypeScript client, and commit both: `bash scripts/dev.sh pnpm -C web gen:api` (which regenerates `web/src/api/openapi.json` and `web/src/api/schema.d.ts`). CI fails if either is stale.
- Re-run `bash scripts/dev.sh cargo sqlx prepare -- --all-targets` and commit `.sqlx/`. CI builds offline.
- Every `operationId` must be unique across the whole document; `src/openapi.rs` has a test that fails otherwise. utoipa derives the id from the handler's function name, so name handlers distinctly (`list_contacts`, not `list`).

**Data rules**
- IDs are UUIDv7 generated in Rust (`Uuid::now_v7()`). Postgres 16 has no `uuidv7()`.
- Every tenant table carries `workspace_id`, and every parent table carries `unique (workspace_id, id)` so children can reference the pair.
- Emails are stored already normalised by `users::normalize_email`. No `citext`, no `lower()` index.
- `contacts` has **no status column**. A workspace-wide opt-out is a `suppressions` row with reason `unsubscribed_all`.

**Roles**
- viewer reads. editor writes contacts, lists, tags, memberships and adds suppressions. admin manages custom field definitions, lifts suppressions and erases contacts. There is no route that lifts a `gdpr_erasure` suppression.

**Frontend**
- The app shell stays under 150 KB gzipped; measured headroom is 45.1 KB. New dependencies load in a route chunk, never the shell.
- Semantic colours are declared with `@theme inline` over `:root` custom properties. Plain `@theme` silently breaks dark mode in Tailwind v4.
- Every route component is `lazyRouteComponent`.
- No webfont and no third-party request at runtime.

## Review Focus

1. **An address suppressed in one Unicode form, mailed in another.** `ops@example.com` typed as NFD or with Turkish dotted İ must hash to the same value as its NFC form, or the blocklist is bypassable. Test: Task 1.
2. **A bulk action catching contacts that arrived after the user clicked.** An import or signup landing between selection and execution must not be included. Test: Task 10.
3. **An erased contact re-created by a later write.** Creating a contact whose address has a `gdpr_erasure` tombstone must be refused, not silently succeed. Test: Task 9.
4. **A custom field deleted, taking ten million values with it.** `DELETE /custom-fields/{id}` must archive and preserve every value; only an explicit purge removes data. Test: Task 2.
5. **A cross-workspace row written directly.** A membership joining workspace A's contact to workspace B's list must fail at the database, not merely at the handler. Test: Task 5.

---

### Task 1: Email normalisation that agrees with itself

**Files:**
- Modify: `src/users/mod.rs` (`normalize_email`)
- Modify: `Cargo.toml` (add `unicode-normalization`)
- Test: `src/users/mod.rs` (unit tests module)

**Interfaces:**
- Produces: `users::normalize_email(raw: &str) -> Result<String, FieldError>`, unchanged signature, now NFC-normalising and lowercasing deterministically.

- [ ] **Step 1: Write the failing tests** in the existing `mod tests` in `src/users/mod.rs`:

```rust
#[test]
fn normalises_unicode_forms_to_one_string() {
    // U+00E9 vs U+0065 U+0301 — the same address typed two ways.
    assert_eq!(normalize_email("josé@example.com").unwrap(),
               normalize_email("jose\u{0301}@example.com").unwrap());
}

#[test]
fn lowercasing_is_deterministic_for_dotted_i() {
    // Must not depend on locale, and must round-trip through the same function.
    let once = normalize_email("İnfo@example.com").unwrap();
    assert_eq!(normalize_email(&once).unwrap(), once);
}

#[test]
fn still_rejects_what_it_rejected_before() {
    for bad in ["", "a@b", "no-at-sign.com", "a@@b.com", "a b@example.com"] {
        assert!(normalize_email(bad).is_err(), "{bad} should be rejected");
    }
}
```

- [ ] **Step 2: Run them and verify the first two fail**

Run: `bash scripts/dev.sh cargo test users:: -- --test-threads=4`
Expected: FAIL — the two normalisation assertions; `still_rejects_what_it_rejected_before` passes already.

- [ ] **Step 3: Implement** — add `unicode-normalization` to `Cargo.toml` and apply NFC **before** lowercasing and before the length and character checks, so the length limit counts normalised characters. Keep every existing rejection rule, and keep the doc comment accurate.

- [ ] **Step 4: Run the full user and auth suites**

Run: `bash scripts/dev.sh cargo test users:: auth:: -- --test-threads=4`
Expected: PASS, including every existing M0 test.

- [ ] **Step 5: Verify the licence gate still passes**

Run: `bash scripts/dev.sh cargo deny check`
Expected: `advisories ok`, `bans ok`, `licenses ok`, `sources ok`.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/users/mod.rs
git commit -m "fix: normalise emails to NFC before lowercasing"
```

---

### Task 2: Custom field definitions

**Files:**
- Create: `migrations/0010_custom_fields.sql`, `src/custom_fields/mod.rs`, `src/custom_fields/routes.rs`
- Modify: `src/lib.rs` (declare the module), `src/http/mod.rs` (merge the router), `src/audit/mod.rs` (action constants), `src/openapi.rs` (register paths and schemas)
- Test: `tests/custom_fields.rs`

**Interfaces:**
- Produces:
  - `custom_fields::FieldKind` — `Text | Number | Date | DateTime | Boolean | Select | MultiSelect`, serialised in snake_case, stored as `text`.
  - `custom_fields::Definition { id: Uuid, key: String, label: String, kind: FieldKind, options: Vec<String>, archived_at: Option<DateTime<Utc>> }`
  - `custom_fields::definitions_for(db, workspace_id) -> sqlx::Result<Vec<Definition>>` — used by Task 3 to validate attributes.
  - `custom_fields::validate_key(raw: &str) -> Result<String, FieldError>` — `^[a-z][a-z0-9_]{0,62}$`.

- [ ] **Step 1: Write the migration** `migrations/0010_custom_fields.sql`, creating the `custom_fields` table exactly as in spec §6.2, including `unique (workspace_id, key)` and `unique (workspace_id, id)`.

- [ ] **Step 2: Write the failing tests** in `tests/custom_fields.rs`:

```rust
// Names only; assertions as described.
create_rejects_a_bad_key                  // "Not A Key" -> 422 on field `key`
create_rejects_select_without_options     // kind=select, options=[] -> 422 on `options`
create_is_admin_only                      // editor -> 403 insufficient-role
duplicate_key_is_conflict                 // same key twice -> 409 key-taken
delete_archives_and_keeps_values          // after DELETE, the contact's value is still readable
purge_refuses_unless_archived             // purge on a live field -> 409 field-not-archived
patch_rejects_a_kind_change               // changing kind -> 422 naming the M1b deferral
list_is_cursor_paginated                  // {data, next_cursor}
cross_workspace_get_is_404                // another workspace's field id -> 404, never 403
```

`delete_archives_and_keeps_values` cannot be written until Task 3 exists; write it in Task 3 and leave a `// covered in tests/contacts.rs` comment here.

- [ ] **Step 3: Run and verify they fail**

Run: `bash scripts/dev.sh cargo test --test custom_fields -- --test-threads=4`
Expected: FAIL — the module does not exist.

- [ ] **Step 4: Implement** `src/custom_fields/mod.rs` and `routes.rs` with handlers `list_custom_fields`, `create_custom_field`, `update_custom_field`, `archive_custom_field`, `purge_custom_field`, mounted at `/api/v1/workspaces/{workspace_id}/custom-fields`. `archive_custom_field` sets `archived_at`; `purge_custom_field` strips the key from every contact's `attributes` and returns the affected count. A `kind` change returns `422` with the detail `changing a field's type is not supported yet`.

- [ ] **Step 5: Run the tests**

Run: `bash scripts/dev.sh cargo test --test custom_fields -- --test-threads=4`
Expected: PASS.

- [ ] **Step 6: Regenerate the API artefacts and commit**

```bash
bash scripts/dev.sh pnpm -C web gen:api
bash scripts/dev.sh cargo sqlx prepare -- --all-targets
git add migrations src/custom_fields src/lib.rs src/http/mod.rs src/audit/mod.rs src/openapi.rs tests/custom_fields.rs web/src/api/openapi.json web/src/api/schema.d.ts .sqlx
git commit -m "feat: custom field definitions, archived rather than destroyed"
```

---

### Task 3: Contacts

**Files:**
- Create: `migrations/0011_contacts.sql`, `src/contacts/mod.rs`, `src/contacts/routes.rs`
- Modify: `src/lib.rs`, `src/http/mod.rs`, `src/audit/mod.rs`, `src/openapi.rs`
- Test: `tests/contacts.rs`

**Interfaces:**
- Consumes: `custom_fields::definitions_for`, `custom_fields::FieldKind`, `users::normalize_email`.
- Produces:
  - `contacts::Contact { id, email, first_name, last_name, attributes: serde_json::Value, timezone: Option<String>, created_at, updated_at }`
  - `contacts::validate_attributes(defs: &[Definition], attrs: &Value) -> Result<Value, Vec<FieldError>>` — rejects unknown keys, wrong types and values for archived fields; returns the canonicalised object.
  - `contacts::create(db, workspace_id, NewContact) -> ApiResult<Uuid>`

- [ ] **Step 1: Write the migration** `migrations/0011_contacts.sql` — `create extension if not exists pg_trgm;` then the `contacts` table and its four indexes exactly as in spec §6.3, including the generated `email_domain` column.

- [ ] **Step 2: Write the failing tests** in `tests/contacts.rs`:

```rust
create_normalises_the_address             // " Ops@Example.COM " stored as "ops@example.com"
duplicate_address_is_conflict             // 409 email-taken, same workspace
same_address_in_two_workspaces_is_fine    // both create successfully
rejects_unknown_attribute_key             // 422 naming the key
rejects_wrong_type_for_kind               // number field given "abc" -> 422
rejects_value_for_archived_field          // 422
accepts_every_field_kind                  // one value per FieldKind, round-tripped
delete_archives_and_keeps_values          // the Task 2 test, written here
viewer_cannot_create                      // 403 insufficient-role
cross_workspace_get_is_404
```

- [ ] **Step 3: Run and verify they fail**

Run: `bash scripts/dev.sh cargo test --test contacts -- --test-threads=4`
Expected: FAIL — the module does not exist.

- [ ] **Step 4: Implement** the module and handlers `get_contact`, `create_contact`, `update_contact`, `delete_contact` at `/api/v1/workspaces/{workspace_id}/contacts`. `update_contact` uses `#[serde(deny_unknown_fields)]` and merges `attributes` key-by-key rather than replacing the object, so a partial update does not erase unrelated fields. `delete_contact` is a hard delete and writes no suppression — erasure is Task 9.

- [ ] **Step 5: Run the tests**

Run: `bash scripts/dev.sh cargo test --test contacts --test custom_fields -- --test-threads=4`
Expected: PASS.

- [ ] **Step 6: Regenerate artefacts and commit** (same six commands as Task 2, paths adjusted)

```bash
git commit -m "feat: contacts with validated typed attributes"
```

---

### Task 4: Sorted pagination and contact search

**Files:**
- Create: `src/pagination/sorted.rs` (and convert `src/pagination.rs` into `src/pagination/mod.rs`)
- Modify: `src/contacts/routes.rs`
- Test: `src/pagination/sorted.rs` (unit tests), `tests/contacts_list.rs`

**Interfaces:**
- Produces:
  - `pagination::sorted::SortedPageParams { limit: Option<u32>, cursor: Option<String>, sort: Option<ContactSort>, dir: Option<Dir>, q: Option<String> }`
  - `SortedPageParams::after(&self) -> ApiResult<Option<(String, Uuid)>>` — the decoded composite keyset.
  - `pagination::sorted::encode_cursor(sort_value: &str, id: Uuid) -> String`
  - `contacts::ContactFilter { q: Option<String>, list_id: Option<Uuid>, tag_id: Option<Uuid> }` — the
    query half of `SortedPageParams`, extracted as its own type because Task 10 embeds it in a
    selection token. It must be `Serialize + Deserialize`, and its serialised form is part of that
    token's signed payload.
- M0's `PageParams` is untouched and every existing endpoint keeps using it.

- [ ] **Step 1: Write the failing unit tests** in `src/pagination/sorted.rs`:

```rust
cursor_round_trips_value_and_id
garbage_cursor_is_validation_error        // ApiError::Validation on field `cursor`
limit_defaults_to_50_and_clamps_to_200
unknown_sort_key_is_validation_error
```

- [ ] **Step 2: Write the failing integration tests** in `tests/contacts_list.rs`. Seed contacts whose email order, creation order and reverse-creation order are three **different** sequences, so a test cannot pass by accident:

```rust
sorts_by_email_ascending_across_pages     // paginate fully; assert the exact concatenated order
sorts_by_created_descending_by_default
search_matches_email_substring            // q="oe" finds "zoe@…"
search_matches_name                       // q="zul" finds last_name "Zulu"
search_below_three_characters_is_422
paging_never_repeats_or_skips_a_row       // walk every page; assert the id set equals the seeded set
```

`ContactFilter` is defined here with all three fields, but only `q` is wired to a query in this
task: `lists` and `tags` do not exist yet. Tasks 5 and 6 wire `list_id` and `tag_id` and add
`filters_by_list_id` / `filters_by_tag_id` to `tests/contacts_list.rs`. Leave a comment in the
filter's doc block saying so, so the unused fields do not read as an oversight.

- [ ] **Step 3: Run and verify they fail**

Run: `bash scripts/dev.sh cargo test --test contacts_list pagination:: -- --test-threads=4`
Expected: FAIL.

- [ ] **Step 4: Implement** `SortedPageParams` and wire `list_contacts` to it. The keyset predicate is `(sort_value, id) < ($1, $2)` for descending and `>` for ascending, matching the index column order in `contacts_keyset_email`. Search uses `ilike '%' || $q || '%'` against the trigram-indexed expression, with a minimum of three characters (shorter terms cannot use a trigram index and would scan the table).

- [ ] **Step 5: Verify no `OFFSET` reached the codebase**

Run: `bash scripts/dev.sh cargo test --test contacts_list pagination:: -- --test-threads=4` then `grep -rin "offset" src/contacts src/pagination`
Expected: tests PASS; grep returns nothing.

- [ ] **Step 6: Regenerate artefacts and commit**

```bash
git commit -m "feat: keyset pagination with a sort key, and contact search"
```

---

### Task 5: Lists and memberships

**Files:**
- Create: `migrations/0012_lists.sql`, `src/lists/mod.rs`, `src/lists/routes.rs`
- Modify: `src/lib.rs`, `src/http/mod.rs`, `src/audit/mod.rs`, `src/openapi.rs`
- Test: `tests/lists.rs`

**Interfaces:**
- Produces:
  - `lists::List { id, name, description, double_optin: bool, visibility: Visibility, created_at, updated_at }`
  - `lists::MembershipStatus` — `Pending | Subscribed | Unsubscribed`
  - `lists::add_member(db, workspace_id, list_id, contact_id) -> ApiResult<MembershipStatus>` — returns `Subscribed` when the list is single opt-in, `Pending` when it is double opt-in.

- [ ] **Step 1: Write the migration** `migrations/0012_lists.sql` — `lists`, `list_memberships` and the membership index exactly as in spec §6.4, with **both** composite foreign keys.

- [ ] **Step 2: Write the failing tests** in `tests/lists.rs`:

```rust
single_optin_add_is_subscribed_immediately
double_optin_add_is_pending
pending_is_never_counted_as_mailable       // asserted again in Task 8
unsubscribe_sets_status_and_timestamp
resubscribe_after_unsubscribe_is_allowed
duplicate_list_name_is_conflict
cross_workspace_membership_is_rejected_by_the_database
cross_workspace_list_get_is_404
```

Also wire `ContactFilter::list_id` and add to `tests/contacts_list.rs`:

```rust
filters_by_list_id                         // only that list's members, still keyset-paginated
```

`cross_workspace_membership_is_rejected_by_the_database` must bypass the handlers and issue the `insert` with raw sqlx, asserting a foreign-key violation. A test that goes through the API proves only that the handler checks — the point of the composite key is that the database refuses.

- [ ] **Step 3: Run and verify they fail**

Run: `bash scripts/dev.sh cargo test --test lists -- --test-threads=4`
Expected: FAIL.

- [ ] **Step 4: Implement** the module and handlers `list_lists`, `create_list`, `get_list`, `update_list`, `delete_list`, `list_list_members`, `add_list_member`, `remove_list_member`. Removing a member sets `unsubscribed` and stamps `unsubscribed_at`; it does not delete the row, because the fact that they unsubscribed is the thing worth keeping.

- [ ] **Step 5: Run the tests**

Run: `bash scripts/dev.sh cargo test --test lists -- --test-threads=4`
Expected: PASS.

- [ ] **Step 6: Regenerate artefacts and commit**

```bash
git commit -m "feat: lists and memberships, with pending only on double opt-in"
```

---

### Task 6: Tags

**Files:**
- Create: `migrations/0013_tags.sql`, `src/tags/mod.rs`, `src/tags/routes.rs`
- Modify: `src/lib.rs`, `src/http/mod.rs`, `src/audit/mod.rs`, `src/openapi.rs`
- Test: `tests/tags.rs`

**Interfaces:**
- Produces: `tags::Tag { id, name, created_at }`; `tags::apply(db, workspace_id, contact_id, tag_id) -> ApiResult<()>`, idempotent.

- [ ] **Step 1: Write the migration** — `tags` with `unique (workspace_id, name)` and `unique (workspace_id, id)`; `contact_tags` keyed `(contact_id, tag_id)` with both composite foreign keys.

- [ ] **Step 2: Write the failing tests** in `tests/tags.rs`:

```rust
applying_a_tag_twice_is_idempotent         // second PUT -> 204, one row
deleting_a_tag_removes_it_from_contacts
duplicate_tag_name_is_conflict
cross_workspace_tag_apply_is_404
viewer_cannot_apply_a_tag
```

Also wire `ContactFilter::tag_id` and add to `tests/contacts_list.rs`:

```rust
filters_by_tag_id
```

- [ ] **Step 3: Run and verify they fail**

Run: `bash scripts/dev.sh cargo test --test tags -- --test-threads=4`
Expected: FAIL.

- [ ] **Step 4: Implement** handlers `list_tags`, `create_tag`, `update_tag`, `delete_tag`, `apply_tag`, `remove_tag`. `apply_tag` is `PUT /contacts/{id}/tags/{tag_id}` and uses `on conflict do nothing`.

- [ ] **Step 5: Run the tests**

Run: `bash scripts/dev.sh cargo test --test tags -- --test-threads=4`
Expected: PASS.

- [ ] **Step 6: Regenerate artefacts and commit**

```bash
git commit -m "feat: tags"
```

---

### Task 7: Suppressions

**Files:**
- Create: `migrations/0014_suppressions.sql`, `src/suppressions/mod.rs`, `src/suppressions/routes.rs`
- Modify: `src/crypto/mod.rs` (add the HMAC helper), `src/lib.rs`, `src/http/mod.rs`, `src/audit/mod.rs`, `src/openapi.rs`
- Test: `tests/suppressions.rs`

**Interfaces:**
- Produces:
  - `crypto::suppression_hmac(secrets: &Secrets, normalised_email: &str) -> [u8; 32]` — HMAC-SHA256 under a subkey derived from the instance secret with the fixed domain-separation string `nm:suppression:v1`. **That string is a one-way door: changing it invalidates every stored hash.** It is a `const` with a comment saying so.
  - `suppressions::Reason` — `UnsubscribedAll | Bounced | Complained | Manual | GdprErasure`
  - `suppressions::is_suppressed(db, secrets, workspace_id, email) -> sqlx::Result<bool>` — the single-address path; checks workspace and instance scope, address and domain, and **sees erasure tombstones**.

- [ ] **Step 1: Write the migration** `migrations/0014_suppressions.sql` — the table, both check constraints and all four indexes exactly as in spec §6.5.

- [ ] **Step 2: Write the failing tests** in `tests/suppressions.rs`:

```rust
hmac_is_stable_across_unicode_forms        // NFD input matches the NFC row (depends on Task 1)
address_scope_blocks_that_address
domain_scope_blocks_every_address_on_it
instance_scope_blocks_in_every_workspace
lifting_unblocks_and_keeps_the_row         // lifted_at set; row still present
suppressing_again_after_a_lift_is_allowed  // the partial unique index permits it
editor_can_add_but_not_lift                // POST 201, lift 403 insufficient-role
lift_is_audited_with_the_original_reason
domain_row_with_an_email_is_rejected       // the check constraint, via raw sqlx
cross_workspace_suppression_get_is_404
```

- [ ] **Step 3: Run and verify they fail**

Run: `bash scripts/dev.sh cargo test --test suppressions -- --test-threads=4`
Expected: FAIL.

- [ ] **Step 4: Implement** `crypto::suppression_hmac`, the module, and handlers `list_suppressions`, `create_suppression`, `lift_suppression`. Instance-wide rows (`workspace_id is null`) are created and lifted only through the admin routes and require `is_instance_admin`. Every insert computes and stores the HMAC, whether or not the plaintext is kept.

- [ ] **Step 5: Confirm no secret reaches a response or a log**

Run: `bash scripts/dev.sh cargo test --test suppressions -- --test-threads=4` then `grep -rn "email_hmac" src/suppressions/routes.rs`
Expected: tests PASS; the grep shows no occurrence inside a serialised response type.

- [ ] **Step 6: Regenerate artefacts and commit**

```bash
git commit -m "feat: suppressions, workspace and instance, address and domain"
```

---

### Task 8: The eligibility predicate

**Files:**
- Create: `migrations/0015_mailable_view.sql`
- Modify: `src/lists/routes.rs` (the mailable-count endpoint), `src/contacts/mod.rs` (the create-time check)
- Test: `tests/eligibility.rs`

**Interfaces:**
- Consumes: `suppressions::is_suppressed`.
- Produces: the `mailable_list_members` view; `GET /lists/{id}/mailable-count` returning `{ "count": u64 }`.

- [ ] **Step 1: Write the migration** `migrations/0015_mailable_view.sql` creating `mailable_list_members` exactly as in spec §7 — **four separate `NOT EXISTS` clauses**, not two with `OR`. Copy the comment explaining why; an OR-qualified anti-join cannot use the indexes and degrades to a sequential scan.

- [ ] **Step 2: Write the failing tests** in `tests/eligibility.rs`. The first is table-driven over every `Reason` and both scopes:

```rust
suppressed_contacts_are_never_mailable     // for each Reason x {address, domain} x {workspace, instance}
pending_membership_is_not_mailable
unsubscribed_membership_is_not_mailable
subscribed_and_unsuppressed_is_mailable
lifted_suppression_restores_mailability
mailable_count_matches_the_view            // endpoint vs a direct count over the view
creating_a_contact_checks_the_single_address_path  // suppressed address still creates, is not mailable
```

- [ ] **Step 3: Run and verify they fail**

Run: `bash scripts/dev.sh cargo test --test eligibility -- --test-threads=4`
Expected: FAIL.

- [ ] **Step 4: Implement** the view and `mailable_count`. Adding a suppressed contact to a list **succeeds** and writes the membership; they are simply not eligible. The API never lifts a suppression as a side effect of anything.

- [ ] **Step 5: Run the tests**

Run: `bash scripts/dev.sh cargo test --test eligibility --test lists --test suppressions -- --test-threads=4`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git commit -m "feat: one definition of whether an address may be emailed"
```

---

### Task 9: GDPR erase

**Files:**
- Modify: `src/contacts/routes.rs`, `src/contacts/mod.rs`, `src/audit/mod.rs`
- Test: `tests/erasure.rs`

**Interfaces:**
- Consumes: `crypto::suppression_hmac`, `suppressions::Reason::GdprErasure`.
- Produces: `POST /contacts/{id}/erase`, admin only.

- [ ] **Step 1: Write the failing tests** in `tests/erasure.rs`:

```rust
erase_removes_the_contact_and_its_rows     // contact, memberships, tags all gone
erase_writes_a_tombstone_with_no_plaintext // suppressions.email is null, email_hmac is not
recreating_an_erased_address_is_refused    // 409 address-erased
erase_is_admin_only                        // editor -> 403
no_route_lifts_an_erasure_tombstone        // the lift endpoint returns 403 for this reason
erase_is_audited_without_the_address       // metadata contains no email
```

- [ ] **Step 2: Run and verify they fail**

Run: `bash scripts/dev.sh cargo test --test erasure -- --test-threads=4`
Expected: FAIL.

- [ ] **Step 3: Implement** `erase_contact` as one transaction: delete the contact (cascades to memberships and tags), insert the tombstone, write the audit row. Wire the tombstone check into `contacts::create`, returning `409 address-erased`. The audit metadata records the contact id and nothing identifying.

- [ ] **Step 4: Run the tests**

Run: `bash scripts/dev.sh cargo test --test erasure --test contacts -- --test-threads=4`
Expected: PASS.

- [ ] **Step 5: Regenerate artefacts and commit**

```bash
git commit -m "feat: GDPR erase, leaving a block the address cannot outrun"
```

---

### Task 10: Selection tokens and bulk actions

**Files:**
- Create: `src/contacts/selection.rs`
- Modify: `src/contacts/routes.rs`, `src/audit/mod.rs`, `src/openapi.rs`
- Test: `tests/bulk.rs`

**Interfaces:**
- Consumes: `crypto::tokens` (M0's signed-token helper), `SortedPageParams`.
- Produces:
  - `selection::Selection { workspace_id: Uuid, filter: ContactFilter, as_of: Uuid }`
  - `selection::issue(secrets, &Selection) -> String` and `selection::parse(secrets, &str) -> ApiResult<Selection>` — rejecting a token over one hour old with `422 selection-expired`.
  - `POST /contacts/selection`, and `POST /contacts/bulk/{tag,untag,subscribe,unsubscribe,delete}` taking `{ selection_token, excluded_ids }`.

- [ ] **Step 1: Write the failing tests** in `tests/bulk.rs`:

```rust
contacts_created_after_as_of_are_not_included   // the Review Focus case: seed, issue token, create more, act
excluded_ids_are_not_affected
token_from_another_workspace_is_404
expired_token_is_422
bulk_writes_exactly_one_audit_row               // count the rows, assert 1, assert it carries the affected count
bulk_delete_is_editor_and_above
response_reports_the_count_actually_affected
```

- [ ] **Step 2: Run and verify they fail**

Run: `bash scripts/dev.sh cargo test --test bulk -- --test-threads=4`
Expected: FAIL.

- [ ] **Step 3: Implement** `selection.rs`. `as_of` is a UUIDv7 upper bound, so the snapshot is enforced as `id <= $as_of` — free, because ids are already time-ordered. Each bulk handler resolves the predicate under that bound, applies `excluded_ids`, performs one set-based statement, and writes **one** audit row carrying the action, the filter, the `as_of` value and the affected count.

- [ ] **Step 4: Run the tests**

Run: `bash scripts/dev.sh cargo test --test bulk -- --test-threads=4`
Expected: PASS.

- [ ] **Step 5: Regenerate artefacts and commit**

```bash
git commit -m "feat: bulk actions over a snapshotted selection, not a list of ids"
```

---

### Task 11: Design tokens and primitives

**Files:**
- Create: `web/src/styles/tokens.css`, `web/src/components/{button,input,select,checkbox,dialog,field,badge,empty-state,error-state,skeleton}.tsx`
- Modify: `web/src/index.css`, `web/index.html` (the pre-paint theme script), `web/src/lib/ui.ts` (re-export, then delete the old constants)
- Test: `web/src/components/*.test.tsx` (node:test + testing-library), `web/e2e/theme.spec.ts`

**Interfaces:**
- Produces the component set every later UI task imports. Props follow native element props plus `variant` where the research specifies one.

Read `.scratch/email-platform/research/m1a-ui-design-system.md` first: it carries the full OKLCH scales, light and dark, each pair contrast-verified. Use those values verbatim.

- [ ] **Step 1: Write the failing tests**

```ts
// web/src/components/dialog.test.tsx
closes_on_escape
traps_focus_while_open
returns_focus_to_the_trigger_on_close
// web/e2e/theme.spec.ts
dark_mode_applies_before_first_paint   // no flash: assert computed background on first frame
both_themes_pass_axe_on_a_sample_page
```

- [ ] **Step 2: Run and verify they fail**

Run: `pnpm -C web test` and `pnpm -C web e2e theme`
Expected: FAIL.

- [ ] **Step 3: Implement** the tokens in `tokens.css`, declared through **`@theme inline`** over `:root` custom properties — with plain `@theme`, Tailwind v4 silently loses dark mode. The theme is a `data-theme` attribute set by an inline script in `index.html` before first paint; do not also use `prefers-color-scheme` with a class, because two mechanisms disagree. Build the dialog on native `<dialog showModal()>`, which supplies the focus trap, `inert`, Escape handling and top-layer stacking for free; build checkbox and select on the native elements.

- [ ] **Step 4: Run the tests and the bundle gate**

Run: `pnpm -C web test`, `pnpm -C web e2e theme`, `pnpm -C web build && node web/scripts/check-bundle.mjs`
Expected: PASS, and the shell still under 150 KB gzipped.

- [ ] **Step 5: Commit**

```bash
git commit -m "feat(web): OKLCH token layer and primitives on native elements"
```

---

### Task 12: The contacts table

**Files:**
- Create: `web/src/routes/contacts.tsx`, `web/src/components/data-grid.tsx`
- Modify: `web/src/router.tsx`, `web/package.json` (add `@tanstack/react-virtual@3.14.26`)
- Test: `web/e2e/contacts.spec.ts`

**Interfaces:**
- Consumes: Task 11's components; `GET /contacts` with `SortedPageParams`; `POST /contacts/selection`; the bulk endpoints.

- [ ] **Step 1: Write the failing e2e tests** in `web/e2e/contacts.spec.ts`:

```ts
renders_and_scrolls_past_the_first_page
sorting_by_email_reorders_the_rows
search_filters_the_rows
select_all_matching_then_exclude_one_bulk_tags_the_rest
grid_is_one_tab_stop_and_arrow_keys_move_the_row     // roving tabindex
aria_rowindex_is_set_on_every_rendered_row
axe_reports_no_violations
```

- [ ] **Step 2: Run and verify they fail**

Run: `pnpm -C web e2e contacts`
Expected: FAIL — the route does not exist.

- [ ] **Step 3: Implement** the route and `data-grid.tsx`. It is a real `<table>` with two spacer rows, so sticky `<thead>` and table semantics survive virtualisation. Data comes from `useInfiniteQuery` with `maxPages: 20`, which bounds the browser at 2,000 rows however far someone scrolls. Accessibility: `role="grid"`, row-level roving tabindex giving the whole grid one tab stop, `aria-rowindex` on every rendered row, and `aria-rowcount="-1"` whenever the total is an estimate rather than a real count — otherwise a screen reader announces "row 3 of 30" over a million results. `@tanstack/react-virtual` is imported here, in this route's chunk, never in the shell.

- [ ] **Step 4: Run the tests and both bundle gates**

Run: `pnpm -C web e2e contacts`, then `pnpm -C web build && node web/scripts/check-bundle.mjs`
Expected: PASS; shell under 150 KB gzipped and the contacts chunk under 50 KB gzipped.

- [ ] **Step 5: Commit**

```bash
git commit -m "feat(web): contacts grid, virtualised over keyset pages"
```

---

### Task 13: Contact detail and lists UI

**Files:**
- Create: `web/src/routes/contact-detail.tsx`, `web/src/routes/lists.tsx`
- Modify: `web/src/router.tsx`
- Test: `web/e2e/contact-detail.spec.ts`

- [ ] **Step 1: Write the failing e2e tests**

```ts
shows_typed_custom_field_values
shows_list_memberships_with_status
explains_why_a_suppressed_contact_is_not_mailable   // names the suppression and its reason
list_detail_shows_a_mailable_count_after_a_skeleton
axe_reports_no_violations_on_both_pages
```

- [ ] **Step 2: Run and verify they fail**

Run: `pnpm -C web e2e contact-detail`
Expected: FAIL.

- [ ] **Step 3: Implement** both routes. The mailable count is fetched in its own query with a skeleton, because the endpoint is deliberately exempt from the 150 ms target and must not block the page. Do not denormalise it into a counter: Interspire's hand-maintained counters drift, which is one of the things being replaced.

- [ ] **Step 4: Run the tests**

Run: `pnpm -C web e2e contact-detail`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git commit -m "feat(web): contact detail and lists"
```

---

### Task 14: Tags, custom fields and suppressions UI

**Files:**
- Create: `web/src/routes/tags.tsx`, `web/src/routes/custom-fields.tsx`, `web/src/routes/suppressions.tsx`
- Modify: `web/src/router.tsx`, `web/src/routes/workspace-layout.tsx` (navigation)
- Test: `web/e2e/audience-admin.spec.ts`

- [ ] **Step 1: Write the failing e2e tests**

```ts
creating_a_field_offers_every_kind
deleting_a_field_warns_that_values_are_kept        // the copy must say archived, not deleted
purging_a_field_requires_it_to_be_archived_first
adding_a_suppression_then_lifting_it_as_admin
an_editor_sees_no_lift_control
axe_reports_no_violations_on_all_three_pages
```

- [ ] **Step 2: Run and verify they fail**

Run: `pnpm -C web e2e audience-admin`
Expected: FAIL.

- [ ] **Step 3: Implement** the three routes and add them to the workspace navigation. Lift is the most dangerous control in the milestone — it re-enables mailing someone who opted out, hard-bounced or complained — so it is admin-only, is not rendered for an editor, and its confirmation names the address and the original reason.

- [ ] **Step 4: Run the tests**

Run: `pnpm -C web e2e audience-admin`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git commit -m "feat(web): tags, custom fields and suppressions"
```

---

### Task 15: CI gates, the glossary, and the decision record

**Files:**
- Create: `GLOSSARY.md`, `docs/adr/0001-contacts-have-no-status-column.md`, `docs/adr/0002-suppression-hashing.md`, `docs/adr/0003-selection-is-a-snapshotted-predicate.md`
- Modify: `web/scripts/check-bundle.mjs`, `.github/workflows/ci.yml`, `README.md`
- Test: `web/scripts/check-bundle.test.mjs`

AGENTS.md mandates a root `GLOSSARY.md` and `docs/adr/`; neither exists yet, and M1a is where the domain vocabulary arrives. The glossary's entries are spec §3, verbatim.

- [ ] **Step 1: Write the failing test** for the new bundle gate in `web/scripts/check-bundle.test.mjs`:

```js
fails_when_a_lazy_route_chunk_exceeds_50kb_gzipped
still_fails_when_the_shell_exceeds_150kb_gzipped
reports_which_chunk_broke_the_budget
```

- [ ] **Step 2: Run and verify it fails**

Run: `node --test web/scripts/check-bundle.test.mjs`
Expected: FAIL — the per-chunk ceiling does not exist.

- [ ] **Step 3: Implement** the per-chunk ceiling in `check-bundle.mjs`, add an INP assertion on the contacts route to the e2e suite against the 200 ms target, and write `GLOSSARY.md` and the three ADRs. Each ADR states the decision, the alternative rejected, and what it costs if wrong.

- [ ] **Step 4: The worst-case-data pass** (spec §14 requires it before sign-off; the UI research was never visually verified). Create `web/e2e/worst-case-data.spec.ts` rendering every new screen against data designed to break layout, and assert no horizontal page scroll, no clipped text and no axe violation in either theme:

```ts
zero_rows_shows_the_empty_state_not_an_empty_grid
one_row_does_not_collapse_the_virtualiser
a_254_character_email_does_not_overflow_the_row
a_name_of_only_combining_marks_renders
rtl_name_does_not_break_column_alignment
a_multi_select_field_with_40_values_wraps
a_list_name_at_the_length_limit_does_not_push_the_count_offscreen
the_longest_suppression_reason_fits_its_badge
```

- [ ] **Step 5: Run the whole suite**

Run: `bash scripts/dev.sh cargo test -- --test-threads=4`, `bash scripts/dev.sh cargo clippy --all-targets -- -D warnings`, `bash scripts/dev.sh cargo fmt --check`, `bash scripts/dev.sh cargo deny check`, `pnpm -C web test`, `pnpm -C web e2e`, `node --test web/scripts/*.test.mjs`
Expected: all PASS.

- [ ] **Step 6: Verify no stray test databases were left behind**

Run: `docker exec nm-postgres-1 psql -U postgres -tAc "select count(*) from pg_database where datname like 'nm_e2e_%'"`
Expected: `0`.

- [ ] **Step 7: Commit**

```bash
git commit -m "ci: per-chunk budget; docs: glossary and the M1a decision records"
```

---

### Task 16: The contact and list write paths

Added after the whole-branch review found the UI ships read-only: Tasks 12-14 each passed their
own brief, and this plan never assigned create, edit, delete, erase or membership management to
any of them. A fresh install cannot add a contact or honour an erasure request through the web
app. The API for all of it already exists and is tested.

**Files:** `web/src/routes/contacts.tsx`, `contact-detail.tsx`, `lists.tsx`; new dialogs as needed
in `web/src/components/`. Test: `web/e2e/contacts-crud.spec.ts`.

- [ ] **Step 1: Write the failing e2e**, which spec §14 requires and which could not be written
      before because nothing existed to drive:

```ts
create_a_contact_then_find_it_in_the_grid
edit_a_contact_and_see_the_change
delete_a_contact_and_it_leaves_the_grid
erase_a_contact_is_admin_only_and_names_what_it_destroys
an_editor_sees_no_erase_control
create_a_list_then_add_a_contact_to_it
remove_a_membership_leaves_it_unsubscribed_not_deleted
apply_and_remove_a_tag_on_one_contact
axe_reports_no_violations_with_each_dialog_open
```

- [ ] **Step 2: Run them and watch them fail.**
- [ ] **Step 3: Implement**, reusing `ConfirmDialog`, `Dialog` and the existing primitives.
      Erase is admin-only and its confirmation says the contact is deleted and the address
      permanently blocked, matching the suppression lift dialog's standard. Removing a membership
      sets `unsubscribed`; say that rather than "remove".
- [ ] **Step 4: Rebuild web AND release, run the e2e, report both bundle numbers.**
- [ ] **Step 5: Commit.**

### Task 17: The remaining bulk actions and the instance suppression screen

**Files:** `web/src/routes/contacts.tsx`, new `web/src/routes/admin-suppressions.tsx`,
`web/src/router.tsx`. Test: extend `web/e2e/audience-admin.spec.ts`.

- [ ] **Step 1: Write the failing tests:**

```ts
bulk_untag_over_a_selection
bulk_unsubscribe_names_the_list_and_reports_affected_of_selected
bulk_delete_requires_confirmation_naming_the_count
the_instance_suppression_screen_is_instance_admin_only
a_workspace_admin_cannot_reach_it
```

- [ ] **Step 2: Run them and watch them fail.**
- [ ] **Step 3: Implement.** `bulk/delete` is irreversible: its confirmation names the count and
      says no suppression is written, so the addresses stay usable. The instance screen drives the
      existing `/api/v1/admin/suppressions` routes and is gated on `is_instance_admin`.
- [ ] **Step 4: Rebuild web AND release, run the e2e, report bundle numbers.**
- [ ] **Step 5: Commit.**
