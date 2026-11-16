Status: resolved

# The count a person confirmed never reaches the server

Found in M1a task 17 review. This is a design gap, **not a defect**: nothing is broken today and
no test is wrong. It is recorded so the next person does not assume the server enforces what the
dialog said.

## What the server can and cannot enforce

A bulk action is bounded by `issued`, the live count signed into the selection token
(`src/contacts/selection.rs`). `bulk/delete` and `bulk/subscribe` refuse when the predicate
resolves to more than that. That is the only number the server has.

The number the person actually agreed to is a different one. The browser computes `promised` from
the selection bar -- matched, less the rows they unticked -- renders it in the confirmation
("Delete 1 contact") and compares it against `affected` when the result lands
(`web/src/routes/contacts.tsx`, `describe`). It is never sent.

So a request carrying the same token with fewer `excluded_ids` legitimately acts on more contacts
than any dialog said, and the server cannot tell: the action is within `issued`, which is what the
token stands for. Only the browser that issued the request notices, through the `short < 0`
branch, and only after the fact.

## Why it is not urgent

The caller is an authorised editor either way -- a token is not a privilege escalation, and
`begin_bulk` re-authorises on every call. The realistic failure is not an attacker but a UI bug
that under-reports the count in the dialog, or a third-party client that builds the body by hand
and gets the exclusions wrong. Neither is caught anywhere.

## Candidate fix

Carry the confirmed count in the bulk body -- `expect_at_most`, say -- and have the server refuse
when the resolved `selected` exceeds it. That makes the dialog's number a contract rather than a
decoration, and it composes with the existing `issued` check rather than replacing it: `issued`
bounds the token, `expect_at_most` bounds the request.

It is an API change -- a new field on three `deny_unknown_fields` bodies, the UI sending it, and a
decision about what an API client that omits it should get. Do it deliberately, in its own task,
not as a drive-by.

## Comments

**2026-11-14 — resolved in M1b, essentially as the candidate fix describes.** Verified against the
code and the tests, not against the spec.

The field is `confirmed_count`, on all five bulk bodies (`BulkTarget`, `BulkTag` x2, `BulkList`
x2 — `src/contacts/routes.rs:592,607,635`). The server refuses with `409 selection-count-mismatch`
when the action reaches more than that number, inside the transaction, so nothing is written:
`refuse_if_more_than_confirmed` (`src/contacts/routes.rs:897`), called from the top of
`finish_bulk` so every bulk route passes through it and a sixth cannot be written that forgets it.
The decision this issue asked for -- what an API client that omits it gets -- is answered: with a
`selection_token` it is **required**, and omitting it is `422` (`routes.rs:805-810`), "because a
client that may omit it is a client that is not checked"; with an explicit `ids` list it is
**refused**, since the ids are the number and a count nothing can check is a count nobody should
send (`routes.rs:825-830`).

The part worth knowing, and §9.2 of the M1b spec now says it too: the comparison is
against **what the action reached** (`counts.selected`), not against `selection_snapshots.size`.
Comparing against the size compares the client's echo with the token's own signed `issued`, a
number the server already had, which a replayed token with fewer exclusions satisfies -- i.e. this
issue renamed rather than closed. The number that matters is the one on the button: the snapshot's
size less the rows the person unticked. That was caught in Task 6's review and fixed in `3ce6470`.
When this comment was written the spec still described the rejected design; §9.2 was corrected in
`142372f`, the commit straight after the one that wrote this comment, and
`specs/2026-11-12-m1b-segments.md:522` now opens with the sentence above.

The comparison is **one-sided** on purpose. Fewer than confirmed is not refused: a contact somebody
else deleted in between is simply absent, the `issued`/`selected`/`affected` gap reports it, and
refusing would turn their delete into this person's error.

- Commits: `8ed2389` (the snapshot design this depended on), **`3ce6470`** (the close -- "fix:
  compare the confirmed count with what the action reaches"), `124be23`/`1e881d5` (the client).
- Rust test: `an_action_reaching_more_than_the_dialog_said_is_refused`, `tests/bulk.rs:714-760`.
  It pins all three directions -- more is `409` and names both numbers, absent is `422`, fewer is
  `200` -- and asserts a refused action wrote **no** audit row.
  `unticking_a_row_outside_the_frozen_set_is_refused_not_quietly_widened` (`tests/bulk.rs:783`)
  covers the ordinary-grid case that now reaches the check.
- Client: `web/src/routes/contacts.tsx:508-515` sends `current.matched - current.excluded.size`.
- Browser test: the real-server `409` is covered without a stub in
  `web/e2e/audience-admin.spec.ts` by
  `a_real_server_409_count_mismatch_still_reaches_the_screen_in_its_own_words` (`:778-812`);
  breaking `refuse_if_more_than_confirmed` with `|| true` reds it.

**Residual, stated rather than hidden:** the check makes the dialog's number a contract for a
client that *shows* a dialog. A hand-built client that sends a `confirmed_count` it never
displayed is not defended against and cannot be -- the number is client-supplied, exactly as this
issue's own candidate fix proposed. What changed is that a UI bug which under-reports the dialog's
count is now caught by the server instead of only by the browser that made the request.

**2026-11-16 — one false sentence and six stale pointers in the comment above (D6-F3, D6-F12).**
The sentence saying the spec "still" describes the rejected design went stale one commit after it
was written, and nobody reread it: `142372f` corrected §9.2 immediately after `411fe0c` wrote this
comment. The pointers had drifted with the code underneath them --
`refuse_if_more_than_confirmed` `:889` -> `:897`, the five bulk bodies `:585,600,628` ->
`:592,607,635`, the required-with-token block `:797-803` -> `:805-810`, the refused-with-ids block
`:817-823` -> `:825-830`, the client's send site `contacts.tsx:495-505` -> `:508-515`, and the
real-server `409` e2e window `:664-690` -> `:778-812`, which was 114 lines out and landed inside
`the_grid_shows_the_frozen_set_and_a_newcomer_never_joins_it` instead. The e2e window now names
the test as well as the lines, so the next drift is visible rather than silent. Nothing about the
close changed: issue `03` is still resolved on the code, and the code was verified again here.

The `tests/bulk.rs` line numbers are left alone on purpose: they point at the `#[sqlx::test]`
attribute line rather than the `async fn`, which is this repo's convention and not drift.
