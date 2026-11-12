# Glossary

The domain vocabulary of NM. Use these terms, not synonyms, in code, issues and tests. The M1a entries are taken from §3 of `.scratch/email-platform/specs/2026-11-11-m1a-contact-core.md`.

**Contact** — a person in one workspace, identified by email address. Has built-in `first_name` and `last_name`, a timezone, and values for the workspace's custom fields. A contact belongs to exactly one workspace; the same address in two workspaces is two unrelated contacts.

**Custom field** — a typed, workspace-scoped definition. Values live in the contact's `attributes` JSONB, keyed by the field's immutable `key`.

**List** — a named set of contacts a campaign can be sent to. Has its own double-opt-in setting and a visibility that decides whether it appears in the preference center (M1c).

**List membership** — the relationship between a contact and a list, carrying the subscription status. This is where "unsubscribed from this list" lives.

**Tag** — a label on a contact. Unlike a list, a tag is never a send target and carries no subscription state. Tags organise; lists receive mail.

**Suppression** — a standing block on an address or a domain. Workspace-scoped or instance-wide. This is the only thing in the system that can say "never email this, whatever else says otherwise".

**Eligible** (or *mailable*) — a derived property, never stored: a contact is eligible for a list when their membership is `subscribed` and no live suppression matches their address or its domain.
