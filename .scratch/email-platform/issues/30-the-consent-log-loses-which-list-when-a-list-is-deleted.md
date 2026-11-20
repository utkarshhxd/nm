Status: needs-triage

# The consent log loses *which* list once that list is deleted

Found during M1d Tasks 10/11's review, when the admin panel tried to distinguish two cases that
the data cannot.

## What happens

`migrations/0020_consent_log.sql:33` carries `on delete set null (list_id)` — deliberately, and
correctly: Task 2 chose it so that **deleting a list cannot destroy the evidence that someone
consented**, which cascading would have done. `src/lists/routes.rs:388` is a hard delete.

So after a list is deleted, every consent row that referred to it has `list_id = NULL`. And a
genuinely workspace-wide row — a one-click unsubscribe, a suppression — also has `list_id = NULL`,
because that is what "no list" means.

**The two are now indistinguishable.** A row recording "unsubscribed from the Weekly Digest"
becomes, once Weekly Digest is deleted, a row recording "unsubscribed from everything".

## Why it matters

Spec §4's first anti-goal: *"A consent log that loses the how, the when or the where is worse than
none, because it invites false confidence."*

This is the **where**, lost. The row still proves the person acted and when and through what
source, which is most of the value — but it now overstates the scope of what they did. Reading the
log back, an operator or a regulator would conclude the person opted out of everything when they
opted out of one topic.

It is worse than a gap because it is not visibly a gap. Nothing on the row says "this used to name
a list".

## What was done now

The admin panel shows one honest label covering both cases. The branch that claimed to tell them
apart was removed, because it could never be reached and asserted a distinction the data does not
carry.

## What a fix looks like

Snapshot the list's identity on the row at write time — `list_name text`, written by every writer
alongside `list_id`, never updated afterwards. Then a deleted list still reads as "Weekly Digest
(deleted)" and a workspace-wide row still reads as workspace-wide, and the two are distinguishable
forever.

That is a migration plus a change at every consent writer — the single-row writers in
`src/lists/`, both set-based writers' CTEs in `src/contacts/`, the importer's CTE, and all four
public routes. It is not a label change, which is why it is filed rather than folded into a UI fix
at the end of a milestone.

**Do it before the log has rows that matter.** Every row written between now and then is a row
whose "where" cannot be recovered, because the list it named is gone by definition.

## Related

- `migrations/0020_consent_log.sql` — the `set null` decision and its reasoning, which stands.
- `issues/24-constraint-changes-on-consent-log-will-block.md` — the other open `consent_log`
  schema note; if both are done, do them in one migration.
