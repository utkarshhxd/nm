# 0004. Selection is a materialised set

Status: accepted (M1b). Supersedes [ADR 0003](0003-selection-is-a-snapshotted-predicate.md). Spec: `.scratch/email-platform/specs/2026-11-12-m1b-segments.md` §8. Code: `src/contacts/selection.rs`, `src/contacts/routes.rs`, migration `0017_selection_snapshots.sql`.

## Decision

A selection is a **set of contact ids, written down**, not a predicate carried to the action.

`POST /contacts/selection` evaluates the predicate once, inside one transaction, and materialises the ids it matched:

```sql
insert into selection_snapshot_members (workspace_id, snapshot_id, contact_id)
select $1, $2, id from contacts where workspace_id = $1 and … limit 1000001
```

The token it returns is `{workspace_id, snapshot_id, issued}` and nothing else, HMAC'd under `nm:selection:v3`, with no `serde(default)` on any field. `snapshot_id` is a `Uuid::now_v7()`, so the issue time comes back out of the id and the one-hour TTL is still checked before the database is touched. `issued` is the materialising insert's own `rows_affected`, which is also `selection_snapshots.size` and the number the confirmation dialog shows.

**A bulk action narrows `contacts` by the frozen set; it never reads ids out of it.**

```sql
with sel as (
  select id from contacts
   where workspace_id = $1
     and id in (select m.contact_id from selection_snapshot_members m
                 join selection_snapshots s
                   on s.workspace_id = m.workspace_id and s.id = m.snapshot_id
                where m.workspace_id = $1 and m.snapshot_id = $2
                  and s.expires_at > now()))
```

Three parts of that are requirements rather than style:

- **Narrowing, not supplying.** `selection_snapshot_members` deliberately carries no foreign key to `contacts` (the cascade would need an index the primary key cannot serve, so `bulk/delete` of a million contacts would do a million index scans). Reading ids *out* of it and inserting them into `contact_tags` or `list_memberships` would therefore hit a foreign-key violation the moment a single selected contact had been deleted since selection — and that violation kills the whole action, not the one row. Narrowed, the deleted contact is simply absent and `affected` is short, which the response already reports.
- **The `expires_at` join.** Expiry must not depend on the reaper, which deletes one snapshot a minute and can be hours behind.
- **The workspace clause on both sides.** The members rows carry `workspace_id`, so another tenant's snapshot id resolves to nothing rather than to their contacts.

What follows from recording identity instead of inferring it:

