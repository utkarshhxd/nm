# M1a — Contact core and suppressions

Spec for the first third of milestone M1 (Audience). The product spec at
`.scratch/email-platform/spec.md` remains the binding authority; this document decides what it
leaves to the builder and records where it overrides the spec's sketch, with reasons.

Research behind these decisions:
`research/m1a-competitive-contact-model.md`, `research/m1a-postgres-data-model.md`,
`research/m1a-ui-design-system.md`.

---

## 1. Scope

In:

- Contacts: identity, built-in name fields, typed custom field values, timezone.
- Custom field definitions: seven types, archive, purge.
- Lists, list memberships with subscription status, tags.
- Suppressions: workspace-level and instance-wide, address and domain scope.
- The eligibility predicate — the single definition of "may we email this address".
- REST API for all of the above, OpenAPI-described, cursor-paginated.
- UI: contacts table, contact detail, lists, tags, custom fields, suppressions.
- GDPR erase for a single contact.

Out, and which milestone owns it:

- Segment engine, CSV import/export, bulk upsert — **M1b**.
- Double opt-in confirmation flow, public forms, preference center, consent log — **M1c**.
- Campaigns, sending, the send-time eligibility check — **M2**.
- Interspire / listmonk importers — **M6**.

M1a builds the schema those milestones need, and nothing they do not.

---

## 2. Decisions carried in

From M0, unchanged: UUIDv7 primary keys generated in Rust (Postgres 16 has no `uuidv7()`);
`authorize` as the only tenancy gate, 404 before 403; RFC 9457 problem+json; `{data, next_cursor}`
list shape; SPDX header on every file; audit actions as constants, never inline strings; secrets
never in logs, audit metadata or responses.

Settled with the owner this session: **unsubscribe is per list, with a workspace-wide opt-out as
well.**

---

## 3. Domain model

The vocabulary below is authoritative and goes into `GLOSSARY.md`.

**Contact** — a person in one workspace, identified by email address. Has built-in `first_name`
and `last_name`, a timezone, and values for the workspace's custom fields. A contact belongs to
exactly one workspace; the same address in two workspaces is two unrelated contacts.

**Custom field** — a typed, workspace-scoped definition. Values live in the contact's `attributes`
JSONB, keyed by the field's immutable `key`.

**List** — a named set of contacts a campaign can be sent to. Has its own double-opt-in setting
and a visibility that decides whether it appears in the preference center (M1c).

**List membership** — the relationship between a contact and a list, carrying the subscription
status. This is where "unsubscribed from this list" lives.

**Tag** — a label on a contact. Unlike a list, a tag is never a send target and carries no
subscription state. Tags organise; lists receive mail.

**Suppression** — a standing block on an address or a domain. Workspace-scoped or instance-wide.
This is the only thing in the system that can say "never email this, whatever else says
otherwise".

**Eligible** (or *mailable*) — a derived property, never stored: a contact is eligible for a list
when their membership is `subscribed` and no live suppression matches their address or its domain.

---

## 4. The contact has no status column

**Decision.** `contacts` carries no consent or deliverability status. The workspace-wide opt-out is
a `suppressions` row with reason `unsubscribed_all`.

Mailchimp, Interspire and listmonk all fold consent and deliverability into one status on the
subscriber. Klaviyo and Customer.io separate them. Separation wins here for two concrete reasons,
both failure modes observed in the research:

1. **Erasure.** GDPR delete removes the contact row. A workspace-wide opt-out stored on that row
   dies with it, and the erased person can be re-imported and mailed. Keyed on the address in
   `suppressions`, the block outlives the row.
2. **Re-import.** A status column on the contact is whatever the last write set it to. An import
   that touches the contact silently clears the opt-out — the "imports re-subscribed people"
   complaint found against more than one commercial product. Suppressions are keyed by address and
   are never written by a contact upsert.

Consequence: exactly one table can forbid sending, and it is keyed by address rather than by row
identity. Invariant 2 ("no marketing email to a suppressed address, checked at send time") becomes
structurally easier to hold in M2.

---

## 5. `pending` exists only on double-opt-in lists

**Decision.** `list_memberships.status` is `pending | subscribed | unsubscribed`. A membership is
eligible when, and only when, it is `subscribed`.

listmonk's `unconfirmed` still receives mail on single-opt-in lists but not on double-opt-in ones,
so the meaning of a membership row depends on a *different* row's setting. That is a bug waiting to
be written. In NM, adding a member to a list with `double_optin = false` writes `subscribed`
directly; `pending` is only ever **created** on a double-opt-in list, and is never mailable. No
cross-row lookup is needed to know what a status means.

