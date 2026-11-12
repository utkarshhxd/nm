# 0001. Contacts have no status column

Status: accepted (M1a). Spec: `.scratch/email-platform/specs/2026-11-11-m1a-contact-core.md` §4.

## Decision

`contacts` carries no consent or deliverability status. Consent lives in `suppressions`, keyed by address, not by contact row. The workspace-wide opt-out is a `suppressions` row with reason `unsubscribed_all`. Per-list unsubscribe lives on `list_memberships.status`.

Exactly one table can forbid sending, and it outlives the contact row.

## Why

1. **Erasure.** GDPR erasure deletes the contact row. An opt-out stored on that row dies with it, and the erased person can be re-imported and mailed. A suppression keyed on the address survives the delete.
2. **Re-import.** A status column holds whatever the last write set. An import that touches the contact silently clears the opt-out. Suppressions are never written by a contact upsert.

## Alternative rejected

One `status` column on the contact (`subscribed`, `unsubscribed`, `bounced`, `complained`), as Mailchimp, Interspire and listmonk do. It is simpler to read and to query. It fails both cases above, and it makes the meaning of "unsubscribed" depend on which write happened last.

## Cost if wrong

If a single status per contact were what users needed, the cost is a join: eligibility is a view over `contacts`, `list_memberships` and `suppressions`, measured at 1.4 s warm over 1.43M mailable contacts. That is why `GET /lists/{id}/mailable-count` is exempt from the 150 ms p95 target. Do not fix it with a denormalised counter; hand-maintained counters drift.

Reversing the decision after data exists means backfilling a status from suppressions and then keeping two sources of truth in step. Reversing it after an erased person has been mailed cannot be undone.
