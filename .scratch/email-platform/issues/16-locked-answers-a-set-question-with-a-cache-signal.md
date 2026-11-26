Status: needs-triage

# `locked` answers a set question with a sort-sensitive cache signal

Found in the M1b whole-branch review (D4-F3, `final-d4-client.md`). Recorded rather than fixed: it
errs safe, and narrowing it needs tracking that Task 11 already considered and declined once.

## What it is

`web/src/routes/contacts.tsx`:

```ts
const locked = query.isPlaceholderData && selection.mode === "all";
```

`isPlaceholderData` tracks `viewKey`, which carries `sort` and `dir` because re-ordered really is
a different page of cursors.

The hazard the lock exists for is documented beside it and pinned by
`audience-admin.spec.ts::the_rows_cannot_be_ticked_while_the_frozen_page_is_in_flight`: a
placeholder row may be **outside** the snapshot, so unticking it would exclude an id the snapshot
never held, the dialog's number would then understate what the action reaches, and the server
refuses with `409 selection-count-mismatch`.

That can only happen in the one transition where `snapshot` goes from absent to present — the
round trip right after `selectAllMatching` lands. Every other transition that sets
`isPlaceholderData` under a frozen selection (any sort, any filter change) shows placeholder rows
that are *already* the snapshot narrowed some other way, and every one of them is safe to tick.

## Why it is the third residue of one confusion

The other two were fixed in the M1b whole-branch pass: the clearing effect keyed on `filterKey`
instead of `coveredKey`, and before that Task 12's `exportCount` / `exportIsSelection`. All three
are the same swap — the grid's **cache identity**, which must carry sort, used where the question
is about **which contacts a view covers**, which sort has nothing to do with.

This one is not a correctness defect, which is why it is recorded and the other two were not: it
fails safe. The cost is friction and a sentence that implies the wrong reason — both tick boxes,
Ctrl+A and row-Space go dead, and "Loading the contacts you selected. These are the rows you were
looking at before" is rendered, on every column-header click under a frozen selection.

## What a fix has to carry

Narrow it to "the covered filter or the snapshot changed", **not** to anything derived from
`viewKey`. That needs `snapshot !== shownSnapshot` tracking — a second ref beside `shownKey`,
holding which snapshot the rows on screen belong to. Task 11 declined exactly that once, so this
is a decision to revisit rather than an oversight to correct, and whoever revisits it should read
Task 11's reasoning first (M1b Task 11's report, in the review notes kept outside the repository).

Whatever is done, `the_rows_cannot_be_ticked_while_the_frozen_page_is_in_flight` must still go red
when the lock is removed entirely.

## Comments