**A pending row may outlive a `double_optin` change, and that is allowed.** An earlier draft of
this rule said pending could only ever *exist* on a double-opt-in list, which forced turning the
flag off to refuse while any pending row remained. On a list with fifty thousand pending members
that makes a boolean unflippable, and with no confirmation flow before M1c those rows cannot be
resolved any other way. The property worth protecting is that `pending` is never mailable and is
never created outside double opt-in; neither needs the stronger claim.

---

## 6. Schema

Migrations numbered from `0010`, style following the existing nine exactly. The implementation plan
decides how many files; nothing here depends on the split.

### 6.1 Extensions

```sql
create extension if not exists pg_trgm;
```

`pg_trgm` is contrib and trusted since Postgres 13, so `create extension` succeeds without
superuser on the Postgres 16+ the spec requires. Documented as an install requirement.

### 6.2 Custom fields

```sql
create table custom_fields (
    id           uuid primary key,
    workspace_id uuid not null references workspaces (id) on delete cascade,
    key          text not null,
    label        text not null,
    kind         text not null check (kind in
                     ('text','number','date','datetime','boolean','select','multi_select')),
    -- select / multi_select only; [] for every other kind.
    options      jsonb not null default '[]'::jsonb,
    archived_at  timestamptz,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),
    unique (workspace_id, key),
    unique (workspace_id, id)
);
```

`key` is immutable once created: it is the JSONB key under which every contact's value is stored,
and renaming it would orphan ten million values. The `label` is what the UI shows and may change
freely.

Three kinds beyond the product spec's four — `datetime`, `select`, `multi_select`. Interspire
stores its checkbox fields PHP-serialized and its date fields in a per-field day/month/year order;
without `multi_select` and `datetime` the M6 importer has nowhere to put them.

### 6.3 Contacts

```sql
create table contacts (
    id           uuid primary key,
    workspace_id uuid not null references workspaces (id) on delete cascade,
    email        text not null,
    email_domain text not null generated always as
                     (substring(email from position('@' in email) + 1)) stored,
    first_name   text not null default '',
    last_name    text not null default '',
    attributes   jsonb not null default '{}'::jsonb,
    timezone     text,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),
    unique (workspace_id, email),
    unique (workspace_id, id)
);

create index contacts_keyset_created on contacts (workspace_id, id desc);
create index contacts_keyset_email   on contacts (workspace_id, email, id);
create index contacts_domain         on contacts (workspace_id, email_domain);
create index contacts_search on contacts using gin
    ((email || ' ' || first_name || ' ' || last_name) gin_trgm_ops);
```

`email` is stored already normalised; the unique constraint is on the plain column. No `citext`
(an extension every self-hoster must then have) and no `lower()` expression index: Postgres
`lower()` and Rust `to_lowercase` disagree on Turkish dotted İ and Greek final sigma, so a
`lower()` index would not agree with the application's own idea of the same address. One
normalisation, performed in Rust, is the single source of truth. See §12.

`first_name` / `last_name` are real columns, not custom fields: every comparable product has them,
merge tags need them, and the trigram index covers them.

**No GIN index on `attributes` in M1a.** Nothing queries custom field values until the segment
engine in M1b, and a GIN index on a JSONB column that every contact update touches destroys HOT
updates for no present benefit. M1b adds the index its compiler actually needs.

### 6.4 Lists, memberships, tags

```sql
create table lists (
    id           uuid primary key,
    workspace_id uuid not null references workspaces (id) on delete cascade,
    name         text not null,
    description  text not null default '',
    double_optin boolean not null default true,
    visibility   text not null default 'private'
                     check (visibility in ('public','private')),
    -- Set by an importer to make re-runs idempotent; null for lists made in the UI.
    source_ref   text,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),
    unique (workspace_id, name),
    unique (workspace_id, source_ref),
    unique (workspace_id, id)
);

create table list_memberships (
    workspace_id    uuid not null,
    contact_id      uuid not null,
    list_id         uuid not null,
    status          text not null check (status in ('pending','subscribed','unsubscribed')),
    subscribed_at   timestamptz,
    unsubscribed_at timestamptz,
    created_at      timestamptz not null default now(),
    primary key (contact_id, list_id),
    foreign key (workspace_id, contact_id)
        references contacts (workspace_id, id) on delete cascade,
    foreign key (workspace_id, list_id)
        references lists (workspace_id, id) on delete cascade
);

create index list_memberships_by_list
    on list_memberships (workspace_id, list_id, status, contact_id);
```

