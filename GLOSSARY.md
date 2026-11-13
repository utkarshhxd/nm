# Glossary

The domain vocabulary of NM. Use these terms, not synonyms, in code, issues and tests. The M1a entries are taken from §3 of `.scratch/email-platform/specs/2026-11-11-m1a-contact-core.md`.

**Contact** — a person in one workspace, identified by email address. Has built-in `first_name` and `last_name`, a timezone, and values for the workspace's custom fields. A contact belongs to exactly one workspace; the same address in two workspaces is two unrelated contacts.

**Custom field** — a typed, workspace-scoped definition. Values live in the contact's `attributes` JSONB, keyed by the field's immutable `key`.

**List** — a named set of contacts a campaign can be sent to. Has its own double-opt-in setting and a visibility that decides whether it appears in the preference center (M1c).

**List membership** — the relationship between a contact and a list, carrying the subscription status. This is where "unsubscribed from this list" lives.

**Unsubscribe (a list member)** — sets a membership to `unsubscribed` and keeps the row: that someone opted out of this list is the fact worth keeping, and a later import must not re-add them as new. Route `POST /lists/{list_id}/members/{contact_id}/unsubscribe`; handler `unsubscribe_list_member`; `lists::unsubscribe_member`; audit action `list.member_unsubscribed` (`LIST_MEMBER_UNSUBSCRIBED`). These names superseded `DELETE /lists/{id}/members/{contact_id}`, `remove_list_member`, `remove_member` and the audit action `list.member_removed` (`LIST_MEMBER_REMOVED`) in M1b (issue `08`, 2026-11-13). Nothing is removed, so nothing is called `remove`.

**`list.member_removed`** — the audit action the system wrote for the same operation until M1b. Superseded by `list.member_unsubscribed`, but **never rewritten**: rows already in `audit_log` keep it, so the log still says what the system called the operation at the time. Anything that reads audit actions must accept both, and read them as the same operation.

**Tag** — a label on a contact. Unlike a list, a tag is never a send target and carries no subscription state. Tags organise; lists receive mail.

**Suppression** — a standing block on an address or a domain. Workspace-scoped or instance-wide. This is the only thing in the system that can say "never email this, whatever else says otherwise".

**Eligible** (or *mailable*) — a derived property, never stored: a contact is eligible for a list when their membership is `subscribed` and no live suppression matches their address or its domain.
