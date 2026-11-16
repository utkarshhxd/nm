# M1b decision record

Decisions taken while building milestone M1b (segment rules and the rule builder, segment counts,
selection snapshots, the bulk engine rewired onto them, CSV export, membership provenance, and the
`remove` → `unsubscribe` rename) whose reasoning does not fit in a code comment and is not
recoverable from the diff.

Read `m0-decisions.md` and `m1a-decisions.md` first; everything in them still applies, and two of
the traps in `m1a-decisions.md` bit again this milestone (below). This file covers what M1b added,
in the same shape: what was decided, why, and what it costs if the decision was wrong.

The architectural decisions proper live in `docs/adr/`. M1b's are
**0004 (selection is a materialised set)**, which **supersedes 0003 (selection is a snapshotted
predicate)**. 0003 was superseded rather than edited, for the same reason the milestone refuses to
rewrite old audit strings: rewriting a record makes it a worse record than it was.

**This file points at those two ADRs and does not restate them.** 0004 already carries the
decision, the three required properties of the read, the measured counterexamples that killed the
timestamp variant, and the cost table. Two records of one argument will eventually disagree, and
the one in `docs/adr/` is the one a stranger finds.

---

## Correctness decisions you should not casually reverse

### A selection is a frozen set of contact ids, written down
`src/contacts/selection.rs`, `src/contacts/routes.rs`, migration `0017_selection_snapshots.sql`.
ADR 0004 carries the full argument. The parts worth repeating here: **a bulk action narrows
`contacts` by the snapshot and never reads ids out of it**, because supplying ids hits a
foreign-key violation the moment one selected contact has been deleted since selection, and that
violation kills the whole action rather than the one row; the `expires_at > now()` join is in the
statement so that expiry never depends on the reaper; and the workspace clause is written on both
sides, so another tenant's snapshot id resolves to nothing rather than to their contacts.

