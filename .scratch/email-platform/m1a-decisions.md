# M1a decision record

Decisions taken while building milestone M1a (contacts, lists, tags, custom fields,
suppressions, the selection and bulk engine, and the web UI for all of it) whose reasoning does
not fit in a code comment and is not recoverable from the diff.

Read `m0-decisions.md` first; everything in it still applies. This file covers what M1a added,
in the same shape: what was decided, why, and what it costs if the decision was wrong.

The architectural decisions proper live in `docs/adr/` — 0001 (contacts have no status column),
0002 (suppression hashing), 0003 (selection is a snapshotted predicate). This file is the
residue: the judgement calls, the deliberate exceptions, and the traps that cost real time.

---

## Correctness decisions you should not casually reverse

### A bulk action is bounded by the count its token was issued with
`src/contacts/selection.rs`, `src/contacts/routes.rs`. ADR 0003 carries the full argument. The
part worth repeating here: the snapshot freezes **creation time only**. `as_of` is a UUIDv7
ceiling, but `push_filter` re-evaluates tags, memberships and name matches against *current*
state, so a contact that existed at `as_of` and did not match when the token was issued is acted
on if it matches when the action runs. A second authorised editor writing a tag inside the hour
is enough.

The token therefore carries `issued`, the match count at issue time, and the comparison is
`selected + excluded_ids.len() > issued`. Both halves of that sum matter: `selected` is counted
*after* the exclusions are applied and `issued` was taken with none, so comparing them directly
let a widening equal to the number of unticked rows slip through — with `MAX_EXCLUDED` at 10,000,
a confirmation reading "Delete 1 contact" could delete 10,000.

The guarantee is `selected <= issued - |excluded|`: **no action touches more contacts than the
confirmation dialog showed**. It is not an identity guarantee. A same-count swap still
substitutes never-selected people, because the check counts and does not identify. That is
issue `04`.

### Delete and subscribe refuse; tag, untag and unsubscribe report
The axis is **creates consent you cannot retroactively obtain**, not irreversible versus
reversible. That framing was wrong first time round. Unsubscribing removes a row but does not
un-send, and being on a list is what makes someone mailable — so a widened subscribe between
selection and campaign puts people on a list they were never selected for. M1a is only safe from
that because nothing sends yet.

`tag` sits on the reporting side **only because nothing mails a tag today**. The moment a
campaign can target a tag or a segment expression, tagging becomes consent-creating by the same
argument and the split must be revisited.

### The widening check runs after the statement and inside the transaction
`src/contacts/routes.rs`. The statement is one CTE — `with sel as (...), done as (<action>
returning 1) select count(sel), count(done)` — so the number compared is the one the action
itself resolved, under one snapshot, not a second count that could disagree. Returning `Err`
drops `tx` before the only `commit()`, so both the rows and the audit entry roll back. Moving the
check before the statement reintroduces a TOCTOU gap; moving it after the commit leaves an audit
row claiming an action that was refused.

### Every request body denies unknown fields, including the signed token payload
This cost three near-misses, each of which would have destroyed every contact in a workspace:

1. `ContactFilter` without `deny_unknown_fields` — a typo'd field name deserialises to an
   all-`None` filter, which matches **every** contact, and `bulk/delete` with that token deletes
   them all.
2. The bulk bodies used `#[serde(flatten)]`, which *cannot* deny unknown fields. A misspelt
   `excluded_ids` was silently ignored and defaulted to `[]`, so the action ran on exactly the
   contacts the user had excluded. The three bodies now spell their fields out, with a doc
   comment saying not to re-flatten them.
3. The `Selection` payload itself accepted unknown top-level keys, so the fail-closed property
   rested entirely on remembering to bump the signing label.

Twelve M0 bodies still ignore unknown fields (issue `11`). On the query-string side nothing
denies them yet (issue `09`).

### Outstanding selection tokens are invalidated by bumping the signing label
`SELECTION_DOMAIN`, `nm:selection:v1` → `v2`. The HMAC check runs before deserialisation, so an
old token never reaches serde. This is the mechanism for any future change to the payload's
meaning: accepting old-shaped tokens for the remainder of a one-hour TTL means accepting tokens
that skip whatever check was just added.

Consequently `issued` has no `serde(default)`. A payload predating the field is a hard
deserialize error, not a defaulted zero — including one this build signed itself.

### `blocking()` is the only suppression predicate, and `is_erased` is deliberately narrower
`src/suppressions/mod.rs`. `blocking()` backs both `is_suppressed` and `blocking_suppression`.
The **create** path checks only `is_erased`, not `is_suppressed`: refusing to create a contact
whose address merely bounced would be wrong. An erasure tombstone is a different thing, and no
route in either scope can lift one.

### Eligibility has exactly one definition
`mailable_list_members` (migration 0015). Four separate `NOT EXISTS` clauses, never two combined
with `OR` — the combined form is a different predicate. Any second implementation of "who can we
mail" is a bug by construction; a grep confirmed there is no third.

---

## Deliberate exceptions, so you do not "fix" them

- **A contact's email address cannot be edited.** `UpdateContact` has no `email` field by design.
  A membership's consent was given by *that address*; moving the row hands one person's consent
  to another and slides them out from under the suppressions held against their own address.
  Delete-and-re-add is correct because it forces fresh consent, and the UI says why rather than
  only saying no.
