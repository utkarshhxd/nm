Status: needs-triage

# The `on` operator compares calendar days in the database server's timezone

Found in the M1b whole-branch review (D2-L5, `final-d2-sql.md`). Recorded rather than fixed: the
spec does not rule on it, and M2's scheduling work meets the same question.

## What it does

`src/segments/compile.rs:214-225`:

```rust
Op::On => {
    lhs.push_as(q, kind);
    q.push(if kind == FieldKind::Date { " = " } else { "::date = " });
    push_operand(q, kind, &r.value);
    if kind != FieldKind::Date { q.push("::date"); }
}
```

For `created_at`, `updated_at` or a `datetime` attribute that emits
`contacts.created_at::date = $1::timestamptz::date`. A `timestamptz → date` cast resolves through
the session's `TimeZone` GUC. Nothing in the application sets it — `src/db.rs`'s `PgPoolOptions`
has no `after_connect`, and `grep -rn "set time zone" src/` finds nothing — so the day boundary is
whatever `postgresql.conf` says on that deployment.

A `date` attribute is unaffected: the stored value is already a calendar day. `before` and `after`
are unaffected: they compare instants.

## Why it matters

Workspaces carry a timezone (`workspaces.timezone`, validated by `validate_timezone`), and a
person who asks for "created on 2026-01-01" means *their* day. Today:

- the same saved segment selects different contacts on two installs of NM, with no setting in NM
  that explains the difference;
- for any non-UTC workspace, up to a day's worth of contacts fall on the wrong side of the
  boundary — nearly a full day's intake for a workspace whose signups cluster in local evening.

## Why this is a gap, not a violation

Spec §4.3 lists `on` as an operator name for date and datetime fields and rules on nothing else.
No binding sentence is being broken, so there was nothing for the milestone's review to fail.

## What it costs

Cheap now: `(contacts.created_at at time zone $tz)::date = $1::date`, with the workspace's
timezone bound like any other value. The compiler would need the workspace's timezone passed in,
which it does not take today — `push_rules` receives a `Group` and nothing about the workspace.

Expensive later. Once saved segments depend on the current answer, changing it silently moves
which contacts they match, and there is no version on a stored document that would let a reader
tell "evaluated under the old rule" from "evaluated under the new one".

## Why it should be settled with M2

M2 ships scheduling, which asks the same question in a sharper form — "send this at 9am" is a
local-time statement and needs the workspace's timezone on the evaluation path anyway. Deciding
the two together gets one answer and one mechanism for passing a workspace timezone into query
building, rather than two that have to be reconciled afterwards.

## Comments
