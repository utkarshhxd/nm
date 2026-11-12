Status: needs-triage
Labels: needs-triage

# Erasing a contact leaves its address in plaintext on any instance-wide block

Found in M1a task 9 (GDPR erase). Severity is low in practice: instance-wide address blocks are
rare and only an instance admin can create one.

## The gap

`POST /contacts/{id}/erase` deletes the contact and replaces every suppression row for that address
in the contact's workspace with a hash-only tombstone. It cannot touch an instance-wide row
(`workspace_id` null): a workspace admin must not delete another scope's block. If such a row exists
for the address, the plaintext survives on it, readable by instance admins through
`GET /api/v1/admin/suppressions`. The erasure is therefore partial.

## The obvious fix is actively harmful

Do NOT null the plaintext `email` on the instance row, and do NOT delete the row. The view
`mailable_list_members` (migration 0015) matches an instance address block on `s.email = c.email`.
Nulling or deleting it silently UNBLOCKS the address for every other workspace that still has the
person as a contact, and they would be mailed. That is the worst failure this product can have, and
no test would go red.

## Correct fix

The view's instance-address clause has to match on a hash instead of plaintext. That needs an
`email_hmac` column on `contacts` (kept in step with `email`), a migration, and a backfill, and only
then can the instance row be reduced to a tombstone. It belongs to the milestone that reworks the
view. Migrations 0011 and 0015 are applied everywhere and must not be edited.

## Operator workaround

On a self-hosted instance the admin is the data controller and can clear or replace the instance
row by hand. Before doing so, check that no other workspace still holds the address as a contact,
for the reason above.
