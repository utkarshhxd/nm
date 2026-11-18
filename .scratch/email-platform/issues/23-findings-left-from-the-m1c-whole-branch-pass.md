Status: needs-triage

# Findings the M1c whole-branch pass recorded rather than fixed

Four reviewers covered disjoint slices of `m1c-import` — backend and schema, web UI, API contract,
tests and plans. Everything Critical and Important was fixed in the branch's single fix wave and
re-reviewed. These are the remainder: each was argued, ruled on, and deliberately left.

Grouped by why it was left, because that is what a future reader needs.

---

## A. Ruled "file and ship" — the fix is known and the cost is not worth paying now

**A1. `migrations/0013_tags.sql` has no `check` on `name`.** Validation lives at call sites. The
obvious `check (char_length(name) between 1 and 100)` expresses only **half** of
`lists::validate_name` — length, but not control characters — so it would retire no call site
while costing a new migration, a lock and a table scan on a released migration. If this is ever
done, express the whole rule.

**A2. `imports.status` is a `String` at the API boundary.** The database `check` already makes
invalid states unreachable, so a typed enum buys exhaustiveness at one comparison. It is blocked
behind issue 22: a client matching exhaustively today would have to write a `running` arm that
cannot execute.

**A3. `src/contacts/import/staging.rs` is ~1000 lines and should not be split yet.** The review
tested the obvious seam — parse-and-stage versus write — and rejected it. The two halves are
coupled by a contract nothing but adjacency enforces: the staging DDL, `COLUMNS`, `stage_row`'s
positional render, and the upsert's four statements reading those columns by name. The upsert's
`coalesce(nullif(…))` arms are correct **only** because `push_field` writes a SQL NULL for a blank
cell, which a reader of a separate `upsert.rs` could not see. The real weight in the file is the
80-line nine-CTE statement in `write_memberships_and_tags`, which that split does not touch.
If it is split later: move `COLUMNS` with the upsert half, and **add a test pinning the DDL
against the upsert's column references** — that test is what makes the split safe, and it costs
more than the move.

**A4. `problemText` prefers the client's `KNOWN` map over the server's `detail`.** So
`too-many-rows` is either the server's real figure with no advice, or advice carrying a
client-side copy of `1,000,000` that goes stale silently. Splicing the server's `detail` is the
**wrong** fix: only `too-many-columns` and `too-many-rows` have a quotable detail, while
`import-too-large`'s is raw bytes, `rate-limited`'s is prose, and `import-in-progress`'s embeds a
UUID. Two routes are sound — make a `known` value `string | ((detail: string) => string)`
(backward compatible across all 28 call sites, and `too-many-rows` would still need digit
grouping because the server sends `1000000`), or, more sturdily, **add a `limit` extension member
to the problem body** and have the client format it. The second is a contract change and wants its
own ticket.

**A5. `web/src/routes/import-detail.tsx` is ~820 lines.** Split on next touch, `csvCell` first.

**A6. `web/package.json` has no lint script.** The suite runs typecheck, build and e2e only.
Project-wide and pre-existing, not an M1c defect.

---

## B. Contract gaps — real, and each misleads a client

**B1. `NewImport.file` is `format: binary`, which generates `file: string`.** TypeScript clients
cannot type a file upload; the UI works around it with `body: { file: "" }` at
`web/src/routes/imports.tsx:60-62`. The fix is on the Rust side.

**B2. The published descriptions carry internal rationale and omit the caps clients need.**
Rustdoc text leaked into the OpenAPI output (`super::staging::MAX_IMPORT_ERRORS`, "see the
savepoint below"), and M1c's fix for issue 22's part 1 widened this by publishing seven lines of
reasoning on `ImportReport::status`. Meanwhile the limits a client must respect — **10,000 error
rows, 512 MiB, 1,000,000 rows, 1,000 columns** — are never stated anywhere in the contract.

**B3. `start_import` returns 409 before 404.** While this workspace has an import in flight, a
foreign or nonexistent id gets `import-in-progress` rather than the documented 404. **This is not
a tenancy leak** — foreign and nonexistent produce the same answer, so nothing is disclosed — but
the documented behaviour is incomplete.

**B4. The 422 slug `duplicate-mapping-target` is emitted but not documented.**

---

## C. Left from the fix wave itself

**C1. `web/src/routes/imports.tsx:23` says something untrue.** The new `empty-file` copy claims
"a file with only a header row counts as empty". It does not: `empty-file` fires only when the
sniff returns zero columns (`routes.rs:385`), which happens only when `detect` found no records at
all (`sniff.rs:137-143`). A header-only file yields one record and uploads normally. The only
tested trigger is a zero-byte file. **Delete the clause.**

**C2. `staging.rs:484` — the `gone` CTE still does `returning s.row_number, c.email` and nothing
reads `c.email`.** Harmless today, and it is the exact shape of the erasure write-back this
milestone fixed. Drop the column.

**C3. Both new `catch {}` blocks relabel any thrown error as a lost connection.** Correct today;
wrong the day an abort signal or a fetch interceptor is added.

---

## D. Test and harness debt

**D1. The e2e suite leaves roughly a dozen `nm-import-*.csv` files per run, for ever.** The server
is right to keep them — two documented decisions at `sweep_orphaned_uploads` say a `pending`
import's file is kept, and a file with no import row is kept too. **The suite is the thing
creating files it knows are garbage**, and it alone knows which ids were in the database it is
about to drop. The teardown belongs in the harness, not the sweep.

**D2. `web/e2e/worst-case-data.spec.ts:872` and `:882` are named for the contacts grid but sweep
all 19 screens** via `everyScreen(SCREENS)`. That naming is why a mutation check during M1c first
targeted a component those tests never visit, passed, and looked like a harness failure when it
was a no-op mutation. Rename to what they actually cover.

**D3. `another_workspaces_contact_is_never_updated_by_an_import` cannot fail.** `contacts` is
unique on `(workspace_id, email)`, the conflict target is that same pair, and the workspace is a
bound parameter rather than a predicate — there is nothing to mutate. It is kept, with a comment
naming what actually enforces the property. The real pin for that area is
`another_workspaces_contact_fails_no_duplicate_check`, added by the fix wave.

**D4. `a_contact_erased_after_it_was_inserted_is_deleted_before_commit`'s fixture is artificial.**
Its delete reports *collapsed* rows where a real run reports the picked one. It was rewritten when
the new unique index exposed that its old fixture encoded a state no run can reach; the rewrite
respects the invariant but does not reproduce a real run's shape.

---

## E. The `lists` arm stages an empty list name

An entry of `;;` produces a list named `""`, where the neighbouring `tags` arm skips blanks. Not
reached through the UI; inconsistent at the API.

---

## F. Two documentation items

**F1. `deny.toml`'s `rsa` rationale is accurate but two wordings are off**: the crate also arrives
via the dev-dependency `webauthn-authenticator-rs`, and the cited `crypto-glue` line is about six
lines out. The substance — that every live RSA use is signature verification, never a private-key
operation — was verified in the vendored sources during the M1c review.

**F2. `oci-email-postgres` and `marketing-automation` are named in
`.scratch/email-platform/HANDOFF.md:78` and three plan files.** They are named because those
files carry the constraint protecting them, and a rule that cannot name what it protects cannot be
followed — so this is deliberate. It is recorded here only so that **if this repository is ever
made public**, the decision is taken knowingly rather than discovered.
