Status: needs-triage

# No e2e covers how an archived custom field is presented

Found in M1a task 17 review.

## What is uncovered

Two behaviours, both deliberate, both asserted only by reading the source:

1. `web/src/components/contact-form.tsx` filters archived definitions out of the edit form, so an
   archived field cannot be given a new value from the UI.
2. Contact detail marks an archived field's existing value with "(archived field)" rather than
   hiding it, because the Task 3 ruling kept those values readable.

The Rust side is covered: archiving is tested, and so is the server's refusal of new values. The
Playwright suite never opens a contact form in a workspace that has an archived definition.

## Why it matters

Both behaviours are one conditional each. A change that unfiltered the form would let someone
write a value the server then refuses -- a form that fails on submit for a reason it never showed.
A change that dropped the marker would present a stale value as current. Neither would turn
anything red today.

`web/e2e/audience-admin.spec.ts` already archives a field (`Region`) in
`deleting_a_field_warns_that_values_are_kept` and checks the value survives over the API, so the
fixture exists. It needs the two screen-level assertions on top.
