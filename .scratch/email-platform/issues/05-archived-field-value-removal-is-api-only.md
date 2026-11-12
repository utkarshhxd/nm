Status: needs-triage

# Removing one contact's value for an archived custom field is API-only

Found in M1a task 17 review.

## The gap

`PATCH /contacts/{id}` with `{"attributes": {"key": null}}` removes one contact's value for one
field, archived or not. It is implemented and tested, and the Task 3 ruling that kept archived
values readable gave "a per-person data-correction or erasure request" as the reason it must
exist.

The UI cannot do it. `web/src/components/contact-form.tsx` filters archived definitions out of the
edit form, so an archived field has no control to clear. The screen's only answers to "remove just
this person's value" are:

- **Purge** (`POST /custom-fields/{id}/purge`), which destroys that field's value for *every*
  contact, or
- **Erase**, which destroys the whole contact and writes an unliftable suppression.

Both are wrong answers to the question, and one of them is irreversible for everyone.

## Why the form filters archived fields

Deliberately: an archived field must not accept *new* values, and the simplest way to guarantee
that in a form is not to render it. The fix is not to unfilter it. It is to render an archived
field that already has a value as read-only, with a clear control and nothing that can set a new
one.

## Scope

`contact-form.tsx` renders archived-with-value as read-only plus Clear; the merge-patch path
already supports it; plus an e2e. See also issue 07, which covers the missing e2e for the current
filtering behaviour.