Tags and `contact_tags` follow the same shape: `unique (workspace_id, name)` and
`unique (workspace_id, id)` on `tags`; `contact_tags` keyed `(contact_id, tag_id)` with both
composite foreign keys.

**Composite foreign keys `(workspace_id, id)` are the point.** A membership joining a contact in
workspace A to a list in workspace B is not rejected by a handler — it cannot be written at all.
Tenancy becomes a property of the schema rather than of every query an implementer remembers to
scope.

`lists.visibility` is unused in M1a and exists so M1c's preference center has somewhere to read
"topics" from without a migration. It is the only forward-looking column in this schema.

### 6.5 Suppressions

```sql
create table suppressions (
    id                uuid primary key,
    -- null means instance-wide: it applies to every workspace.
    workspace_id      uuid references workspaces (id) on delete cascade,
    scope             text not null check (scope in ('address','domain')),
    -- Address scope: plaintext, except on an erasure tombstone where it is null.
    email             text,
    -- Domain scope only.
    domain            text,
    -- HMAC-SHA256 of the normalised address. Address scope only; survives erasure.
    email_hmac        bytea,
    reason            text not null check (reason in
                          ('unsubscribed_all','bounced','complained','manual','gdpr_erasure')),
    source            text not null default '',
    note              text,
    created_at        timestamptz not null default now(),
    created_by_user_id uuid references users (id) on delete set null,
    lifted_at         timestamptz,
    lifted_by_user_id uuid references users (id) on delete set null,
    check (case scope
             when 'address' then domain is null     and email_hmac is not null
             when 'domain'  then domain is not null and email_hmac is null and email is null
           end),
    check (reason <> 'gdpr_erasure' or (scope = 'address' and email is null))
);

create unique index suppressions_live
    on suppressions (workspace_id, scope, email_hmac, domain)
    nulls not distinct
    where lifted_at is null;

create index suppressions_lookup_address
    on suppressions (email) where lifted_at is null and scope = 'address';
create index suppressions_lookup_domain
    on suppressions (domain) where lifted_at is null and scope = 'domain';
create index suppressions_lookup_hmac
    on suppressions (email_hmac) where lifted_at is null;
```

**Why plaintext and a hash.** An admin's blocklist is a list of addresses they manage, read and
export — storing only a hash would make the feature unusable. The hash exists for the one row type
where the plaintext must be destroyed: the **GDPR erasure tombstone**, which keeps the block alive
after the address itself has been erased.

**Why HMAC rather than SHA-256.** Email addresses are an enumerable space, so a plain digest of a
suppression list is reversible by dictionary attack — a leaked table would disclose precisely who
complained or asked to be erased. The key is derived from the existing instance secret
(`crypto::secrets`), domain-separated with a constant. This adds no new catastrophic-loss mode:
losing that secret already loses the encrypted relay credentials. **It is a one-way door** — the
hashes cannot be recomputed if the derivation changes — so the derivation string is fixed in M1a
and recorded in the decision record.

Lifting a suppression sets `lifted_at` rather than deleting the row, so the blocklist's history
survives. The unique index is partial on `lifted_at is null`, so an address can be suppressed again
after a lift.

---

## 7. The eligibility predicate

One definition, used by the mailable count in M1a's UI and by the send-time check in M2. It is a
**view**, not a function: a `STABLE` SQL function containing `EXISTS` is not inlined by the
planner, which measured roughly twice as slow.

```sql
create view mailable_list_members as
select m.workspace_id, m.list_id, m.contact_id, c.email, c.first_name, c.last_name
from list_memberships m
join contacts c
  on c.workspace_id = m.workspace_id and c.id = m.contact_id
where m.status = 'subscribed'
  and not exists (select 1 from suppressions s where s.lifted_at is null
                    and s.workspace_id = m.workspace_id
                    and s.scope = 'address' and s.email = c.email)
  and not exists (select 1 from suppressions s where s.lifted_at is null
                    and s.workspace_id is null
                    and s.scope = 'address' and s.email = c.email)
  and not exists (select 1 from suppressions s where s.lifted_at is null
                    and s.workspace_id = m.workspace_id
                    and s.scope = 'domain' and s.domain = c.email_domain)
  and not exists (select 1 from suppressions s where s.lifted_at is null
                    and s.workspace_id is null
                    and s.scope = 'domain' and s.domain = c.email_domain);
```

