Status: needs-triage

# Five small findings the M1b whole-branch pass recorded rather than fixed

Found in the M1b whole-branch review (D4-F7, D4-F8, D4-F9 and D1's two cross-dimension notes).
Each is real, each is small, and none was this branch's job. Kept together because splitting five
one-paragraph findings into five files would cost more to read than to keep.

## Client

### 1 — a selection arriving by handover is the only one never announced

`web/src/routes/contacts.tsx`, the `useMemo` that adopts the handover and the `[selection]`
effect.

The `useMemo` adopts the handed-over selection during the **first** render, so
`selection.mode === "all"` on mount. The `[selection]` effect's first run is swallowed by
`firstRun.current`, and for a handed-over selection that is the only run it would ever have. So
the one navigation the person did not initiate from this screen — arriving from the segments list
with a frozen set — is the one whose bulk bar is never announced, and that bar is the thing they
need confirming.

The guard is needed: without it a plain mount announces "Selection cleared". The fix is a targeted
announcement where the handoff is adopted, not a change to the guard.

### 2 — a bulk delete reads its outcome twice

`web/src/routes/contacts.tsx`. The delete path sets `toFocus.current`, so the result paragraph
(`tabIndex={-1}`) takes focus and is read, **and** pushes the same sentence into the live region
500 ms later. One or the other, not both.

### 3 — the header checkbox clears from indeterminate (pre-existing)

`web/src/components/data-grid.tsx`'s `onToggleAll`, with its caller in `contacts.tsx`. It selects
the loaded rows only when the mode is `none`; from indeterminate it **clears**, where most grids
select all. In mode `all` it discards the entire frozen selection.

Both outcomes are announced, and the M1b diff does not touch these lines, so this is older than
the milestone and is recorded rather than pressed.

## Server

### 4 — two spellings of one comparison have drifted

`src/contacts/selection.rs`'s `subscribe` binds `status` twice and compares the second copy
against a bare `'subscribed'` literal with no cast, where `lists::add_member` writes
`$4::text = 'subscribed'`. Both work — sqlx declares the parameter's OID — but the same comparison
is now written two ways in two files, which is how the next change to one of them misses the
other.

### 5 — the purge guard's `jsonb_path_exists` is a full scan

`src/custom_fields/routes.rs`'s `field-in-use` query matches `'attr:' || field.key` against the
stored rules through `jsonb_path_exists`, over a workspace's whole `segments` table with no index
that can serve it. It is workspace-scoped and injection-free (the key goes through
`jsonb_build_object`), and it runs only on a purge — a rare, admin-only act. Fine at today's row
counts; named here so it is not rediscovered as a surprise.

## Comments
