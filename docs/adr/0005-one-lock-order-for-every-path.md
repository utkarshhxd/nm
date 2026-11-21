<!-- SPDX-License-Identifier: AGPL-3.0-only -->

# 0005 — One lock order, written down, for every path that takes more than one lock

Status: accepted, 2026-11-21. Supersedes the per-pair ordering arguments scattered through
`public::unsubscribe`, `public::forms` and `suppressions`, which remain as local explanations of
*why* a particular line is where it is but no longer as the definition.

## The problem this exists to stop

M1d shipped four ABBA deadlocks in three review rounds. Each round fixed the pair it was told
about and wrote a paragraph arguing that pair was now safe; each next round found another pair.
They were not four unlucky bugs. They were one missing artifact.

Every one of those paragraphs reasoned about **two** paths. There are nine paths that take more
than one of these locks, so there are thirty-six pairs, and each new writer re-derives its own
order from whichever neighbouring comment it happened to read. That is a defect generator, not a
defect: the next person to add a `consent_log` insert after a `list_memberships` update introduces
the same cycle again, and no test sees it, because every one of these is a race that the full
suite — green through all four — cannot reach.

## The order

Every transaction that takes more than one of these takes them in this order, and never out of it:

```
1. the address advisory lock      suppressions::lock_address / lock_address_for
2. lists rows                     lists::lock_lists (for key share)
2a. form_lists rows               (below lists; unordered against 3-6)
3. suppressions rows
4. contacts rows                  contacts::lock_contact (for key share)
5. list_memberships rows
6. consent_log rows
```

`form_lists` is `2a` rather than a number of its own because only one thing is forced about it: it
sits **below `lists`**, since `delete_list` holds the list row and then cascades into it.

**It is unordered against 3-6, and the reason is not the one first written here.** That said "no
path takes it together with any of them", which is false: `delete_list` takes all of them in one
transaction. The cascade on `lists` fires in constraint-OID order, read off the catalog —
`list_memberships`, `imports`, `consent_log`, `form_lists`, `jobs`, `suppressions` — with
`suppressions` additionally hand-pre-locked before the delete. The real reason the position is free
is that `delete_list` is the **only** path taking them together, so there is no second path to
invert against. A right ruling on a false reason is exactly what let `form_lists` through in the
first place, which is why this paragraph is longer than the ruling needs.

**And `set_lists` does not obey "below `lists`" for every row it takes.** It holds `lists` only for
the ids it keeps or adds, then deletes the link rows of the lists it **drops** without ever holding
those `lists` rows. What makes that safe is not the ordering but `form_lists`' primary key
`(form_id, list_id)`: `set_lists(F)` touches `{F} × lists(F)` and `delete_list(L)` touches
`forms(L) × {L}`, so the two intersect in **at most one row**, and a one-row intersection cannot
host an ABBA — whoever gets that row finishes. Both arrival orders were raced and both commit. The
same argument clears `delete_form` against `delete_list`.

**Whether that argument transfers is the thing to check for the next table of this shape**, and
there is one coming: the `PUT /contacts/{id}/tags` tripwire below has the identical
delete-and-reinsert shape over `contact_tags`, which also has a composite primary key. If the
intersection is again at most one row, the same reasoning applies; if a writer ever deletes by one
half of the key alone, it does not.

Within a class, by `id` ascending. `lists::lock_lists` sorts for you.

**`contacts` was missing until it had produced three more cycles**, and the shape of that miss is
worth more than the fix. Every `list_memberships` and `consent_log` row carries a foreign key to
`contacts` exactly as it does to `lists`, so every writer took the contact row *last*, at the end
of its statement, while `delete_contact`, `selection::delete` and `erase_contact` take it **first**
and then cascade into those same membership rows. It is the `lists` inversion again, on a class
nobody had named — and it was invisible because the tests that hold the `lists` class all wedge the
contact row with a transaction that never asks for anything back, which cannot cycle. The
antagonist was standing in the fixture.

**Why this order and not another.** Four of the six positions are forced, and the other two
follow:

- `lists` cannot move below `suppressions`, because the rename trigger
  (`0027_list_scope_suppressions.sql`) runs `lists` → `suppressions` inside one statement and
  nothing can get between them.
- `lists` cannot move below `list_memberships` either, because `delete_list` locks the list row it
  is deleting before anything cascades.
- `suppressions` must sit above `list_memberships`, because `contacts::routes::erase_contact` has
  to write the tombstone before it deletes the contact — the tombstone is what makes a concurrent
  create of that address impossible, and the contact delete is what cascades into the memberships.