- **The affected/selected denominator is conditional.** `"4,000 of 5,000"` prints only when the
  numbers differ. `"5 of 5"` is noise that trains people to stop reading the number that matters.
  The gap is the information.
- **Deleting a list cascades its memberships, including the unsubscribed ones.** So the record
  that someone opted out of *that list* cannot be produced afterwards. The confirmation says so
  explicitly, along with the fact that no contact is deleted and that a workspace-wide opt-out is
  a suppression and untouched.
- **The `affected > promised` branch in `describe()` is unreachable and stays.** With the server
  refusing a widened filter selection and an id selection unable to widen by construction,
  nothing reaches it. It is three lines on the one direction that cannot be undone, where the
  fall-through would otherwise render as success. Defence in depth, not covered behaviour.
- **At zero selected the bulk controls disable rather than hide.** The bar still reads
  `"0 of 3 … (3 left out)"`, so a disabled control explains itself; a vanished one raises a
  question instead of answering it.
- **The axe assertion does not fail on `incomplete` results.** A dangling `aria-describedby` does
  pass today. But axe reports `incomplete` for anything it cannot decide without a human, so
  asserting on it produces a flaky check that gets deleted within a month.
- **`TableScroller` carries `tabIndex=0` unconditionally.** Making it conditional needs a
  `ResizeObserver` and overflow measurement for one extra Tab press, and a keyboard-reachable
  scroll region is the safe direction to err.
- **`the_dialogs_hold_up_at_320px_with_a_254_character_address` stays despite overlapping the
  break harness.** It went red under mutation, it runs against a different workspace, and the
  harness itself was vacuous for an afternoon. Overlapping coverage is cheap.

---

## Things that bit us, recorded so they do not bite again

- **`web/dist` is compiled into the binary by rust-embed** (`src/http/web_ui.rs`). After
  `pnpm -C web build` you must `cargo build --release --bin nm`, or the e2e runs the *previous*
  UI. This produced a false negative and a false positive in one session.
- **Never pipe a build to `/dev/null`.** A silent `tsc -b` failure leaves a stale `dist` and the
  next run looks like a result. Worse: `gen:api` writes `openapi.json` through a truncating `>`
  redirect, so a failed `cargo run -- openapi` silently empties a committed file.
- **Playwright runs serial.** When a test fails, the rest of that file is **skipped, not passed**.
  A single run cannot prove a global break reached every test; use `--no-fail-fast` on cargo and
  several runs on Playwright before claiming "only that test failed".
- **`worst-case-data.spec.ts` must run as a whole file.** `-g every_screen` skips its seed test,
  the grid never appears, and the run fails spuriously.
- **`<dialog>` carries a UA `overflow: auto`.** Adding dialogs to the break harness passed *with*
  a layout defect still present, because `layoutProblems` read the overflowing content as sitting
  inside a scroller that could reach it — the same exemption that legitimises `TableScroller`. The
  fix is one rule: a `<dialog>` whose `scrollWidth` exceeds its `clientWidth` is a finding.
- **`let _ = f(...)?;` still propagates.** As a mutation it changes nothing, the suite stays green,
  and it looks exactly like a vacuous assertion. Drop the `?`. Two separate agents lost time here.
- **Query keys collide when one is paginated and another is not.** `["tags", ws]` was shared by an
  infinite query and a plain one that mapped an array; navigating between the screens handed the
  consumer `{pages: [...]}`. Suffix the broader one — `["tags", ws, "all"]` — and prefix
  invalidation still reaches it. `["lists", ws]` had the same shape.
- **Two copies of a helper drift.** `scan()` existed twice and only one copy got the guard that
  makes it fail when axe runs no rules; `REASON` was defined identically in two screens. If a
  thing is worth fixing in one file it is worth extracting.
- **`git checkout -- <file>` restores committed work only.** Using it to revert a mutation on an
  uncommitted feature discards the feature. Snapshot the file first.
- **Nine assertions were found passing against deliberately broken code.** Mutation is the only
  proof an assertion discriminates. Three of the nine were caught by implementers on their own
  work, which is the habit worth keeping.

---

## Known gaps at the end of M1a

Eleven issues are filed under `.scratch/email-platform/issues/`. The ones that shape M1b:

- **`04` — the same-count swap is unbounded.** The widening check counts, it does not identify.
  Nothing makes the substitutable number small: an import retagging a segment could swap all of
  it. The TTL bounds how long the window is open, not how much moves inside it. **Blocking for
  M1b's importer**, which is what makes this race routine rather than exotic.
- **`08` — "remove" names an operation that unsubscribes.** `DELETE .../lists/{list_id}`,
  `remove_list_member`, `remove_member`, `LIST_MEMBER_REMOVED`. The row is kept and set to
  `unsubscribed`. **Blocking for M1b:** free to rename now, expensive once any external client
  exists.
- **`03` — the confirmed count never reaches the server.** The number the dialog promises is
  browser-side only, so the server bounds by `issued` alone and a replayed token with fewer
  exclusions legitimately exceeds what the dialog said.
- **The grid query does not apply `id <= as_of`**, so a contact created after the token can be
  drawn as a ticked row; unticking it excludes an id the match never held and produces a
  spurious `409`. Over-refusal, never unsafe, and uncovered by any test.
- **No load testing.** Issues `01` and `02` (contact search cost at scale, erasure leaving the
  instance block in plaintext) remain open and untriaged. Nothing has been run at 10M rows and
  the eligibility view has not had an `EXPLAIN` re-run since the indexes landed.