Four separate `NOT EXISTS` rather than two with `OR` inside: an OR-qualified anti-join cannot use
the indexes and degrades to a sequential scan. Ugly, measured, deliberate — there is a comment at
the view saying so.

Erasure tombstones carry `email is null` and therefore match nothing here. That is correct: an
erased contact has no `contacts` row to be counted. Tombstones are enforced on the *other* path.

**The single-address path.** Creating a contact, or importing one, computes the HMAC in Rust and
checks `suppressions_lookup_hmac`. This path alone sees erasure tombstones, and it is the one that
stops an erased person being silently re-created and mailed.

Both paths are covered by tests that assert a suppressed address is never mailable, for every
reason value and both scopes.

---

## 8. API

All routes under `/api/v1/workspaces/{workspace_id}/…`, every handler calling `authorize` as its
first statement touching workspace data.

| Resource | Routes |
|---|---|
| Contacts | `GET`/`POST` `/contacts`; `GET`/`PATCH`/`DELETE` `/contacts/{id}`; `POST /contacts/{id}/erase` |
| Memberships | `GET`/`POST` `/contacts/{id}/lists`; `DELETE /contacts/{id}/lists/{list_id}` |
| Tags on a contact | `PUT`/`DELETE` `/contacts/{id}/tags/{tag_id}` |
| Custom fields | `GET`/`POST` `/custom-fields`; `PATCH`/`DELETE` `/custom-fields/{id}`; `POST /custom-fields/{id}/purge` |
| Lists | `GET`/`POST` `/lists`; `GET`/`PATCH`/`DELETE` `/lists/{id}`; `GET /lists/{id}/members`; `GET /lists/{id}/mailable-count` |
| Tags | `GET`/`POST` `/tags`; `PATCH`/`DELETE` `/tags/{id}` |
| Suppressions | `GET`/`POST` `/suppressions`; `POST /suppressions/{id}/lift` |
| Bulk | `POST /contacts/bulk/{tag,untag,subscribe,unsubscribe,delete}` |

Instance-wide suppressions live under the existing admin surface and require
`is_instance_admin`.

### 8.1 Sorted pagination

M0's `PageParams` carries a bare UUID cursor and every list orders `id desc`. A contacts table
sorted by email cannot use it. M1a adds `SortedPageParams`: `?sort=created|email&dir=asc|desc`,
with the cursor encoding the composite keyset `(sort_value, id)` rather than an id alone. The
existing `PageParams` is untouched and every M0 endpoint keeps working.

`OFFSET` is not used anywhere. At ten million rows it cannot meet the 150 ms p95 target.

### 8.2 Selection tokens — the part that is a compliance bug if skipped

A contacts table must offer "select all 2.3 million matching". Sending the ids is impossible, and
re-running the filter at action time is **wrong**: a CSV import or a form signup landing between
the click and the action silently joins the selection. In an email product that means messaging
people who were never selected.

So selection is a predicate with a timestamp, not a set:

- `POST /contacts/selection` takes the active filter and returns a token signed with the existing
  `crypto::tokens` HMAC, encoding `{workspace_id, filter, as_of, issued}`. `issued` is the live
  count at that moment: the number the person is about to be shown.
- `as_of` is a UUIDv7 upper bound, so the snapshot is enforced as `id <= $as_of` — free, because
  ids are already time-ordered.
- Bulk endpoints take `{selection_token, excluded_ids}` and re-resolve the predicate under that
  bound.
- A token is rejected if its workspace does not match the authorised one, or if it is older than
  one hour.

The `as_of` bound freezes **creation time only**. The filter itself is re-run against current
state, so a contact that existed at `as_of` and did not match then does match now if someone has
since tagged it, subscribed it or renamed it into the search. `issued` is what catches that:
`bulk/delete` and `bulk/subscribe` refuse with `409 selection-widened` when the match has grown,
inside the transaction, so nothing is written. They are the two that manufacture consent nobody
can obtain retroactively — a delete cannot be undone, and being on a list is what makes someone
mailable, which an unsubscribe afterwards does not un-send. `tag`, `untag` and `unsubscribe` do
not refuse; they return `issued` beside `selected` and the UI reports that the view grew. The
comparison adds `excluded_ids.len()` back to `selected`, because `issued` was counted without
exclusions and `selected` with them.

