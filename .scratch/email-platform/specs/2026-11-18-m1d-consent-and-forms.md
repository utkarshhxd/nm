# M1d — consent, the preference center, and signup forms

Completes **M1 Audience** from `spec.md` §11. Everything else in M1 ships: contacts, custom
fields, lists, tags, segments, import and export, suppressions, GDPR erase. What is missing is the
half that lets a person outside the product change their own mind about being mailed.

## 1. Intent

Today a contact gets into a workspace three ways: typed in, imported, or created through the API.
All three are the operator acting. Nothing lets the **subscriber** act — there is no way for
someone to sign up, confirm, see what they are subscribed to, or leave.

That gap is not a missing screen. It is the reason the product cannot legally send marketing mail
in most of its target markets, and it is the reason M2 cannot ship: RFC 8058 one-click unsubscribe
is required on every marketing message, and there is nothing for its URL to point at.

**This milestone builds the subscriber's side of the product**, and the record that proves consent
was given.

## 2. Goals

1. A person can sign up through a form the operator embeds on their own site, or through a hosted
   page, and is not mailable until they confirm.
2. A person can unsubscribe in one click, without logging in, and it takes effect everywhere
   within five seconds.
3. A person can see what they are subscribed to and change it, on one page, without an account.
4. Every change of consent is recorded append-only, with enough context to answer "prove this
   person agreed" years later.
5. M2 can be built on this without revisiting it: the unsubscribe URL, the token format and the
   enqueued-mail contract are all settled here.

## 3. Non-goals

- **Sending the confirmation email.** M1d enqueues it; M2's pipeline sends it. See §6.4.
- Click and open tracking. Shares the token layer built here; the endpoints are M2's.
- Tracking domains and on-demand TLS (`spec.md` §225). M2, with the rest of deliverability.
- A visual form builder with drag-and-drop. Forms here are a field list and settings.
- CAPTCHA integration. Honeypot and rate limiting only — see §8.

## 4. Anti-goals — failure even if it all works

- **A confirmed subscriber who cannot be proven to have confirmed.** A consent log that loses the
  how, the when or the where is worse than none, because it invites false confidence.
- **An unsubscribe that does not stick.** Any path that lets a later import, API call or form
  submission silently re-subscribe someone who left is a product-ending defect, not a bug.
- **A public endpoint that leaks.** Tokens that are guessable, enumerable, or that disclose
  whether an address exists in a workspace.
- A preference center that shows someone a list they were never on, or hides one they are on.

## 5. Constraints

- Every tenant table carries `workspace_id`; cross-workspace reach returns 404, never 403, never
  the data. Public endpoints resolve their workspace **from the token**, never from a parameter.
- Migrations are append-only once released. `0012_lists.sql` already carries
  `double_optin boolean not null default true` and `visibility ('public','private')`, and
  `list_memberships.status` already allows `pending`. **M1a decided this; M1d honours it and must
  not re-model it.**
- Secrets never appear in logs, audit metadata or API responses.
- Public pages are server-rendered and must work with JavaScript disabled. They are reached from
  an email client, on an unknown device, by someone who may be annoyed.

## 6. Approaches considered

### 6.1 Topics: a new entity, or `lists.visibility`?

