<!-- SPDX-License-Identifier: AGPL-3.0-only -->

Status: open

# ADR 0005's `users` and `api_keys` deletion rulings are still a grep someone ran once

Raised by Task R8 of M2b, which closed the same gap for `workspaces` and deliberately stopped
there.

## What is now enforced

M2b added `tests/lock_order.rs::nothing_in_src_deletes_a_workspace`. It scans every file under
`src/` recursively for a `delete from` or `truncate` against `workspaces`, and
`the_workspace_deletion_matcher_sees_what_it_is_for` holds the matcher against 11 positive and 7
near-miss samples (`workspaces_archive`, `update workspaces`, and similar, which must not match).

It catches any case, any whitespace including newlines inside the statement, Rust `\` line
continuations in LF and CRLF, the `only` and `table` keywords, a `public.` qualifier, quoted
identifiers, a statement split across adjacent string literals, and `workspaces` inside a CTE.

It misses a table name not spelled out in `src/` (`format!("delete from {t}")`, `concat!(..)`),
anything outside `src/`, and `drop table`. Those limits are stated in the test's doc comment and
in the ADR.

## What is not

ADR 0005 makes the same no-deletion claim for `users` and `api_keys`. Nothing checks either one.
Both rulings rest on a grep someone ran once, by hand, at a point in time — which is the same
state `workspaces` was in before R8.

## Why it was left out of M2b

Scope, and the reason is worth writing down rather than leaving as a judgement call.

`workspaces` was in scope because **M2b changed it**: migration 0032 added two edges out of it,
taking it to 17 children. A `delete from workspaces` would now cascade into all seventeen and take
the entire wait-for graph, so it is the single statement that could invalidate the whole ADR, and
M2b is what made it that much worse. `users` and `api_keys` are untouched by this milestone. The
gap is real but it is not M2b's to close, and widening a milestone to tidy adjacent ground is how
milestones stop ending.

## What closing it looks like

The matcher is already written, already tested against 18 samples, and already parameterised by
table name in all but form. Extending it is a loop over a list of table names, not new logic.

**One thing to decide rather than assume while doing it:** a legitimate delete may already exist.
Revoking an API key, in particular, is plausibly implemented as a row deletion. If the extended
test goes red, that is not a test to loosen — it means ADR 0005's ruling for that table is wrong
and needs re-deciding on the evidence. Stop and re-rule rather than weakening the check or
carving out an exception for the caller that tripped it.

## See also

- `docs/adr/0005-one-lock-order-for-every-path.md` — the rulings in question.
- `tests/lock_order.rs` — the enforcement that now exists for `workspaces`.