This bounds how many contacts an action reaches, never which ones: an equal number leaving and
joining the match is invisible to a count. See
`docs/adr/0003-selection-is-a-snapshotted-predicate.md`.

Every bulk endpoint returns the count it actually affected, which may be lower than the count the
UI showed, and the UI reports both.

### 8.3 Bulk operations write one audit row

A bulk unsubscribe of 2 million contacts writes **one** audit row recording the action, the
predicate, the `as_of` bound and the affected count — not two million rows. Per-contact audit at
that volume would make the audit log larger than the contacts table and unreadable for the admin
it exists to serve.

### 8.4 Three semantics an implementer would otherwise have to guess

**Delete is not erase.** `DELETE /contacts/{id}` removes the contact and their memberships and
tags; the address is free to be added again, and nothing is suppressed. `POST /contacts/{id}/erase`
is the GDPR action: it removes the same rows **and** writes an `gdpr_erasure` suppression holding
only the HMAC. The two are deliberately different routes with different roles, because an editor
tidying a typo must not be able to permanently blocklist an address, and an erasure must not be
reversible by re-creating the contact.

**Re-subscribing does not override a suppression.** Adding a suppressed contact to a list succeeds
and writes the membership — the relationship is real, and refusing it would lose information. They
are simply not eligible, and the contact detail page says why, naming the suppression. Making them
mailable again requires an admin to lift the suppression, which is the audited, deliberate act.
The API never silently lifts a suppression as a side effect of any other operation.

**`GET /lists/{id}/mailable-count` is exempt from the 150 ms p95 target**, and is the only endpoint
in M1a that is. The predicate measured 1.4 s warm over 1.43M mailable contacts. The UI therefore
requests it separately from the page's own data and renders a skeleton until it lands; the page
itself stays within the target. An implementer must not "fix" this by denormalising a counter —
Interspire's hand-maintained counters drift, which is one of the things being replaced.

---

## 9. Permissions

| Action | Minimum role |
|---|---|
| Read contacts, lists, tags, fields, suppressions | viewer |
| Create / edit / delete contacts, lists, tags, memberships | editor |
| Add a suppression | editor |
| Bulk operations | editor |
| Create / archive a custom field | admin |
| Purge an archived field's data | admin |
| Lift a suppression | admin |
| Erase a contact (GDPR) | admin |
| Lift a `gdpr_erasure` suppression | **nobody** — the API has no route for it |

Every lift is audited with the suppression's reason in its metadata, so a lifted `complained` row
is distinguishable from a lifted `manual` one in the audit log.

Lifting a suppression re-enables mailing someone who opted out, hard-bounced or complained. It is
the single most dangerous action in this milestone, which is why it is admin-only and why erasure
tombstones have no lift route at all.

---

## 10. Custom fields are better than the incumbents here

Mailchimp caps at 30 fields and cannot change a field's type; Brevo cannot either; Interspire's
delete is permanent. In all three, deleting a field destroys its data with no recovery. NM:

- **Delete archives.** `DELETE /custom-fields/{id}` sets `archived_at`. The field stops appearing
  in forms and the UI; **every contact keeps its value**. The response says so explicitly.
- **Purge is separate, explicit and admin-only.** `POST /custom-fields/{id}/purge` strips the key
  from every contact's `attributes`. It names the field and the affected count in its audit row,
  and refuses unless the field is already archived.
- **Type changes are refused in M1a**, with a problem response naming the deferral rather than a
  generic 422. Converting ten million JSONB values is a background job with a dry run, and that
  machinery arrives with the import pipeline in M1b.
- No field-count cap beyond what validation needs.

---

## 11. UI

Research: `research/m1a-ui-design-system.md`. The shell has **45.1 KB gzipped of headroom**
measured against the 150 KB budget, not estimated.

**This revises product spec §7's mention of Radix primitives.** The seven-package Radix set
measures 44.5 KB gzipped — 98.7% of the entire remaining budget, with DropdownMenu alone at
31.6 KB. `react-aria-components` (38.7), Headless UI (38.9), Ariakit (40.3) and Base UI (65.6) are
rejected on the same ground. Primitives are hand-built on native elements: `<dialog showModal()>`
gives a focus trap, `inert`, Escape handling and top-layer stacking for free.

**M1a adds exactly one dependency:** `@tanstack/react-virtual@3.14.26` (MIT, 10.2 KB gzipped),
loaded in the contacts route chunk, never the shell.