**Decided: `lists.visibility = 'public'` is a topic.** Not because it is cheaper, but because
`migrations/0012_lists.sql:11-13` already says so in a comment written during M1a. (That
comment calls it "the M1c preference center" — it predates this milestone's renumbering, and the
preference center is M1d's. The comment is in a released migration and is left alone.) A second entity
would mean a mapping table, a second name to keep in sync, and two answers to "what is this person
subscribed to". A public list gains nothing but its `description` being shown.

Rejected: a `topics` table with a many-to-many to lists. It buys one thing — a topic spanning
several lists — which no part of the spec asks for.

### 6.2 Tokens: a new scheme, or extend `selection.rs`?

**Decided: extend the existing pattern.** `src/contacts/selection.rs` already issues
domain-separated HMAC tokens through `Secrets::derive_key`, with `issue` and `verify`. M1d adds a
module with the same shape and three new domains. A second scheme would mean two things to audit.

Tokens carry `(workspace_id, contact_id, purpose, issued_at)` and a nonce; nothing public is a
sequential id. **Unsubscribe tokens do not expire** — an email sits in an inbox for years and
RFC 8058 must still work. **Confirmation tokens expire in 14 days.**

**Amended 2026-11-20, during M1d implementation.** This paragraph originally added a third,
90-day preference token. It cannot exist alongside §6.3: that section decides the preference
center *is* `/u/{token}` and that RFC 8058's one-click POST lands on the same token, and one token
cannot carry two purposes. A 90-day preference token would also mean the unsubscribe link in a
two-year-old email either expires — breaking this section's own rule — or is a second token for
the same page. `/u/{token}` parses `Unsubscribe` for both GET and POST, and no preference purpose
is issued or accepted.

### 6.3 One page or three?

**Decided: one page, three doors.** `/u/{token}` is the unsubscribe confirmation *and* the
preference center, because an unsubscribe link that offers "or just change what you get" is the
single highest-leverage retention surface an email product has, and because RFC 8058's one-click
POST must land on the same token.

### 6.4 The confirmation email, with no sender

**Decided: enqueue a `jobs` row, send nothing.** `spec.md` §269 already names `jobs` as the queue.
M1d creates the table and writes the row; M2's pipeline drains it. M1d's tests assert the row
exists with the right payload, and exercise confirmation by using the token directly.

Rejected: a `LoggingMailer` or an SMTP call behind a trait. Both are a sender built before the
milestone that owns senders, and both would be rewritten in M2. A queue row is the real contract.

**Consequence, stated plainly:** at the end of M1d, a real person signing up through a real form
receives nothing. The flow is complete and testable but not usable end-to-end until M2 drains the
queue. This is the honest cost of the milestone order in `spec.md` §11, and it is accepted.

## 7. What gets built

### 7.1 `consent_log` — append-only

One row per consent change, never updated, never deleted:

`id`, `workspace_id`, `contact_id`, `list_id` (nullable: a workspace-wide suppression has no
list), `action` (`requested`, `subscribed`, `confirmed`, `unsubscribed`, `resubscribed`, `suppressed`),
`source` (`form`, `import`, `api`, `operator`, `one_click`, `preference_center`, `bounce`,
`complaint`), `source_ref` (the form id, the import id, the user id — whatever caused it),
`ip` (`inet`, nullable), `user_agent` (nullable), `token_jti` (nullable), `created_at`.

**Amended 2026-11-20, during M1d implementation.** This list originally omitted `requested`.
A membership that lands in `pending` is the subscriber having *asked*, not having *agreed*, and
without `requested` such a row is written as `subscribed` — byte-identical to a real single
opt-in. The only thing then distinguishing them is `lists.double_optin`, which is mutable and
unversioned, so an operator flipping that flag retroactively turns unconfirmed rows into proof of
consent. That is §4's first anti-goal, reached by the very table built to prevent it. A writer
emits `requested` when the resulting status is `pending` and `subscribed` when it is `subscribed`,
so the log answers "did this person agree" without joining anything mutable.

**Every existing writer of `list_memberships.status` gains a consent-log write.** That includes
the importer, the contacts API and the lists API. A status change with no log row is a defect, and
one test asserts it for each writer.

### 7.2 Public endpoints

| Route | Does |
|---|---|
| `GET /c/{token}` | Confirms a pending membership. Idempotent: a second visit says "already confirmed", never errors. |
| `GET /u/{token}` | The unsubscribe + preference page. |
| `POST /u/{token}` | Body `List-Unsubscribe=One-Click` → unsubscribe from everything, 200, no body needed. Any other body → apply the form's checkboxes. |
| `GET /f/{slug}` | The hosted signup page for a form. |
| `POST /f/{slug}` | Submit it. |

All five are unauthenticated, CSRF-exempt by design (they are token-authenticated), rate-limited
per IP, and **must not disclose whether an address exists**: `POST /f/{slug}` answers identically
for a new address, an existing subscriber, and a suppressed address.

### 7.3 Forms

A form belongs to a workspace and names: the lists it subscribes to (at least one), which contact
fields it collects (email always; first name, last name, timezone and any custom field optional,
each required or not), its slug, its redirect URL on success, and its copy.

Double opt-in follows **the list's** `double_optin`, not the form's. A form targeting two lists
where one is double opt-in and one is not produces a `pending` membership on the first and
`subscribed` on the second, from one submission.

Embedding is a plain HTML `<form>` snippet the operator copies, posting to `/f/{slug}`. No
JavaScript is required for it to work.

### 7.4 Admin UI

Forms list and editor; the embed snippet with a copy button; a consent history panel on the
contact detail screen; the public name/description/visibility fields on the list editor.

## 8. Correctness invariants — each needs an automated test

1. **An unsubscribe is never silently reversed.** An import, an API call or a form submission
   naming an address that unsubscribed from a list leaves that membership `unsubscribed` unless
   the person themselves acts through a token. One test per writer.
2. A `pending` membership is never mailable, and `confirmed` is the only transition that makes it
   so. (`is_mailable` already refuses `pending`; the test proves the transition.)
3. One-click unsubscribe takes effect in **under five seconds**, measured end to end, and is
   idempotent.
4. A token signed for one workspace is rejected by every other workspace's data, and a token with
   any byte altered is rejected.
5. A confirmation token used twice confirms once and reports "already confirmed".
6. An expired confirmation token refuses and offers to re-send, without disclosing whether the
   address exists.
7. `POST /f/{slug}` is byte-identical in response for a new, existing and suppressed address.
8. Every `list_memberships.status` write in the tree has a matching `consent_log` row.
9. A form submission that trips the honeypot or the per-IP rate limit answers exactly as a
   successful one does.
10. The preference center shows exactly the workspace's `public` lists, plus any non-public list
    the contact is already on, and nothing else.

## 9. Performance

On the reference box of `spec.md` §9 (8 vCPU, 32 GB):

- `GET /u/{token}` renders in **under 200 ms** at p95.
- `POST /u/{token}` one-click completes in **under 5 s**, per `spec.md` §225.
- Form submission completes in **under 500 ms** at p95, excluding the enqueued job.

## 10. Left to the builder

- The exact token encoding, within §6.2's rules.
- The public pages' markup and styling, within the existing design system and WCAG 2.2 AA.
- Whether `jobs` gains its worker here or in M2 — M1d needs only the row.
- The per-IP rate-limit numbers, with the reasoning recorded as `src/http/rate_limit.rs` does.
- Whether `consent_log` is partitioned now or later; it is append-only and will grow like
  `events`.
