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
2. lists rows                     lists::lock_lists / lock_lists_of_contact (for key share)
3. suppressions rows
4. list_memberships rows
5. consent_log rows
```

Within a class, by `id` ascending. `lists::lock_lists` sorts for you.

**Why this order and not another.** Three of the five positions are forced, and the other two
follow:

- `lists` cannot move below `suppressions`, because the rename trigger
  (`0027_list_scope_suppressions.sql`) runs `lists` → `suppressions` inside one statement and
  nothing can get between them.
- `lists` cannot move below `list_memberships` either, because `delete_list` locks the list row it
  is deleting before anything cascades.
- `suppressions` must sit above `list_memberships`, because `contacts::routes::erase_contact` has
  to write the tombstone before it deletes the contact — the tombstone is what makes a concurrent
  create of that address impossible, and the contact delete is what cascades into the memberships.
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
| `public::unsubscribe::save` (preference centre) | A, L (every candidate list), S, M, C |
| `public::unsubscribe::one_click` | A, L (the contact's lists, locked by the sweep's own CTE), S, M, C |
| `public::forms::apply` | A, L (the form's lists), S (read), M, C |
| `public::confirm` | L, M, C |
| `contacts::routes::erase_contact` | A, S, M, C (cascade). Takes no `lists` row. |
| `contacts::import::staging::write_memberships_and_tags` | L (the job's list and every list the file names), M, C |
| `contacts::selection::subscribe` / `unsubscribe` | L, M, C |
| `lists::add_member` / `unsubscribe_member` | L, M, C |
| `lists::routes::delete_list` | L, S, M, C (cascade) |
| `lists::routes::update_list` (rename) | L, S (trigger) |

## The rule for new code

If you write `list_memberships` or `consent_log`, call `lists::lock_lists` with the list ids you
are about to name, first. If you also touch `suppressions`, do that between the two. If the write
is about one address, take `suppressions::lock_address_for` before any of it.

If a path cannot obey this, that is worth saying out loud rather than working around — put it in
this table with the reason, and expect the next deadlock to be there.

## What is not covered

`jobs`, `form_lists`, `contact_tags`, `tags` and `audit_log`. None of them is taken by two paths
in conflicting orders today: `jobs` and `form_lists` are only ever reached through a `lists`
cascade or after the `lists` row is already held, `contact_tags` and `tags` only by the importer,
and `audit_log` has no foreign keys and is append-only. Add them here if that changes.
