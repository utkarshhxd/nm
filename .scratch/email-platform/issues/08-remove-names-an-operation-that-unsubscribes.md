Status: resolved

# "Remove" is the name of an operation that unsubscribes

Found in M1a task 17 review. **Blocking for M1b**: these names are free to change while this
instance is the only client, and expensive once any external one exists.

## Where the word appears

| Thing | Says | Does |
| --- | --- | --- |
| `DELETE /contacts/{id}/lists/{list_id}` | an HTTP delete | sets `status = 'unsubscribed'`, keeps the row |
| `remove_list_member` (handler) | remove | the same |
| `lists::remove_member` | remove | the same |
| `LIST_MEMBER_REMOVED` (audit action) | removed | the same |

The doc comments are explicit and correct everywhere. The vocabulary is not, and the audit log is
the worst of the four: it is the record someone reads months later to answer "was this person
removed from the list, or did they opt out?", and the action string answers the wrong one.

## The behaviour is right; only the name is wrong

Keeping the row is the point. An unsubscribed membership is the record that this person opted out
of this list. Deleting it would lose the opt-out, and the next import would re-add them as new.
`web/src/routes/contact-detail.tsx` says exactly this to the user.

## Scope

A rename, plus a migration or a mapping for `LIST_MEMBER_REMOVED` if existing audit rows are to
stay readable -- decide whether to rewrite history or accept two action strings. The UI already
says "Unsubscribe" and needs no change.

## Comments

**2026-11-14 — resolved in M1b. The rename is whole, and a grep says so.** All four things in the
table above are renamed, and both verbs changed with them:

| Was | Is |
| --- | --- |
| `DELETE /lists/{id}/members/{contact_id}` | `POST .../members/{contact_id}/unsubscribe` |
| `DELETE /contacts/{id}/lists/{list_id}` | `POST /contacts/{id}/lists/{list_id}/unsubscribe` |
| `remove_list_member`, `remove_contact_from_list` | `unsubscribe_list_member`, `unsubscribe_contact_from_list` |
| `lists::remove_member` | `lists::unsubscribe_member` |
| `LIST_MEMBER_REMOVED` / `list.member_removed` | `LIST_MEMBER_UNSUBSCRIBED` / `list.member_unsubscribed` |

Routes at `src/lists/routes.rs:44-57`, handlers at `:547` and `:717`, the shared helper at
`src/lists/mod.rs:208`, the audit constant at `src/audit/mod.rs:50`. A whole-tree grep for
`remove_(list|member|contact_from)` leaves exactly one hit in product code,
`src/workspaces/members.rs:205`'s `remove_member` -- which is a **different and genuine** removal:
it is `delete from memberships where workspace_id = $1 and user_id = $2` (`:237`), a workspace
member, not a list membership. The rest of the hits are the deliberate historical ones below.

**The history question is answered: two action strings, never a rewrite.** Rows already in
`audit_log` keep `list.member_removed`, so the log still says what the system called the operation
at the time. The reasoning is in the constant's own comment and in `GLOSSARY.md`, which carries an
entry under the **old** name -- that is the convention for anything renamed here.

- Commits: **`999e30a`** ("feat: remove becomes unsubscribe"), **`7a17694`** (the contact-side
  route, which the spec had not named and which is the one the UI actually calls -- it is listed
  first in this issue's own table), **`b675186`** (the migration test and cross-workspace pinning).
- Tests, `tests/lists.rs`: both action strings are accepted and read as one operation (`:628-...`);
  `migrations_leave_an_old_audit_row_as_written` uses `#[sqlx::test(migrations = false)]`,
  `MIGRATOR.run_to(1)`, inserts the old-action row, then runs the rest and asserts it unchanged --
  a real `migrations/0099_tidy.sql` carrying a `like 'list.member\_r%'` rewrite reds only that
  test; `neither_unsubscribe_route_crosses_a_workspace` pins `404` for a non-member and another
  workspace's editor on both routes, with the membership and the audit log untouched.
  Reverting the verb reds eight tests across two suites.
- Glossary: the `Unsubscribe (a list member)` and `list.member_removed` entries, dated 2026-11-13.

Worth recording because it changes what drifted: the contact-side route **already shared the audit
helper**, so it already wrote the new action string. The audit action was never the thing that
drifted -- only the route paths and the handler names were. The first attempt to guard the
no-rewrite rule was a substring grep for `member_removed`, which a migration written as
`where action like 'list.member\_r%'` walks straight through; that was found in review and
replaced with the `run_to` test above.