- `contacts` must sit **below `suppressions`**, because `erase_contact` writes its tombstone and
  only then deletes the contact; and **above `list_memberships`**, because every path that deletes
  a contact takes the row and then cascades into the memberships. Nothing forces it against
  `lists`: no path takes both exclusively, so it sits after `lists` to keep one sequence rather
  than two.
- `consent_log` goes last because it is written last everywhere: it records what the membership
  write did, so it cannot precede it.
- The advisory lock goes first because it is the only one of the five that is not a row, it is
  what serialises two transactions *about one address* before either reads anything, and a path
  that took it later would already have branched on data it did not hold.

**The two places the natural order is the wrong way round, and what was done about them.** Every
writer of `list_memberships` or `consent_log` takes a `lists` row *implicitly*, through
`list_memberships_workspace_id_list_id_fkey` or `consent_log_workspace_id_list_id_fkey`, and a
foreign key check runs at the **end** of the statement that triggered it. So the natural order of
every membership writer is `list_memberships` → `lists`, which is backwards. Each such path now
takes the `lists` rows it is about to touch explicitly, first. The same applies to `delete_list`'s
cascade, whose trigger firing order reaches `suppressions` last: it now takes those rows up front.

## What this makes impossible

A path that obeys this cannot deadlock with any other path that obeys it, whatever either does —
not "cannot deadlock with the paths somebody checked". That is the difference between a rule and
thirty-six arguments, and it is why this is a document rather than another comment.

It does **not** make lock-free. Paths still wait for each other, and
`suppressions::unsubscribe_list` carries a note about the one wait that is long (a list deletion
holds its row across its whole cascade).

## Every path that takes more than one, and where it takes them