The cost is paid whether or not the decision is wrong and it is large — 4,887 ms and 158 MB for a
1M-row selection, against a 150 ms budget. Four things exist because of it and are requirements,
not tuning: the 1,000,000 cap applied as `limit 1000001`, the rate limit on
`POST /contacts/selection`, one live snapshot per actor (a new selection supersedes the same
actor's previous one), and the materialising insert running in a detached task because a handler
future is dropped when the client disconnects. **Both** snapshot tables are `unlogged`, which is
safe only while the "your selection expired, select again" path exists — and the parent had to
join the members there in the whole-branch fix pass, because a crash that truncated only the
members would leave the parent carrying its `size` and both readers would pass, answering
`200 {selected: 0}` over an empty page instead of `422 selection-expired`.

### The confirmed count is compared with what the action reached, never with the snapshot's size
`refuse_if_more_than_confirmed` in `src/contacts/routes.rs`, called at the top of `finish_bulk`, so
all five bulk routes pass through it and a sixth cannot be written that forgets it. This is issue
`03`'s actual close, and the distinction is the whole value of it: `confirmed_count` is **the
number on the button** — the snapshot's size less the rows the person unticked — while
`selection_snapshots.size` is the token's own signed `issued`, a number the server already had.
Comparing against the size verifies only that the client can echo, and a replayed token with fewer
exclusions satisfies it.

**The comparison is one-sided on purpose.** More than confirmed is refused, because that is the
direction in which a dialog reading "Delete 1 contact" deletes two. Fewer is not refused: a contact
somebody else deleted between the selection and the action is simply absent, the
`issued`/`selected`/`affected` gap already reports it, and refusing would turn their delete into
this person's error. Making the comparison two-sided reds the fewer-than-confirmed half and
`deleting_a_selected_contact_leaves_the_action_working_and_the_count_short`.

### `confirmed_count` is a deliberate breaking API change, not a regression
Required when the body carries a `selection_token`, refused when it carries `ids` (the ids *are*
the number, and a count nothing can check is a count nobody should send). Any M1a script now gets a
`422`. Accepted for the same reason the `remove` → `unsubscribe` rename was: nothing is published,
this instance is the only client, and both breaks are free now and expensive once an external
client exists.

### The dangerous state is unrepresentable in the type, not merely guarded
A three-field token plus an unchanged `of_ids` would have put `snapshot_id: Option<Uuid>` and
`ids: Option<Vec<Uuid>>` on one struct, leaving `{None, None}` representable — a selection narrowed
by nothing, which is `bulk/delete` of the whole workspace. Instead the payload is
`selection::Token {workspace_id, snapshot_id, issued}` and the resolved thing is an enum,
`Selection::Snapshot { token, filter } | Selection::Ids { workspace_id, ids }`. That is M1a's
composite-foreign-key principle applied to a Rust type. Do not flatten it back into one struct for
convenience.

### Every count state is its own value; a null never stands for more than one fact
The segment-count response carries a per-row discriminated state — `exact`, `estimated`,
`no_estimate`, `unreadable`, `failed` — because one `null` had been standing for three different
facts and then four: an `attr:` document that cannot be counted exactly, a timeout, a stored
document that will not parse, and a planner call that failed. `no_estimate` means no honest number
exists, `unreadable` means the document is broken and needs re-saving, and `failed` means we could
not get a number this time and you may retry. Saying any of those about another is a lie, and it is
cheaper to kill in the type than to keep arguing it in a comment. `POST /segments/preview` is
unchanged: its null only ever means `attr:`.

### The compiler writes no `workspace_id` clause, so every caller must
`src/segments/compile.rs`, exactly like M1a's `push_filter`. The structural test proves the
emitted fragment is more than a workspace clause; it cannot prove the caller added one. Segment
CRUD, preview, counts, the grid, selection and export each write their own. The suppression arms
are the one place where the correlation is load-bearing rather than defence in depth: suppression
matches on `contacts.email` and `email_domain`, not on a contact id, and one address legitimately
exists in several workspaces. The list and tag arms *are* defence in depth — the whole-branch fix
pass put the first line in the parser, which resolves a `list` or `tag` id against the workspace
and refuses one naming no row, so a foreign id no longer reaches the compiler at all. The test
that carries their tenancy asserts that refusal, and then asserts **the database refuses a
crossing insert** (SQLSTATE 23503) rather than asserting on rows, because the composite foreign
keys make such a row unwritable and no row-based test could ever go red.

### `is_false` on a boolean attribute means stored false
A boolean attribute is three-valued — true, false, absent — so `is_true` / `is_false` / `is_set` /
`is_not_set` form a strict partition, asserted disjoint and exhaustive. Letting `is_false` also
cover absence would make "stored false" unaskable, while the reverse is expressible as
`any[is_false, is_not_set]`. `suppressed is_false` still means "no blocking suppression", because
`suppressed` is computed and cannot be unset. §4.5's "a NULL value matches every negative operator"
was written about negations of positive operators and does not reach an arm of a partition; the
spec says so now because an implementer read it literally and correctly.

### A rule that matches everything is unrepresentable, not merely unlikely
An empty group is refused at parse. `is_set` / `is_not_set` are refused on `email`,
`email_domain`, `created_at` and `updated_at`, which are all `NOT NULL` and would compile to a
vacuously true predicate — one misclick away in a rule builder. `%`, `_` and `\` are escaped with
an explicit `ESCAPE` clause, because an unescaped search for `50%` matches every row: the same
shape arriving through a value rather than through a document. `timezone` stays eligible for
`is_set` because it is genuinely nullable, and `first_name` / `last_name` stay because they are
`not null default ''` and the `''`-as-unset convention makes the operator mean something.

### The one string formatted into SQL in this milestone
sqlx 0.9 will not take a dynamic statement, so the preview timeout is
`AssertSqlSafe(format!("set local statement_timeout = {timeout_ms}"))`, with the injection audit
written at the call site. The only interpolated value is a `u64` from `Config`, so the statement
can differ from the literal only in its digits. **Widening that interpolation to any non-numeric
type reopens an injection path nothing else in M1b has.**

### A save share-locks the definitions it resolved, and the lock order is one way
A segment save takes `for share` on the non-archived definitions it resolved, so a concurrent
purge blocks until the save commits and the purge's guard then sees the new segment. There is no
deadlock cycle, and the reason is an ordering invariant worth preserving: the purge locks only its
own already-archived field row, a save share-locks only non-archived definitions, and every holder
of a definition lock either takes nothing else or takes the `segments` row **before** the
definitions. If anyone later makes the purge or the archive lock a `segments` row, or makes a save
lock a definition after a `segments` row, that argument stops holding and `40P01` becomes reachable
as a 500 where a clean `409 field-in-use` belongs.

---

## The dead designs, recorded so nobody proposes them again

**A predicate re-run at action time, bounded by a timestamp.** ADR 0003's design with the repair
its own "cost if wrong" proposed: bound every joined fact by the token's moment and claim the
resolved set is always a subset of what the user saw. It was broken twice, with measurements, and
the long form is in ADR 0004 under *Alternatives rejected*. The two sentences that matter: negation
**inverts** it, so deleting a tag row makes a bounded `NOT EXISTS` true and a contact that was
never shown is acted on — a superset violation, and through `not_member_of` it is the
re-solicitation predicate; and a timestamp is not a commit order, because `now()` is
transaction-start time, so a long transaction (an import, which is what issue `04` named as the
trigger) commits rows stamped before a token issued in between. No bound fixes it: subset
correctness for `¬P` needs proof the fact was *absent* at `t`, and deleting the row destroys the
evidence that it was ever present. `tests/bulk.rs` pins both operators in both directions.

**A list of ids in the request.** "Select all 2.3 million matching" cannot be sent as ids. This is
why ADR 0003 existed at all, and it is still true — which is why the ids are written down on the
server instead of travelling.

**Riding M0's session sweep for the snapshot reaper.** The session sweep is
`delete from sessions where expires_at <= now()`, inline in a user's request, once a minute,
best-effort. That is right for a 200-byte row and wrong for a 158 MB one, for two separate reasons:
it only runs while somebody is logged in, so Friday's snapshots survive to Monday; and the measured
850 ms delete cascade would land on the hot path of a stranger's authenticated request. The reaper
is its own task instead, deleting one expired snapshot a minute for as long as the process runs, so
no request pays for it at all. §19 records partitioning by expiry hour as the upgrade if churn ever
measures badly.

---

## Deliberate exceptions, so you do not "fix" them

- **`selection_snapshot_members` has no composite foreign key to `contacts`.** It is the first
  tenant table holding a `contact_id` without one, against M1a's "unrepresentable, not merely
  unwritten". The cascade would need an index on `(workspace_id, contact_id)` that the primary key
  cannot serve, so `bulk/delete` of a million contacts would do a million index scans. A dangling
  `contact_id` is harmless **because UUIDv7 is never reused** — that clause is load-bearing — and
  because the read narrows rather than supplies.
- **`refuse_if_widened` is kept and must never fire.** A frozen set cannot widen, and a test
  asserts the refusal never fires, including with exclusions that are not in the snapshot — the
  case M1a's arithmetic wrongly refused. It costs one comparison and it is the last line if the
  snapshot join is ever bypassed.
- **A viewer can read any snapshot in their own workspace through `?snapshot_id=`.** They can
  already read every contact there; a snapshot is those same contacts grouped differently. It does
  reveal *what a colleague selected*, which is a small within-workspace disclosure worth knowing
  about and not one that crosses a trust boundary.
- **The export is of the filter, not of the selection.** `POST /contacts/export` takes a
  `NewSelection` and has no `snapshot_id`, so it re-evaluates when the stream runs. Since the grid
  now shows the frozen set, "export the contacts you selected" would be a false sentence in exactly
  the situation the person is most likely to be in, so the dialog says which it is. Do not add
  `snapshot_id` to the route to make the simpler sentence true: that is a spec change, not a UI
  tidy.
- **The export dialog invents no row count.** `Page` carries only `data` and `next_cursor`, so the
  grid has no total, and the only exact count available is `POST /contacts/selection` — up to a
  million rows written and a bite out of the selection quota to produce a *label*. It names a count
  only when an exact one for that exact filter is already in hand, says when it was taken, and
  otherwise says the row count is in the audit log. A labelled stale count beats a sometimes-count,
  because a label tells the reader what kind of number they are reading while an absence is
  indistinguishable from "we never count".
- **The CSV download buffers the whole file in browser memory.** CSRF needs an `X-CSRF-Token`
  header, so neither a form POST nor an `<a>` can work, which leaves `fetch` plus a blob — the
  thing §10 forbids on the server, reintroduced on the client. It carries a `shortcut:` comment;
  the upgrade when somebody needs it is a signed short-lived GET URL so the browser streams to
  disk. Two consequences are handled rather than discovered: the download does not start until the
  whole stream has arrived, so a large export looks hung and the dialog says so, and a failure
  arrives after the person confirmed, so the error lands where they are still looking.
- **The tag confirmation is not red; the export confirmation is.** The axis is the one
  `m1a-decisions.md` already names — whether the effect can be taken back. "Remove tag" undoes a
  tag completely and nothing sends to a tag yet, while you cannot un-download a file. Red on the
  tag dialog, two controls from bulk delete on the same bar, trains people to read red as
  meaningless, and that cost is paid by the control that really needs it.
- **The preview and counts limiter is in memory and per process.** What it defends is this
  process's 20-connection pool, so a per-process counter matches the resource exactly and costs no
  round trip on the cheap path. A sign-in lockout must survive a restart; a debounce cap must not.
  N instances allow N times the rate, which is still N times their own pools.
- **Counts and previews share one budget, and the budget is per workspace.** 30 a minute covers
  both the segment list's batch count and the rule builder's debounced previews, so reopening the
  segments page twenty times in a minute shows a real operator the "counting is busy" alert. The
  number is provisional and was chosen before anyone watched real use.
- **`is distinct from $new::jsonb` stays although a text compare behaves identically today.**
  Canonicalisation through `Group` serialisation removes key-order variance on the current write
  path, but number *form* survives it: store `{"value":1}`, PATCH `{"value":1.0}`, and the jsonb
  compare leaves the version alone where a text compare bumps it. Anything that ever writes `rules`
  without going through `Group` — a migration, a repair script, a direct write — reintroduces the
  rest.
- **A PATCH resending `rules` byte-identical to the stored document is accepted even when it names
  an archived field.** The rule is that a *new* rule cannot name an archived definition, and an
  unchanged document contains no new rule. Refusing it would break the pattern every client uses,
  which is to send the whole object on save. The comparison happens under a `for update` row lock
  and the identical case still parses and writes, so the version comparison above stays exercised.
- **A `failed` count row renders the quiet `—` and contributes to one page-level alert.** Not a
  per-row Retry: twenty-five identically-named Retry buttons and twenty-five `role="alert"` spans
  were removed once already, and axe cannot catch them. One failure is one event, said once for the
  page.
- **`source` is replaced when an unsubscribed membership is revived.** The live row answers "how
  did *this* membership arise"; the unsubscribe already broke the old chain, and a row saying
  `import` about a subscription somebody made in the UI last week is a false record. The history
  belongs to M1d's consent log.

---

## Things that bit us, recorded so they do not bite again

Two of these are already in `m1a-decisions.md`. They are listed because they **bit again**, not to
be written out twice.

- **rust-embed, again, and this time the ordering.** `cargo build --release` is a **no-op** after a
  web-only change: `web/dist` is baked in at compile time and cargo does not re-run the macro when
  only those files change, so the old binary serves the old UI and the suite passes against code
  that was never loaded. One implementer lost four attempts to it and "proved" a working fix broken
  each time. The order is `pnpm -C web build` → `cargo build --release --bin nm` → e2e, and **a run
  made in the wrong order says nothing at all**, which the earlier record did not say. The habit
  that beats remembering: delete the binary, rebuild, and grep it for a string you just added. A
  *debug* build reads `web/dist` at runtime, which is why a mutation loop can use one.
- **Never pipe a build to `/dev/null`, again** — and the theoretical case in `m1a-decisions.md`
  happened. `pnpm -C web gen:api` **run from Windows** fails to link and truncates
  `web/src/api/openapi.json` to empty through its `>` redirect. Run it through `scripts/dev.sh`
  only and check the file is non-empty before committing. Relatedly, a wrapper that swallowed
  output turned a clippy run into an unexplained `101` with no diagnostic; every gate now writes to
  a log file first.
- **A task that regenerates `schema.d.ts` must also run `pnpm -C web typecheck` *and*
  `pnpm -C web build`.** Making one Rust field `Option<T>` turned the generated type into
  `string | null | undefined` against a `strict` consumer, and CI went red on a milestone where
  every brief had asked for `gen:api` and none had asked for the check that consumes its output.
- **A migration edited after it was applied.** `0017` was edited in place, which is allowed while
  the migration is unreleased — the alternative is a permanent record of a mistake in the schema —
  but it must be **announced**, because any database already at that version refuses it with
  "previously applied but has been modified". The recovery order matters: drop what the migration
  created, **then** `delete from _sqlx_migrations where version = N`, then re-run. Leaving the row
  makes the re-run skip the file and leave the schema missing. Second occurrence in two milestones.
- **`mv file.orig file` restores the older mtime**, so cargo reuses a stale binary and the first
  full run after a mutation sweep shows a false failure. `touch` after restoring.
- **`grep -F` with a multi-line pattern matches any one line of it.** A grep-based "did my mutation
  land" guard therefore reports success for a mutation that never applied. Compare against a backup
  instead.
- **A whole-file `diff` after a scripted edit can be a line-ending artefact, not a bad edit.** A
  Python rewrite normalised `schema.d.ts`'s CRLF to LF, so plain `diff` reported all 5,556 lines and
  looked exactly like a mutation that had gone far wider than intended;
  `diff --strip-trailing-cr` showed the single intended line. This belongs beside the `grep -F`
  rule because it is the same trap wearing different clothes: **both are "did my mutation land?"
  guards that lie** — one by matching any single line, the other by reporting every line. A guard
  that can report success for an edit that never happened, or failure for one that did, is worse
  than no guard, because it is trusted.
- **A mutation that fails to apply is the inverse failure of mutation testing.** It changes nothing,
  the suite stays green, and that is indistinguishable from a test that does not discriminate — so
  it manufactures confidence that an assertion is vacuous. A `sed` that did not match across a
  newline cost a cycle here. The standing rule it produced: assert the replacement matched **exactly
  once**, print a hash before and after, and confirm the file changed, before concluding anything
  from a green suite. Later rounds also guarded on the build: three mutations that did not compile
  would otherwise have run against a stale `dist`.
- **The working tree is CRLF in places, so a patch written with LF newlines matches nothing.** Not
  "matches badly" — matches zero times and changes zero bytes, which is indistinguishable from a
  mutation that proved an assertion vacuous. D5's occurrence-count assertion caught it; a grep
  would not have. The tree is **mixed**: `src/contacts/export.rs` and the spec under `.scratch` are
  CRLF while `src/app.rs`, `src/contacts/routes.rs` and `tests/*.rs` are LF, so a patch tool must
  detect the ending **per file** rather than assume one. This is the third face of the same trap as
  the `grep -F` and `diff` entries above, and the same cure answers all three: assert the
  replacement matched exactly once and compare against a byte copy.
- **`export A=$DB B=$A` expands `$A` before the assignment lands.** The shell evaluates the whole
  command's word list first, so `B` gets whatever `A` held *before* the line, which is usually
  nothing. That aimed a `nm migrate` at the real `nm` database instead of the intended throwaway,
  and is how the migration-17 checksum error in the handoff was produced. Use two statements.
- **A Playwright serial skip prints one character away from a pass.** A skipped test is
  `-  28 [chromium] › …` against a pass's `✓`, and the only unambiguous statement is the summary
  line's `9 did not run`. A mutation report on a serial file must quote **`did not run`**, never
  `passed`: the two look identical in a scrollback and the second is a claim the run did not make.
- **A mutation that reds the wrong test proves nothing about the new one.** In a serial Playwright
  file the first failure **skips** the rest rather than passing them, so an over-broad mutation
  looks like proof and is not. Two mutations were correctly discarded for this before being counted.
- **Agents share one scratchpad path and overwrite each other's tools.** A mutation helper was
  silently replaced by another agent's byte-exact variant that does not unescape `\n`, so the next
  mutation matched zero times and did nothing. Name a mutation helper per task; the match-count
  assertion is what makes the collision survivable.
- **One e2e file can exhaust a per-workspace quota.** `audience-admin.spec.ts` now makes exactly ten
  selections against `SELECTION_PER_MINUTE = 10` and runs in 22 seconds, so all ten land in one
  fixed window and refused requests count too. Zero headroom. A whole file has already failed once
  on a `429` that read exactly like a product defect. **Anything that needs a selection uses its own
  workspace.**
- **Narrowing a Playwright run with `-g` silently changes the fixture.** `attr:plan` is in the CSV
  header only because of three earlier tests in the file.
- **`window.__x` set by the app is invisible to `page.evaluate`** — Chromium isolated world, shared
  DOM, different global. Probe through the DOM.
- **A real failure can look exactly like a known flake.** The clippy bind-mount flake recurred
  again, and in the same session a red clippy was a genuine `too_many_arguments`. Retry before
  reading anything into a red clippy, and do not let the retry become the conclusion.

---

## What this milestone learned about its own evidence

This is the part worth more than any line of the diff.

- **Twenty-two assertions have now been found that passed against deliberately broken code, and the
  twenty-second was in the measuring harness itself.** `layoutProblems` skipped `display: none` —
  and, worse, any zero-height box, which is the harder case a reviewer found afterwards — so a full
  sweep of all twenty-nine screens in both themes certified a rule builder drawing **fifty
  invisible rows**. The four new entries alone would have certified a screen that rendered nothing.
  (The count runs continuously from the nine recorded in `m1a-decisions.md`, so thirteen of the
  twenty-two are M1b's; the ledger calls the running total "this milestone" in places, which
  overstates M1b's share.)
- **Two tallies, two shapes, and they must not be added together.** They have been merged once
  already, in a fix pass's own report, which is why this entry exists:
  - **Vacuous evidence** — an assertion that **passes against deliberately broken code**. Running
    total **22**, of which **13 are M1b's**; the bullet above is that tally and nothing else.
  - **A false fact** — a comment, report or label **asserting something untrue**, whatever the
    tests do. Six in M1b: the `confirmed_count` doc comment, which described the opposite of what
    the server compares and reached third-party clients through `openapi.json`; the
    `worst-case-data.spec.ts` navigation comment; the `segments_compile.rs` "their negations match
    everyone" comment; the fix pass's own claim that nothing gates `openapi.json`, which CI does;
    issue `14`'s claim that `contacts` has "no composite unique", in a ticket marked
    `ready-for-agent`, i.e. one an agent is cleared to action without a human reading it (D6-F1);
    and the "six for six" entry this file carried until D6, inherited from the D5 brief (D6-F2,
    below). The wrong numbers and stale pointers D6 also found are not in this tally — a pointer
    that drifted is cured by re-reading the pointer, not by reading the sentence against the code.

  The two are different defects with different cures. A vacuous assertion is found by mutating the
  code and watching nothing go red; a false fact is found by reading the sentence against the code
  it describes, and no mutation will ever surface it. **A tally that merges them stops meaning
  anything** — it can neither be checked nor acted on, because the two numbers answer different
  questions.
- **Seven of them were one single shape: a negative assertion with nothing to wait on.**
  `toHaveCount(0)` polls, so a stale value clears itself while the test watches. Enumerated,
  because a count nobody can check is not evidence, and the one list in this file that *was*
  enumerated is the one whose wrong entry got found:

  1. `"Selection cleared because the filters changed."` absent — the live region's *next*
     announcement always overwrites it, so the text is never there to find
     (`task-11-report.md:161-165`).
  2. `"The action failed and nothing was changed."` absent — raced the alert (`:166-168`).
  3. a row absent on a grid that had not drawn a row yet (`:169-172`).
  4. the page-wide `"too many to count exactly"` pair in the failed-row test — a page-wide absence
     has nothing to wait on (`:297-303`).
  5. `toHaveAccessibleDescription("")` after the lock lifts — the paragraph it points at is
     unmounted by the very transition being checked (`:419-423`).
  6. the export control absent on the editor page — passed against a build that shows it to
     everyone, once its neighbours were stripped (`task-12-report.md:157-176`).
  7. **not identified anywhere.** `task-11-report.md:425-426` counts "five of this family found in
     my own tests … against one the reviewers found", and that reviewer's one is never named in
     any report or in `progress.md`, which numbers a fourth and a sixth and no fifth
     (`task-11-rereview.md:338` went looking for a fifth in that round and found none). The
     arithmetic closes — five from Task 11's implementer, one from a reviewer, one from Task
     12's — so seven is right, but entry 7 is the part of this bullet a reader takes on trust.

  **Six of the seven were found by implementers in their own isolation runs** — mutate each
  negative twice, once with its positive neighbour present and once with the neighbours stripped.
  That is the specific thing to grep for; it is not a general exhortation to check negatives.
- **"Untestable" and "unreachable" are hypotheses, not findings — and this milestone went five for
  five.** Five such claims were written and **every one of them was wrong. None was upheld.**
  Listed by the task that wrote them, so the number is checkable rather than quoted:

  1. Task 4's per-worker plan node — fell to three zeroed parallel GUCs.
  2. Task 7's history rewrite — fell to `migrations = false` plus `run_to(1)`.
  3. Task 11's unreachable `409` — fell to a held route.
  4. Task 11's L4 untestable client guard — fell to holding the real lazy chunk in a fresh browser
     context.
  5. Task 13's "cannot be measured" page alert — fell to a twelve-line test.

  **The tell is the wording.** All five were written at the end of a task, about a precondition the
  writer had not attacked, and all five said *"I cannot see how"* rather than *"I tried X, Y and
  Z."* Attack it for ten minutes before you write it down, and a reviewer who accepts such a claim
  without trying has not reviewed it.

  The count has moved four times, which is its own warning. The ledger wrote "four for four" at
  Task 11; Task 14 made it six claims, five disproven, with Task 12's F2 upheld, and enumerated
  exactly the five above; D5 made it "five claimed, five disproven, none upheld" — a five that
  counts F2, so not the five above; the record then committed "six for six". **D6 found that the
  sixth entry was never one of these claims at all** — see the entry below. Count the list, do not
  quote the sentence.
- **A false premise travelled from a controller's brief into the repo through three hands, and no
  hand checked it.** Until D6 this file's list above had a sixth entry: Task 12's F2 stream abort,
  "the one that had held", said to have fallen to D5's fifth attack. It is false in both halves.
  Task 12's F2 (`task-12-review.md:133-143`) is about `web/e2e/audience-admin.spec.ts:958` —
  `route.abort("failed")` firing a pre-response abort where the comment claimed a mid-body one —
  and its four named attacks (`task-12-fix-1-report.md:78-97`) are all browser-interception
  attacks: `route.fulfill` twice, CDP `Fetch.fulfillRequest`, CDP `Fetch.continueResponse`. What
  D5 refuted (`final-d5-report.md` D5-4) is a **Rust server test** against
  `src/contacts/export.rs:274-285`. No browser is involved at any point, and **none of Task 12's
  four attacks was shown wrong**.

  The path is fully traceable, which is why it is worth more than the entry it replaces. **The
  controller wrote it**, in the D5 brief (`final-d5-tests.md:88-97`): "Task 12 claimed this one
  was untestable and that claim was upheld." Task 12 never claimed it. D5 inherited the sentence
  and repeated it as fact because the brief asserted it; the next fix round committed it to this
  file in `83f05cb` because the report said it. Each hand was reasonable. Nobody read the Task 12
  record, which took D6 one `sed` to do. This file already carries "the spec says what was
  decided, not what was built"; this is its sibling — **a brief says what the controller believes,
  not what happened**, and a factual claim inherited from a brief gets checked exactly as hard as
  one inherited from a comment. The route is brief → report → record, and the first of those three
  is the one seat nobody reviews.
- **An out-of-scope observation that names its lever is a prediction, and this one paid.** Task
  12's rereviewer saw the same gap from the other side, declined to widen the task, and wrote it
  up as an out-of-scope observation (`task-12-rereview.md:183-187`): `src/contacts/export.rs`'s
  `Err` branch "has no test on the server side either … this is the cheaper and more honest lever
  than any browser-side synthesis: it tests the thing that actually produces the state." Two tasks
  and a whole-branch review later, D5 ran exactly that lever. Four server-side attacks were
  rejected for stated reasons (a session `statement_timeout` the test cannot aim at a pooled
  connection, `ALTER DATABASE` that
  binds only later sessions, DDL the open cursor's lock blocks, and dropping a pool the stream no
  longer needs) and the fifth worked: `pg_terminate_backend` on the export's own backend, whose
  pid the test knows because it acquired the connection itself. 35 chunks, then
  `Err("export failed")`, audit row `{"complete":false,"rows":7627}`, now pinned by
  `tests/export.rs::a_query_that_dies_mid_stream_aborts_the_body_and_is_audited_incomplete`. That
  is an out-of-scope note doing its whole job, and the counter-example to the instinct that an
  observation deferred is an observation wasted.
- **A green consistency gate is evidence of propagation, not of correctness.** This is how one of
  the false facts above reached third-party clients with every gate green at every step, and
  it was itself recorded wrongly once: the fix pass wrote that nothing gates `openapi.json`, and CI
  does. `.github/workflows/ci.yml:42` diffs the committed spec against the live server, and
  `:62-63` regenerates `schema.d.ts` and fails on any diff. The chain is tight and it held. What it
  gates is **consistency, not truth**: one wrong doc comment on `confirmed_count` propagated
  faithfully into `openapi.json` and then into `schema.d.ts`, because each step only ever asks
  *"does the generated artefact match its source?"* **None of them can ask "is the source true?"**
  So a generator chain multiplies whatever it is given, and a green run of it says the lie is
  consistent — which is worse than no gate would have been, because it reads as a check. The source
  comment is the only place that question can be asked, and only by a person reading it against the
  code beneath it.
- **The sharpest thing anyone said this milestone was said about themselves.** Task 13's implementer
  closed its report:

  > Round 0's "no margin" and "silent expiry" worries were both wrong, and both were things I
  > asserted without measuring in a task whose whole subject is measuring. The alert claim was the
  > third. I would rather that pattern were looked at than any single line of this diff.

  All three were checked and all three were false: the scroller assertion had 388 px of slack, not
  none; the `segmentRow` locator does not expire silently, because one constant feeds both the seed
  and the locator and a broken locator fails loudly in `visit()`; and the "cannot be measured" count
  alert was a twelve-line test. **This is a finding about process, not about code** — the person
  doing the measuring was the one making unmeasured claims, which is the whole point, and naming
  one's own pattern unprompted is the habit to copy. The same implementer later named a second:
  "I was reasoning about the control and not about its content, in a task whose whole subject is
  what content does to controls."
- **The good habits that came out of it, all cheap.** Run a no-mutation control first, so a green
  run proves the mutated assets were actually served. Prove a new bound catches what the *old* one
  missed, not merely that it fails on the obvious break. Type a stubbed response body against the
  generated schema: the product is already covered by those types, so an untyped stub is the only
  place a wire rename slips through quietly. And delete a vacuous assertion rather than shipping it
  — somebody did that unprompted for the first time this milestone.

---

## Known gaps at the end of M1b

Issues are filed under `.scratch/email-platform/issues/`, numbered from `01` and still being added
to: `01`–`12` by Task 14, `13`–`18` by the whole-branch pass, `19` by the fix round after D5.
**This sentence carries no total on purpose.** It said "twelve" for three commits, then
"eighteen" against nineteen files; a count here goes stale every time somebody files a ticket, and
nothing checks it. List the directory and read each file's `Status:` line. `03`, `04` and `08` are
resolved by this milestone, with the evidence in their own `## Comments`; `14` is
`ready-for-agent` on its §1 and §2, whose two plans are confirmed by `EXPLAIN`, and explicitly not
on its §3. The rest are open, and these are the ones that shape what comes next:

- **`09` — query-string params ignore unknown keys** has grown teeth. `GET /contacts` now takes
  `snapshot_id`, so a client that misspells it is served the **live filter** instead of the frozen
  set, with no error — the exact "a client that believes it is narrowing a listing and is not"
  failure, now on the one screen where narrowing is a safety property. `CountParams`, added this
  milestone, does deny unknown fields, so the convention now exists on the query side too and the
  three M1a structs are the exception.
- **`11` — twelve M0 JSON bodies ignore unknown fields** is unchanged, and the convention it is
  measured against is now stronger: every body added in M1b denies unknown fields, including all
  five bulk bodies and the signed token payload. `ChangeRole` is still the one to look at first.
- **`12` — M0 PATCH bodies ignore an explicit null**, filed during this milestone. The real work is
  one extraction: `present` is copy-pasted in four places.
- **The reserved finding is closed, not reserved.** `web/src/routes/contacts.tsx`'s effect now
  depends on `[coveredKey]` and not `filterKey` (whole-branch MUST 5/7, `4a2df0f`), so ticking
  three rows and then sorting the grid keeps the ticks: which contacts a view reaches is what a
  tick belongs to, and sorting moves no contact in or out. D5 then found that the *announcement*
  half of the repair had no test — removing `silentClear.current = true` left all 127 committed
  e2e tests green — and added one.
- **The one-hour TTL is still a guess.** Nobody has watched a real operator work a bulk action.
- **Still almost no load testing.** Three things have been measured and nothing else has: the
  snapshot numbers in ADR 0004, issue `14`'s `EXPLAIN` run on 50,000 segments and 50,000
  snapshots, and the export pool saturation — four workspaces × five exports held all twenty
  connections, a twenty-first request still waiting at 8.00 s, `/readyz` at `503` after 2.00 s,
  which is what `MAX_CONCURRENT_EXPORTS` now bounds. **All three changed a conclusion the
  reasoning had reached**: ADR 0004's timestamp variant died of its measurement; the export figure
  produced a new constant, a new `503` and a new spec bullet; and issue `14`'s run showed the
  supersede delete spends 96% of its time in the members FK cascade and not in the index everyone
  was looking at. What *merely confirmed* what reading had predicted is one half of that same
  run — the two index plans themselves — which is the outcome you cannot know in advance, and is
  the reason to run it. (Written first as "two of the three changed a conclusion, the third merely
  confirmed", which counted issue `14`'s run on both sides and left the export figure on neither.)
  The segment compiler has never run against 10M rows,
  `estimate()`'s fallback has never been watched on a real table, and issues `01` and `02` remain
  open and untriaged.
- **A merge feature breaks the frozen set.** Identity is the only thing a snapshot records, so an
  importer that replaced one contact with another would make a frozen set stop meaning what it
  says. §15.3 carries it: an importer may **update** a contact, never replace one, and a merge must
  remap snapshot members or invalidate live snapshots.
- **M1c inherits three written obligations from the export**, documented together at `push_cell`:
  strip a leading `'` from a cell; split `lists` and `tags` on a `;` not preceded by a backslash,
  split a list entry at its **one** unescaped `:`, then unescape `\`, `\;` and `\:`. And it must
  **refuse** a list entry with more than one unescaped `:` rather than coping with it, because a
  lenient parser hides an escaping bug in the exporter — which is exactly how the exporter's own
  bug stayed green until the test was tightened.