- **No widening is possible**, so `Selection::widened` becomes `selected > issued` — the exclusion term M1a added back is not merely redundant but wrong (below) — and the guard is kept only as defence in depth, with a test asserting it never fires.
- **No same-count swap is possible.** ADR 0003's remaining hole closes with it.
- **The count is exact for free**: it is the insert's `rows_affected`.
- A contact deleted between selection and action is absent; a contact that has changed so it no longer matches **is still acted on**, which is correct — the person confirmed *these contacts*, not *this query*.
- The client sends back **the number on the button** (`confirmed_count`: the selection's size less the rows unticked), and the server refuses, inside the transaction, if the action would reach *more* than that. That closes issue `03`, open since M1a because fixing it needed exactly this design change. Comparing `confirmed_count` with `selection_snapshots.size` instead would compare it with the token's own signed `issued` — a number the server already had, which a replayed token with fewer exclusions satisfies. The comparison is one-sided for the reason the next bullet gives.
- A contact deleted between selection and action is absent, the action affects fewer, and the `issued` / `selected` / `affected` gap reports it. Fewer is therefore never refused: somebody else's delete is not this person's error.
- The audit row copies `selection_snapshots.filter`, with `q` redacted. The token carries no predicate and the snapshot is deleted within the hour; without the copy the log would read *"deleted 417,004 contacts, snapshot 0192…"* against a row nobody can look up — worse than M1a had, on the action that cannot be undone. **The snapshot itself keeps `q` as it was sent**, deliberately: it lives for one hour, it is deleted with the selection, and it is read back only by the person who typed it, while the audit row is permanent and outlives a GDPR erasure. The redaction belongs to the copy that persists, not to the one that does not.
- `GET /contacts?snapshot_id=…` returns the frozen set, and answers `422 selection-expired` — never an empty page — when the snapshot is expired, swept, superseded or another workspace's. "Zero contacts matched" is a different fact from "this selection is gone", and the grid cannot offer to select again if the server will not say which.

## Alternatives rejected

**A list of ids in the request.** "Select all 2.3 million matching" cannot be sent as ids. This is why ADR 0003 existed.

**A predicate re-run at action time, bounded by a timestamp.** ADR 0003's design, with the repair its "cost if wrong" proposed: bound every joined fact by the token's moment and claim the resolved set is always a subset of what the user saw. It was broken twice, with measurements:

1. **Negation inverts it.** `not_has(tag)` compiles to `NOT EXISTS (… created_at <= t)`. A contact that *had* the tag at `t` was correctly not shown. Delete the tag row, and the bounded `NOT EXISTS` becomes true — the contact is acted on, having never been shown. That is a **superset** violation, not a subset one. The same inversion reaches `not_member_of` through the `updated_at` maintenance the design itself mandated: an unsubscribe hides a membership that existed at `t`, pulling somebody who *was* on the list into a "not on the list" action. On an email product that is the re-solicitation predicate. `tests/bulk.rs` pins both operators in both directions.
2. **A timestamp is not a commit order.** `now()` is transaction-start time, so a transaction that starts, writes, and commits two minutes later produces rows stamped *before* a token issued in between — rows the token could not see, which nonetheless pass `created_at <= as_of`. Issue `04` was filed as blocking *for the importer*, and an import is exactly a long transaction.

No bound fixes this. Subset-correctness for `¬P` needs proof that the fact was **absent** at `t`, and deleting the row destroys the evidence that it was ever present.

**Comparing the confirmed count with the snapshot's size.** It is the same number the token carries, signed, so the check verifies only that the client can echo. The number that can disagree — and that a person actually agreed to — is the one on the button, which is the size less the rows they unticked. The cost of comparing against what the action reached is that an exclusion outside the frozen set is now refused rather than silently acted on: today the grid shows the live filter, so unticking a row that is not in the snapshot makes the dialog understate, and a dialog reading "Delete 1 contact" would delete two. `?snapshot_id=` removes that case at source.

**Keeping M1a's widening arithmetic.** `selected + excluded.len() > issued` was right while `issued` was the whole match at issue time and `selected` the match *now* less the exclusions: two different quantities, and comparing them raw let a widening of up to `MAX_EXCLUDED` (10 000) through. Under a frozen set `issued = |S|` and `selected = |S| − |E ∩ S|`, so the sum exceeds `issued` **whenever an excluded id is not in the snapshot** — a `409` on a set that cannot widen. Snapshots make that case more likely, not less: the grid shows the live filter with `refetchOnWindowFocus` on, so it diverges from the frozen set by every contact created, edited, tagged or subscribed since. Unticking one of those is the most ordinary action on that screen. A tripwire that fires in normal operation is a tripwire whoever is on call deletes.

## Cost if wrong

The cost is paid whether or not it is wrong, and it is large. Measured on a 1M-row table, floor case, no predicate beyond `workspace_id`:

| | |
|---|---|
| `insert … select` of 1,000,000 ids | **4,887 ms** |
| `pg_total_relation_size` of the members rows | **158 MB** |
| `delete … where snapshot_id = $1` | **850 ms** |

One selection can therefore cost five seconds and hold 158 MB for an hour, against `spec.md:458`'s 150 ms server-time budget — two orders of magnitude over, and the one endpoint in M1b that misses a stated budget. It is recorded as a deviation rather than left silent. Four consequences, each a requirement: the cap is 1,000,000, applied as `limit 1000001` so one extra row is the whole evidence and the refusal names no count; `POST /contacts/selection` is rate-limited at least as hard as preview; a new selection supersedes the same actor's previous live one, which bounds one person's footprint to one snapshot; and the insert runs in a detached task, because a handler future is dropped when the client disconnects.

**Both** snapshot tables are `unlogged` (`migrations/0017_selection_snapshots.sql:21` and `:55`): logged, 158 MB per selection would go through WAL and into every base backup. A crash truncates them together, which produces the same "your selection expired, select again" path the TTL already produces — so that decision is only safe while the TTL path exists. The parent has to be unlogged too, or that sentence is false: Postgres would truncate only the members and leave `selection_snapshots` carrying its `size`, both readers look at the parent alone, and the action would answer `200 {selected: 0}` over an empty page instead of `422 selection-expired`. The parent was made unlogged in the whole-branch fix pass (`b15259e`); this paragraph named only the members until D6's record pass, which is how it came to be the one of three storage records not saying "both". The 158 MB above is `pg_total_relation_size` of the **members** rows and is unaffected.

If the `expires_at` join or the token's age check were ever dropped, the failure is quiet: a Friday selection acts on Monday, or an unswept snapshot acts for as long as the reaper is behind. Both are pinned by tests, one through the handler and one through the statement itself.

If a merge feature ever lands — an importer replacing one contact with another — a frozen set stops meaning what it says, because identity is the only thing it records. M1b's spec §15.3 carries that: an importer may **update** a contact, never replace one, and a merge must remap snapshot members or invalidate live snapshots.

The one-hour TTL is still a guess. Nobody has watched a real operator work a bulk action.