| path | order it takes |
|---|---|
| `public::unsubscribe::save` (preference centre) | A, L (every candidate list), S, contacts, M, C |
| `public::unsubscribe::one_click` | A, L (the contact's lists), S, contacts, M, C |
| `public::forms::apply` | A, L (the form's lists), S (read), contacts, M, C |
| `public::confirm` | L, contacts, M, C |
| `contacts::routes::erase_contact` | A, S, contacts, M, C (cascade). Takes no `lists` row. |
| `contacts::routes::delete_contact` | contacts, M, C (cascade). No tombstone, so no S. |
| `contacts::selection::delete` | contacts, M, C (cascade), over a selection |
| `contacts::import::staging::upsert_from_staging` | L (the job's list and every list the file names), contacts (the upsert, exclusively), M, C |
| `contacts::selection::subscribe` / `unsubscribe` | L, contacts (the `sel` CTE), M, C |
| `lists::add_member` / `unsubscribe_member` | L, contacts, M, C |
| `lists::routes::delete_list` | L, S, M, C (cascade) |
| `lists::routes::update_list` (rename) | L, S (trigger) |
| `forms::routes::create_form` / `update_form` | the `forms` row, L (`set_lists`), `form_lists` |
| `contacts::import::routes::start_import` | L (held, not merely read), then `imports` |
| `contacts::selection::tag` | the `tags` row (`for key share`), `contact_tags` |
| `suppressions::routes::create_suppression` / `lift_suppression` | S, C (through `record_workspace_optout`) |

## The rule for new code

If you write `list_memberships` or `consent_log`, call `lists::lock_lists` with the list ids you
are about to name and `contacts::lock_contact` with the contact, in that order, first. If you also
touch `suppressions`, do that between the two. If the write is about one address, take
`suppressions::lock_address_for` before any of it.

**Use what `lock_lists` returns.** A caller that inserts must narrow itself to the ids that came
back; one that only updates may discard it, because a vanished list matches nothing and it answers
`404` anyway. Getting that wrong is a foreign key violation reported as a `500`.

If a path cannot obey this, that is worth saying out loud rather than working around — put it in
this table with the reason, and expect the next deadlock to be there.

## The method, and the closed enumeration it produced

This section used to be a short list of tables somebody had thought about, and it was wrong twice:
it omitted `contacts`, and then it said in so many words that `form_lists` was covered when it was
not. Both were right answers reached by the wrong route — "which tables feel relevant" — so here is
the route that is not guesswork.

**A class exists wherever one path writes a child table directly and another path reaches the same
rows through a parent's `on delete` action.** The cascading path takes the parent first; the direct
writer takes the child first and the parent last, through its own foreign key at the end of its
statement. That is the inversion, every time. So the enumeration is over **foreign key parents**,
because that is what bounds the set of cascades — there are nine, and every one is ruled on:

| parent | ruling | why |
|---|---|---|
| `lists` | **class 2** | cascades into `form_lists`, `list_memberships`, `consent_log`, `suppressions`, `jobs`, `imports`; several paths write those directly |
| `contacts` | **class 4** | cascades into `list_memberships`, `consent_log`, `contact_tags`, `jobs`; every membership writer takes it last through a foreign key |
| `forms` | not a class | its only child is `form_lists`, and every exclusive taker of a `forms` row takes that row first |
| `tags` | not a class **today** — see the tripwire below | its only child is `contact_tags`, and every writer either inserts `on conflict do nothing` (which retains no lock on a conflicting row) or deletes and asks for nothing back |
| `imports` | not a class | its only child is `import_errors`, written only by the run already holding the `imports` row |
| `selection_snapshots` | not a class | its only child's rows are inserted once, by the transaction that minted the parent |
| `api_keys` | not a class | one child, `selection_snapshots`, and no exclusive taker exists (below) |
| `workspaces` | not a class | thirteen children and no exclusive taker (below) |
| `users` | not a class | twelve children and no exclusive taker (below) |

**Two measured properties this rests on, worth re-checking rather than re-deriving.** The "no
cycle" result for `selection::tag` depends on Postgres granting a compatible `FOR KEY SHARE` past
an already-queued `FOR UPDATE` waiter — measured on 16.15 as granted in 1.1 ms while the exclusive
waiter went on waiting 2.4 s. That is an implementation property of `heap_lock_tuple`, **not a
documented guarantee**, so it is worth re-measuring on a major-version upgrade. And `start_import`'s
`lock_lists` does **not** hold the list for the length of a run: the `update imports … list_id`
below it already took that same row through its own foreign key and held it to commit, so the
change moves the acquisition point by microseconds and closes a window, rather than extending a
hold.

**"No exclusive taker" is the load-bearing phrase for the last three**, and it rests on a Postgres
rule rather than on a count: an `UPDATE` that changes no column of a candidate key takes
`FOR NO KEY UPDATE`, which does **not** conflict with the `FOR KEY SHARE` a foreign key check
takes. Every update to these three touches non-key columns only, and `src/` contains no
`delete from workspaces`, `delete from users` or `delete from api_keys`. If a delete appears, or an
update starts changing a key column, they become classes the same day.

`audit_log` is outside all of this: it has no foreign keys, nothing deletes from it, and it is
append-only.

### The tripwire on `tags`, and what to check rather than predict

`tags` is quiet because of the shape of its writers, not because of anything structural — the
mechanism is live. A writer that **deletes a contact's `contact_tags` rows and re-inserts them**
deadlocks against `delete_tag` exactly as `set_lists` did against `delete_list`, and that is the
obvious next endpoint: a `PUT /contacts/{id}/tags` replacing the set. It has the same shape
`set_lists` already has.

**So: if that endpoint is written, `tags` becomes class 2b and it takes `for key share` on the tag
rows first.** This is written here as the thing to check when it merges, not as a prediction.

Today's writers of `contact_tags` are `tags::apply`, `tags::remove`, `selection::tag`,
`selection::untag` and the importer's `write_memberships_and_tags` — it is **not** "only the
importer", which is what this section used to say. The ruling survives on `on conflict do nothing`,
not on the writer count. And `jobs` is not a parent at all: nothing references it.

### Known and open: the importer's tag race

`write_memberships_and_tags` still raises `23503` on `contact_tags_workspace_id_tag_id_fkey` when a
tag is deleted while it runs, failing the whole import. Its `known` CTE reads `tags` under the
statement's snapshot while the end-of-statement referential check uses a newer one, so a tag it
*found* can be gone by the time the check runs; a tag it *created* in the same statement cannot,
being uncommitted. `selection::tag`'s pre-lock does not transfer, because **the importer resolves
tags by name, not by id**, and the whole thing is one statement, so the lock cannot be hoisted
without restructuring it. Reproduced, pre-existing, and recorded here so the next person meets it
as a known gap rather than as a new finding.

**The honest warning, since this document has now been wrong twice.** The claim above that a path
obeying this cannot deadlock with another that obeys it is only as good as the enumeration. It said
that while `contacts` was missing and while `form_lists` was listed as covered. What is different
now is that the enumeration is **closed and mechanical**: nine parents, all ruled on, each with a
reason that can be re-checked against the schema rather than against somebody's memory. If a tenth
parent appears, it is a new row in that table. If a class appears that is not reachable from any of
the nine, the method itself is unsound and that is a bigger conversation than another round.
`issues/38` is the enforcement story that would stop any of this depending on somebody remembering
to ask.
