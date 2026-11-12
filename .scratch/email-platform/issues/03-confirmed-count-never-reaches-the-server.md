Status: needs-triage

# The count a person confirmed never reaches the server

Found in M1a task 17 review. This is a design gap, **not a defect**: nothing is broken today and
no test is wrong. It is recorded so the next person does not assume the server enforces what the
dialog said.

## What the server can and cannot enforce

A bulk action is bounded by `issued`, the live count signed into the selection token
(`src/contacts/selection.rs`). `bulk/delete` and `bulk/subscribe` refuse when the predicate
resolves to more than that. That is the only number the server has.

The number the person actually agreed to is a different one. The browser computes `promised` from
the selection bar -- matched, less the rows they unticked -- renders it in the confirmation
("Delete 1 contact") and compares it against `affected` when the result lands
(`web/src/routes/contacts.tsx`, `describe`). It is never sent.

So a request carrying the same token with fewer `excluded_ids` legitimately acts on more contacts
than any dialog said, and the server cannot tell: the action is within `issued`, which is what the
token stands for. Only the browser that issued the request notices, through the `short < 0`
branch, and only after the fact.

## Why it is not urgent

The caller is an authorised editor either way -- a token is not a privilege escalation, and
`begin_bulk` re-authorises on every call. The realistic failure is not an attacker but a UI bug
that under-reports the count in the dialog, or a third-party client that builds the body by hand
and gets the exclusions wrong. Neither is caught anywhere.

## Candidate fix

Carry the confirmed count in the bulk body -- `expect_at_most`, say -- and have the server refuse
when the resolved `selected` exceeds it. That makes the dialog's number a contract rather than a
decoration, and it composes with the existing `issued` check rather than replacing it: `issued`
bounds the token, `expect_at_most` bounds the request.

It is an API change -- a new field on three `deny_unknown_fields` bodies, the UI sending it, and a
decision about what an API client that omits it should get. Do it deliberately, in its own task,
not as a drive-by.
