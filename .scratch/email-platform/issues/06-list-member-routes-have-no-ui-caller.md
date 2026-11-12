Status: needs-triage

# Two list-side membership routes have no UI caller

Found in M1a task 17 review.

## The routes

- `POST /api/v1/workspaces/{ws}/lists/{id}/members`
- `DELETE /api/v1/workspaces/{ws}/lists/{id}/members/{contact_id}`

Both are routed, authorised and covered by Rust tests. The list-detail Members table
(`web/src/routes/lists.tsx`) is read-only: it renders members and their status and offers no
control that calls either route.

## Only one of the two was a decision

The **add** picker on list detail was deliberately deferred -- the same thing can be done from
contact detail, and a contact picker over a large workspace is its own piece of work.

The **remove** was not deferred; it was never built. A contact's own page can unsubscribe them
from a list, but the list's page cannot unsubscribe a member, which is the direction someone
working through a list actually wants.

## Note the naming trap before building it

`DELETE .../members/{contact_id}` does not delete the membership. It sets the status to
`unsubscribed` and keeps the row, which is the behaviour the product depends on. Any control built
for it must say "Unsubscribe", never "Remove" -- see issue 08.
