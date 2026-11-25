<!-- SPDX-License-Identifier: AGPL-3.0-only -->

Status: open

# A custom field may be keyed `email` and then never render

Found by Task R3 of M2b, reported rather than fixed because the change belongs to `custom_fields`,
not to the template layer.

`custom_fields::validate_key` does not reserve the system field names. A workspace can therefore
create a custom field keyed `email`, `first_name`, `last_name` or `timezone`.

## What goes wrong

`render::Contact::value` matches **system names first**. A custom field sharing a system name is
shadowed: the operator creates it, fills it on every contact, writes `{{ email }}` expecting their
field, and silently gets the contact's real email address instead. Nothing errors, and nothing in
the UI says the field is unreachable.

It also makes a line in `template::Registry` load-bearing that was written as defensive:
`archived.retain(|k| !live.contains(k))`. R2's review flagged that removing it survived the whole
suite — which is true only because no test creates the collision this issue describes. With the
collision, that line is what stops an archived custom field shadowing a live system field.

## Why it was not fixed in place

Reserving the names is a `custom_fields` change, in M1 code, and it cannot simply start rejecting:
a workspace may already hold such a field, so the fix needs a decision about existing rows
(rename, refuse, or surface). That is a migration question, not a validation one, and smuggling it
into a template milestone is how unrelated regressions arrive.

## When this is picked up

- Reserve the system names in `validate_key` for **new** fields.
- Decide what happens to an existing collision, and write the test for whichever way it is ruled.
- Then revisit `Registry`'s `retain`, which can go back to being genuinely defensive.
