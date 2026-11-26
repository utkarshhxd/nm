# 50 — The template list carries every document, has no cursor, and saves render inside a transaction

Status: item 1 (the list carries every document) closed by M2c R3: `GET /templates` returns `TemplateSummary` (`id, name, kind, created_at, updated_at`). Item 2 (no cursor) was already served by the house `PageParams` and is closed with it: the list takes `?cursor=&limit=`. Item 3 (the save compiles inside a transaction) stays open.

**Found by:** the M2b final whole-branch review (findings M6, M7), and named by W6 as a
deliberate shortcut.
**Blocks:** nothing in M2b. Revisit before a workspace is expected to hold more than a few
dozen templates.

## What is true today

1. **The list returns full documents.** `GET /api/v1/workspaces/{id}/templates` returns the
   whole `tree` or `source` of every row (`src/template/routes.rs`, the list handler), but the
   screen shows only name, kind and date. The worst case is 200 rows of up to 256 KiB each,
   about 51 MB in one response.
2. **The list stops at 200 rows and has no cursor.** A workspace with more than 200 templates
   cannot see the rest.
3. **`mrml` runs while a database transaction is held open.** In create and update, the
   compile runs on the async worker inside the open transaction, so a connection and its row
   lock are held for the length of a render. The render is bounded by `MAX_SOURCE` (256 KiB)
   and by the 120/min preview limiter, but it is CPU work done while holding a lock.

## Why it was not fixed in M2b

None of the three is wrong for a workspace of realistic size today, and each fix touches a
wire shape or the ADR 0005 lock order:

- A summary list shape needs a second response type and a web change.
- A cursor needs the paging pattern the other modules use.
- Compiling before the transaction opens needs the update path's read of the stored version's
  archived fields (the `keep_archived` set) to move outside the transaction. That is
  ADR 0005 work, not a one-line move.

M2b chose to ship and record this rather than widen the milestone.

## The fix, when it is taken

- A `TemplateSummary` list response (`id, name, kind, updated_at`) and the house cursor
  pagination.
- Compile before `begin`. Read what compile needs with a plain select first, then hold the
  transaction only for the write. Add the new path to ADR 0005's path table and to
  `tests/lock_order.rs`.
- `spawn_blocking` around `mrml`, once the render is outside the transaction.