**Tokens** are OKLCH, light and dark, every pair contrast-verified and gamut-checked. They must be
declared through `@theme inline` over `:root` custom properties — with plain `@theme`, Tailwind v4
silently loses dark mode. One theme mechanism only: a `data-theme` attribute set before paint, not
`prefers-color-scheme` plus a class.

**The contacts table** is a real `<table>` with two spacer rows, so sticky `<thead>` and table
semantics survive virtualisation. Data comes from `useInfiniteQuery` with `maxPages: 20`, bounding
the browser at 2,000 rows however far someone scrolls. Accessibility: `role="grid"` with row-level
roving tabindex (one tab stop for the whole grid), `aria-rowindex` on every rendered row, and
`aria-rowcount="-1"` whenever the total is a planner estimate rather than a real count — otherwise
a screen reader announces "row 3 of 30" over a million results.

Pages: contacts table, contact detail, lists, list detail with mailable count, tags, custom fields
in settings, suppressions. Each with designed empty, loading and error states.

No webfont. A self-hosted install should make zero third-party requests.

---

## 12. A latent M0 bug this milestone must fix first

`users::normalize_email` lowercases with Rust's `to_lowercase` and performs no Unicode
normalisation. Postgres `lower()` disagrees with it on Turkish dotted İ and Greek final sigma, and
without NFC the same address in two Unicode forms is two different strings.

Today that is a minor duplicate-account edge case. The moment suppressions exist it is a
**suppression bypass**: the same address in a different normalisation form hashes differently,
misses the blocklist and gets mailed. It is therefore the first task of M1a, not a later tidy-up,
and it is fixed in the shared `normalize_email` so users and contacts cannot drift apart.

---

## 13. Deviations from `spec.md`, recorded

| Spec says | M1a does | Why |
|---|---|---|
| `contacts` has a `status` | No status column; opt-out is a suppression | §4 — survives erasure and re-import |
| Membership status `subscribed / unconfirmed / unsubscribed` | `pending / subscribed / unsubscribed` | §5 — status meaning must not depend on another row |
| Custom field types: text, number, date, boolean | Adds `datetime`, `select`, `multi_select` | Interspire's serialized checkbox and ordered date fields need them |
| §7 suggests Radix primitives | Hand-built on native elements | §11 — Radix is 98.7% of the remaining JS budget |
| §10 "unsubscribes become suppressions" | Per-list unsubscribes map to memberships; **bounces** map to suppressions | A bounce is a property of an address; an unsubscribe is a property of a relationship. Collapsing them over-suppresses irreversibly, against the owner's per-list decision |
| "Subscription topics" named, no table | A topic is a `list` with `visibility = 'public'` | Avoids a second nearly-identical table |

---

## 14. Testing

- `#[sqlx::test]` per endpoint, per-test database, as M0.
- **A cross-tenant 404 test for every new route.** No exceptions.
- A composite-FK test proving a cross-workspace membership cannot be inserted at all.
- Eligibility: a test per suppression reason and per scope asserting the address is never mailable,
  through both the view path and the single-address path.
- An erasure test: erase, then attempt to re-create the contact, and assert it is blocked.
- Validation tests per custom field type, including rejecting a value for an archived field.
- Selection-token tests: wrong workspace, expired token, and a contact created *after* `as_of`
  proven absent from the resolved set.
- Playwright: contacts CRUD, select-all-matching with an exclusion, suppression add and lift.
- The `break` harness pass over the new components before sign-off — the UI research was not
  visually verified, by its own admission.

## 15. CI

Two additions to the existing gates:

- `check-bundle.mjs` gains a **per-lazy-route-chunk ceiling of 50 KB gzipped**. Today only the
  shell is measured, and contacts is precisely the route that will grow.
- An INP assertion on the contacts route, against the 200 ms target.

## 16. Open, non-blocking

1. **Legal:** keeping an HMAC of an erased address is standard practice and generally defended
   under GDPR Article 17(3), but it has not been reviewed by counsel. The design works without it;
   the cost of dropping it is that an erased person can be re-imported and mailed.
2. **Trigram search at 10M rows** is measured at 1–30 ms on 2.1M and extrapolated beyond that.
   Expect 100–250 ms for mid-density terms. If a 10M benchmark misses the 150 ms p95, the fallback
   is a `text_pattern_ops` prefix index, which is a migration and not a redesign.
3. **Whether a campaign may target a segment with no list subscription** is an M2 question. M1a's
   schema supports either answer.
