Status: needs-triage

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
