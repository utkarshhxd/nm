# M1a competitive research: the contact / audience model

Scope: how Mailchimp, Klaviyo, Brevo, listmonk, Interspire 6.x (and briefly Kit, Customer.io, Resend) model contacts, lists, tags, subscription status, custom fields, suppression and consent. Aimed at the gaps in `spec.md` sections 6 and 7 (data-model sketch), not at what is already decided.

Evidence labels used throughout:

- **[V]** verified from official docs/API reference, or from source code/schema I read directly.
- **[C]** community, forum or vendor report, or a search-engine summary of a page I could not fetch. Real, but not an official statement.
- **[I]** my inference or recommendation. Treat as opinion.

Interspire facts marked [V-src] come from the actual 6.2.0 source in a local read-only copy of the 6.2.0 `admin` tree, which is more reliable than its docs. listmonk facts marked [V-src] come from `schema.sql` and `queries/*.sql` on master, fetched 2026-11-11.

---

## 0. What the NM spec says today (baseline)

From `spec.md` section 6 and section 7 "Data model":

- `contacts` (email, case-insensitive unique per workspace; **status**; `attributes` JSONB; timezone), `custom_fields` (types text/number/date/boolean).
- `lists`, `list_memberships` (subscribed / unconfirmed / unsubscribed + timestamps), `tags`, `contact_tags`, `segments` (JSON rules).
- `suppressions` (workspace or instance level, reason, source, timestamp), `consent_log`.
- Preference centre "with subscription topics" is a listed feature, but **no `topics` table exists in the data-model sketch**.
- Invariant 2: no marketing email to a suppressed address (unsubscribed, hard-bounced, complained, instance-wide list), checked at send time.
- Interspire import: unsubscribes and bounces become suppressions (mandatory).

---

## 1. Comparison table

| Dimension | Mailchimp | Klaviyo | Brevo | listmonk | Interspire 6.2 | NM spec today |
|---|---|---|---|---|---|---|
| Contact unit | One row per (audience, email); same email in 2 audiences = 2 contacts, billed twice | One global profile per account; `id` is the key | One global contact; `id` + optional `ext_id`; email optional | One global subscriber; serial id + uuid; `UNIQUE(LOWER(email))` | One row **per (list, email)**; no global contact | One per (workspace, email) |
| Status values | subscribed, unsubscribed, non-subscribed, cleaned, pending, transactional, archived | Per channel: SUBSCRIBED / UNSUBSCRIBED / NEVER_SUBSCRIBED, plus separate suppression | `emailBlacklisted` bool; `listIds`, `listUnsubscribed` | Subscriber: enabled / disabled / blocklisted. Subscription: unconfirmed / confirmed / unsubscribed | `confirmed` char, `unsubscribed` int ts, `bounced` int ts, on the list row | contact `status` (undefined); membership subscribed / unconfirmed / unsubscribed |
| Where status lives | On the audience-member row (mixes consent and deliverability) | Consent on profile per channel; list membership separate; suppression separate | Contact-level blocklist + per-list unsub | Both: subscriber-level blocklist and per-list subscription | Only on the per-list row | Both (unspecified) |
| Lists | "Audience" = hard data/billing boundary; recommended: one | Static lists; do not imply consent | Many lists | public/private/temporary; single/double opt-in | Lists = separate subscriber sets | Lists |
| Tags | Internal labels (groups = subscriber-visible) | None (properties + segments) | None | **None on subscribers** (tags exist on lists/campaigns only) | None | Tags |
| Segments | Saved dynamic filters | Dynamic, condition-based; no manual members | Dynamic segments | Raw SQL expression typed by admin | `segments.searchinfo` serialized rule blob | JSON rules, live counts |
| Custom field types | text, number, address, phone, date, url, imageurl, radio, dropdown, birthday, zip (+ checkbox, website, image in UI) | Schema-less; string/int/float/date/bool/list/JSON; type picked per segment condition | text, date, float, boolean, id, category, multiple-choice | Schema-less JSONB, no types | text, textarea, numbers-only, dropdown, checkbox (multi), radiobutton, date | text, number, date, boolean |
| Field limits | 30 per contact (80 Premium); value 255 bytes; label 50 bytes | No count cap; 100 KB request body | Names alnum + underscore [C]; option text 200 chars | None | None documented; email `varchar(200)` | none stated |
| Delete / type change | Delete destroys data; type cannot change | n/a (no schema) | Delete destroys data; name and type cannot change [C] | n/a | Delete permanent; encryption flag irreversible | not stated |
| Suppression model | Folded into status (`cleaned`, unsubscribed, archived) | Separate object; reasons HARD_BOUNCE, INVALID_EMAIL, SPAM_COMPLAINT, UNSUBSCRIBE, USER_SUPPRESSED | "Blocklist"; reason codes unsubscribedViaMA, unsubscribedViaEmail, adminBlocked, unsubscribedViaApi, hardBounce, contactFlaggedAsSpam | `blocklisted` is a **status on the subscriber row**; bounce actions none/unsubscribe/blocklist/delete | `banned_emails` (address, domain or `@domain`; list id or `g`; ban date only) **plus** unsub/bounce as list-row state | `suppressions` w/ reason, source, timestamp |
| Suppression scope | Per audience | Account-wide for email; list-level unsub separate | Global for marketing email [C] | One instance | Per list or global (`g`) | Workspace or instance |
| Undo | Self-resubscribe via form only; API blocked for unsub/deleted | Owner/Admin/Manager; deliverability suppressions stay; 90-day lockout on re-suppress | Contact page only, one at a time [C] | Admin sets enabled | Admin edits status to Active | not stated |
| DOI state | `pending` | Pending until confirmed; `double_optin` flag | not verified | `unconfirmed` (meaning depends on list type) | `confirmed='0'` until link clicked | `unconfirmed` |
| Consent proof | ip_signup, timestamp_signup, ip_opt, timestamp_opt, source, marketing_permissions | consent_timestamp, method, method_detail, custom_method_detail, double_optin | not verified | None by default; `privacy.record_optin_ip` default **false** | requestdate/ip, confirmdate/ip, subscribedate, formid | `consent_log` (fields unspecified) |
| Identity change | `id` = MD5(lowercase email); separate stable `contact_id` | Stable `id`; email is a property | `id`, `ext_id` | serial id; email editable | per-list `subscriberid`; email repeated in each row | email unique; id unspecified |

---

## 2. Q1: Lists vs tags vs segments

### Findings

**Mailchimp** [V]
- Audience = the container. Tags = "built for your own, internal audience organization". Groups = subscriber-chosen interests (shown on the signup form). Segments = filters. https://mailchimp.com/help/getting-started-tags/
- Its "one audience" article gives these reasons: single source of truth, easier targeting with tags/segments, one send instead of duplicated content, trend spotting, data not siloed, one engagement history for re-engage/archive decisions. It names exceptions: B2B vs B2C tracked separately, internal vs external contacts, groups you would never message the same way. https://mailchimp.com/resources/one-audience-organize-contacts-to-optimize-marketing/
- **Correction to the brief's premise:** Mailchimp did not remove multiple audiences; it *recommends* one. That article does **not** mention billing. The billing reason is documented elsewhere: the bill is the total contacts across all audiences (excluding archived/cleaned), and the same address in 3 audiences is 3 contacts. [V for the rule via Intuit-hosted Mailchimp help; C for the "charged three times" example.] https://quickbooks.intuit.com/learn-support/en-us/help-article/email-campaign/requirements-best-practices-audiences/L8TtT0EMy_US_en_US
- An unsubscribe in one audience does not touch the same address in another ("Mailchimp treats all audiences in your account independently") [V via Intuit help]. Users syncing two audiences fight this: https://community.zapier.com/troubleshooting-99/managing-contact-synchronization-between-nexudus-and-mailchimp-in-zapier-51884 [C]

**Klaviyo** [V]
- Lists are static (people join via forms, import, manually). Segments are condition-driven and cannot have manual or imported members. A segment snapshot becomes a static list. https://help.klaviyo.com/hc/en-us/articles/115005061447
- Explicit footgun: "Adding contacts to a list does not itself establish permission to send marketing messages", and segments "can include profiles that haven't opted in" unless you add a can-receive-marketing condition. https://help.klaviyo.com/hc/en-us/articles/360002057172
- No tags. Properties + segments do the job.

**Brevo** [C]: lists plus attributes; no tags. A community user running two brands on one account found an unsubscribe from List A removed them from List B, and support offered no workaround ("this would be a dealbreaker"). The explanation came from a community member, but the poster says support replies matched the docs. https://community.brevo.com/t/how-to-unsubscribe-from-a-specific-list-and-not-all-lists-or-be-blocklisted/1497

**listmonk** [V-src]: `lists.type` public/private/temporary, `lists.optin` single/double. Subscribers have **no tags**; `tags` columns exist only on lists, campaigns and one other table. Segments = a raw Postgres expression the admin types (`subscribers.attribs->>'x'` with manual casts). The docs do not say segments are saved or dynamic. https://listmonk.app/docs/querying-and-segmentation/ . A "visual SQL query builder" proposal (#2406) was closed as stale. https://github.com/knadh/listmonk/issues/2406

**Resend** [V]: moved from per-"Audience" contacts to **global contacts** ("Each email address is treated as a single Contact across your team"), with **Segments** for internal grouping and **Topics** for user-facing preferences. Previously an unsubscribe applied only to the audience used in that broadcast. The page does not state a reason. https://resend.com/docs/dashboard/contacts/migrating-from-audiences-to-segments

**Customer.io** [V]: a global `unsubscribed` master switch plus per-topic and per-channel preferences; a message sends only if the person is subscribed to both its topic and its channel. https://docs.customer.io/messaging/channels/subscriptions

**Interspire 6.2** [V-src]: no global contact. `list_subscribers` has a row per (listid, email) with `UNIQUE(emailaddress, listid)`. Custom-field data hangs off that per-list row (`subscribers_data(subscriberid, fieldid, data)`). Same person on 5 lists = 5 rows, 5 independent unsub states, 5 independent field-value sets. Counters (`lists.subscribecount/unsubscribecount/bouncecount`) are hand-maintained in PHP on every change. No tags.

### Where users get confused
1. List vs segment: people expect a list to equal consent. Klaviyo had to document that it does not. [V]
2. Tag vs group vs segment: Mailchimp has three overlapping things. [V]
3. Duplicate contacts across lists/audiences with divergent unsubscribe state: Mailchimp, Brevo multi-brand, Interspire by construction. [V/C]
4. "Unsubscribed from this list" vs "unsubscribed from everything" being decided by the platform instead of the sender (Brevo). [C]

### Model that causes the fewest problems [I]
The industry is converging (Resend, Klaviyo, Customer.io, listmonk) on: **one global contact**, **lists = membership with consent state**, **tags = internal free-form labels**, **segments = computed only**, **topics = subscriber-visible preferences**. Each concept has one job.

Does Mailchimp's reasoning apply to NM's multi-brand case? Split it:
- *Within a brand/workspace*: yes. One record, one history, one unsubscribe truth. This is exactly the Interspire problem NM is replacing.
- *Across brands*: no. Mailchimp's own exceptions (no message crossover; track separately) describe brands. NM's workspace is a hard boundary and consent is given to a brand, so the same address in two workspaces correctly means two contacts. The billing rationale does not exist when self-hosted.

---

## 3. Q2: Subscription status values

| Product | Exact values and meaning | Source |
|---|---|---|
| Mailchimp | **subscribed** (opted in; may get marketing; counts to limit), **unsubscribed** (opted out; counts), **non-subscribed** (interacted, never opted in; counts; no marketing), **cleaned** (hard bounce or repeated soft; counts nothing; can receive nothing), **archived** (removed, data kept, uneditable; counts nothing), **pending** (API enum; awaiting confirmation; API page gives no definition), **transactional** (API enum; undefined; the help article treats it as a message class, not a contact status) | https://mailchimp.com/help/about-your-contacts/ ; https://mailchimp.com/developer/marketing/api/list-members/ |
| Klaviyo | Consent per channel: **SUBSCRIBED**, **UNSUBSCRIBED**, **NEVER_SUBSCRIBED** (has details, never explicitly opted in, e.g. checkout). Suppression is separate (section 5). Consent not shown until DOI confirmed. | https://developers.klaviyo.com/en/docs/collect_email_and_sms_consent_via_api |
| Brevo | `emailBlacklisted`, `smsBlacklisted` booleans per channel; `listIds` and `listUnsubscribed` arrays on the contact | https://developers.brevo.com/reference/getcontactinfo-1 |
| listmonk | Subscriber: **enabled / disabled / blocklisted**. Subscription (per list): **unconfirmed** ("added directly without explicit confirmation, but still receives single opt-in campaigns"), **confirmed**, **unsubscribed**. Blocklisting forces all subscriptions to `unsubscribed`; a blocklisted subscriber added or imported always gets `unsubscribed` rows. | https://listmonk.app/docs/concepts/ ; `schema.sql`; `queries/subscribers.sql` (`insert-subscriber`, `upsert-subscriber`, `blocklist-subscribers`) |
| Kit | `state`: **active, inactive, bounced, complained, cancelled** (+ "cold" as an engagement subset). Cancelled = unsubscribed (can re-subscribe). Complained = auto-unsubscribed. Bounced = hard bounces only. Bounced and complained are not billed. | https://developers.kit.com/api-reference/subscribers/list-subscribers [V for enum]; https://help.kit.com/en/articles/2502651-the-subscriber-profile-page-and-status [C for meanings] |
| Customer.io | `unsubscribed` boolean (master switch) + `cio_subscription_preferences.topics.topic_<id>` booleans; opt-in topics have no attribute until chosen | https://docs.customer.io/messaging/channels/subscriptions |
| Resend | `unsubscribed` boolean on a global contact + topics | https://resend.com/docs/dashboard/audiences/contacts |
| Interspire | No enum. Derived from columns: `confirmed` ('0'/'1'), `unsubscribed` (unix ts, 0 = no), `bounced` (unix ts, 0 = no), `unsubscribeconfirmed`. One row can be unconfirmed, bounced and unsubscribed at once. | [V-src] `admin/com/install/schema.mysql.php`; `admin/functions/api/subscribers.php` (`UnsubscribeSubscriber`, `BounceSubscriber`, `ActivateSubscriber`) |

### Where status lives
- Mailchimp, Interspire, Kit: on the audience-member row, so one status mixes consent and deliverability.
- Klaviyo, Customer.io, Resend: consent at contact level (+ list/topic overlay); deliverability separate.
- listmonk: both levels, with a one-way coupling (blocklist implies unsubscribe all).

### Observations [I]
- Three different questions get answered by one "status" in most products: (a) did they consent or withdraw? (b) is the address reachable? (c) what do they want to hear about? Klaviyo and Customer.io separate these best.
- `unconfirmed` in listmonk (and in the NM spec) is **context-dependent**: on a single opt-in list it still receives mail, on a double opt-in list it does not. A status whose eligibility depends on another row's setting invites bugs. Use a distinct `pending` and record how consent was obtained separately.
- "Non-subscribed" / NEVER_SUBSCRIBED exists because contacts also arrive from checkout or API without marketing consent. NM covers this implicitly (a contact with no subscribed membership); the spec should say so.

---

## 4. Q3: Custom fields

### Types
- **Mailchimp** (API): `text, number, address, phone, date, url, imageurl, radio, dropdown, birthday, zip`; options `choices`, `date_format`, `default_country`, `phone_format`, `size`; flags `required`, `public` (show on signup form). https://mailchimp.com/developer/marketing/api/lists/merge-fields/create-merge-field.md . UI adds check boxes (act as groups, max 60 per audience), website, image, language. https://mailchimp.com/help/manage-audience-signup-form-fields/
- **Klaviyo**: schema-less properties; docs list strings, integers, floats, dates, booleans, arrays/lists, JSON. Type is chosen **per segment condition**, not stored on the property: "This setting determines how Klaviyo compares the value in the condition." The docs do not say what happens if one profile holds "5" and another 5. https://help.klaviyo.com/hc/en-us/articles/115005237648 ; https://help.klaviyo.com/hc/en-us/articles/115000250912 . Only simple lists are segmentable.
- **Brevo**: categories `normal, transactional, category, calculated, global`; types text, date, float, boolean, id (transactional only), category (enumeration of int value + label), multiple-choice (`multiCategoryOptions`); each option max 200 chars. https://developers.brevo.com/reference/createattribute-1
- **listmonk**: none. Free-form JSON in `subscribers.attribs`; casts in every query. [V-src]
- **Interspire** [V-src + docs]: text (min/max length, default), textarea (rows/cols), number ("Numbers only", min/max length, default), dropdown ("pick list"), checkbox (multi-select; values stored **PHP-serialized** in `subscribers_data.data`), radiobutton, date (three-part day/month/year with configurable display order and start/end year; stored as a slash-separated string in the field's own order). Flags: `required`, `defaultvalue` (optionally applied to existing contacts), `isglobal`, per-list association via `customfield_lists`. Encryption on text/textarea/number only, irreversible, limits segment rules to equals / not-equals. https://www.interspire.com/docs/ug/custom-fields

### Limits and name rules
- Mailchimp: **30** fields per contact (**80** Premium); value **255 bytes**; label **50 bytes** [V]. Merge tags reportedly 10 characters, trimmed and uppercased downstream [C, single source].
- Klaviyo: no cap on property count; 100 KB create-profile request body [V-search/C]. Custom *objects* (a separate feature): 15 to 30 properties, 8 KB per record [C-search].
- Brevo: attribute names alphanumeric + underscore (error text in a community thread) [C]; maximum count not found.
- Interspire: none documented. Email is `varchar(200)` while RFC allows 254 [V-src].

### Delete and type-change behaviour
- Mailchimp: "Deleting a field ... also deletes all data collected from that field"; "You can't change a field's type after you add it ... delete the field and create a new one, which means the existing data is lost." Its offered workaround is hiding the field. https://mailchimp.com/help/manage-audience-signup-form-fields/ [V]
- Brevo: attribute name and type cannot be changed; delete loses all data, not restorable. https://help.brevo.com/hc/en-us/articles/10617359589906 [C: official help article, but I only saw a search summary because direct fetches returned 403]
- Interspire: delete is permanent ("The field cannot be retrieved"). [V]
- Klaviyo, listmonk: no schema, so no migration problem, and no validation either.

### Observation [I]
Two camps: strict-but-brittle (Mailchimp, Brevo, Interspire: typed, but the type cannot evolve and deletion destroys data) and schema-less (Klaviyo, listmonk: flexible, but types are enforced only at query time, so bad data shows up as broken segments). None offers **typed, evolvable and non-destructive**. That is NM's opening.

---

## 5. Q4: Suppression

### How each models "never email this address"
- **Mailchimp**: `cleaned` (deliverability) and `unsubscribed` are values of the one status, **per audience**. A permanently deleted contact cannot be re-added by anyone except via a hosted signup form: help page says "A deleted contact can't be added back to your audience" [V] https://mailchimp.com/help/delete-contacts/ ; the API surfaces this as 400 "Forgotten Email Not Subscribed" [C]. `unsubscribe_reason` exists on the member [V].
- **Klaviyo** (most explicit) [V]: https://help.klaviyo.com/hc/en-us/articles/115005246108
  - Suppression is independent of consent. Reasons: HARD_BOUNCE, INVALID_EMAIL, SPAM_COMPLAINT, UNSUBSCRIBE, USER_SUPPRESSED (plus "synced from another provider", carried over).
  - Soft bounce: suppressed after **7 consecutive** soft bounces counted over 2 years.
  - With several reasons only the highest-priority one shows; deliverability beats manual.
  - Only Owner/Admin/Manager can suppress. A manual suppression can be removed; after unsuppressing you cannot manually re-suppress for 90 days (first 30 days of an account exempt).
  - Resubscribe requires "explicit consent", lifts manual global suppression and that list's suppression, but **deliverability suppressions such as hard bounce stay**.
  - Suppression does not apply to SMS; changes only by email identifier.
- **Brevo**: "blocklist" per channel. The transactional blocked-contacts API returns `reason.code` in `unsubscribedViaMA, unsubscribedViaEmail, adminBlocked, unsubscribedViaApi, hardBounce, contactFlaggedAsSpam`; docs say compare `code`, not `message`. https://developers.brevo.com/reference/get-transac-blocked-contacts [V]. Unblock or resubscribe only from the contact detail page, not in bulk; Brevo warns that unblocking someone who asked to be blocklisted is illegal and can lead to account suspension. [C: search summary of help article]. An unsubscribed contact can display as "blocklisted" in the UI [C].
- **listmonk** [V-src]: blocklist is `subscribers.status='blocklisted'`.
  - A suppression cannot exist without its subscriber row. Deleting the subscriber deletes the suppression [I from schema]. A `delete-blocklisted-subscribers` query deletes every blocklisted row [V-src].
  - The block itself has no reason, source or actor (only `updated_at`). The `bounces` table has type (soft/hard/complaint), source and campaign. What happens on a bounce is a setting: action none/unsubscribe/blocklist/delete plus count threshold. Recommended SES setup: soft threshold 2 = none, hard threshold 1 = blocklist, complaint threshold 1 = blocklist. https://listmonk.app/docs/bounces/ [V]
  - Mailbox-bounce hard/soft classification is a keyword heuristic that defaults to soft [V docs].
- **Interspire** [V-src]:
  - `banned_emails(banid, emailaddress, list, bandate)`. `emailaddress` can be a full address, a bare domain, or `@domain`. `list` is a list id or `'g'` (global). **No reason, source or actor**; only the ban date.
  - Bans are enforced at send time by deleting matching recipients from the send queue; the contact's status is untouched.
  - Unsubscribes and bounces are *not* in `banned_emails`. They are timestamps on the list row, so "never email" lives in two mechanisms.
  - `list_subscribers_unsubscribe` keeps only the latest request: the old row is `DELETE`d and re-inserted on each unsubscribe. `unsubscribearea` is a one-character code (first letter of stats type), not a reason.
  - Bounce detail is in `list_subscriber_bounces(bouncetype, bouncerule, bouncemessage)`; code uses `hard`, `soft` and `unknown`.
  - Import skips anyone already unsubscribed or banned and lists them separately in the results (`unsubscribedemails`, `bannedemails`). This is the *safest* import behaviour of any product studied.
- **Kit**: complained subscribers are auto-unsubscribed and cannot be emailed; only hard bounces produce `bounced`; neither is billed. [C]

### Scope
Account-wide for address-level facts: Klaviyo email suppression, Brevo global marketing blocklist, Kit. Per audience: Mailchimp. Per list or `g`: Interspire.

### Who can undo
Klaviyo: Owner/Admin/Manager, and never deliverability reasons via resubscribe [V]. Brevo: one at a time from the contact page [C]. Mailchimp: only the person via hosted form for self-unsubscribes; admin-bulk-unsubscribed contacts can be resubscribed manually [C]. listmonk: admin, no friction. Interspire: edit status to Active, no friction [V].

### Observations [I]
- Best practice visible in Klaviyo: **reason-typed, priority-ordered, sticky deliverability reasons, role-gated undo, separate from consent**.
- Worst: listmonk and Interspire, where suppression has no provenance (nobody can answer "why is this blocked and who unblocked it?"), and listmonk, where erasure kills the block.
- Interspire's domain-level ban (`@domain`) is a real capability that competitors lack as a first-class concept. Keep it.

---

## 6. Q5: Double opt-in and consent proof

| Product | Pending representation | Proof recorded |
|---|---|---|
| Mailchimp | status `pending` [V] | `ip_signup`, `timestamp_signup`, `ip_opt`, `timestamp_opt`, `source`, `marketing_permissions` (id, enabled, text) on the member [V] https://mailchimp.com/developer/marketing/api/list-members/get-member-info/ |
| Klaviyo | A profile may exist before confirmation but is not consented: "Having a profile does not mean they have joined the list or confirmed marketing consent". Subscribe-API double opt-in creates no profile until confirmed. A list's opt-in setting overrides the account default. [V] https://help.klaviyo.com/hc/en-us/articles/115005251108 | `consent_timestamp`, `last_updated`, `method`, `method_detail`, `custom_method_detail`, `double_optin` (true only if the confirmation message was received and confirmed) [V]. The help article says nothing else about the confirmation is recorded. |
| listmonk | `unconfirmed` subscription on a double opt-in list gets no campaign mail; on single opt-in lists it does [V]. A `delete-unconfirmed-subscriptions` query exists [V-src]. | Nothing by default. `privacy.record_optin_ip` defaults to `false`; `subscriber_lists` has `created_at`, `updated_at` and a `meta` JSONB. No consent log table. [V-src] |
| Interspire | `confirmed='0'` until link clicked; FAQ suggests manually filtering unconfirmed contacts after several weeks and deleting them [V] https://www.interspire.com/subscribers/ | `requestdate`, `requestip`, `confirmdate`, `confirmip`, `subscribedate`, `formid` per list row [V-src] |

**What none documents:** the exact consent *wording/version* shown at the time, and an append-only history. Mailchimp's `marketing_permissions.text` is the closest. Absence from docs is not proof of absence.

---

## 7. Q6: Identity, re-subscribe, import duplicates

### Identity
- Mailchimp: member `id` = MD5(lowercase email), so changing the email changes the URL key. A separate stable `contact_id` "doesn't depend on the contact having an email address. Mailchimp may add contacts without email addresses in the future". `unique_email_id` is cross-account. [V] https://mailchimp.com/developer/marketing/api/list-members/
- Klaviyo: `id` is the only identifier described as unique; email, phone and `external_id` are properties. Profiles can use an email or a customer id as primary identifier. [V] https://developers.klaviyo.com/en/reference/get_profiles
- Brevo: numeric `id`, optional `ext_id`; email "mandatory if ext_id and SMS not passed". `updateEnabled` defaults to **false**, so creating an existing email errors unless you opt in to upsert. [V] https://developers.brevo.com/reference/createcontact
- listmonk: serial `id` + `uuid` (used in public URLs) + `UNIQUE(LOWER(email))`; `name` is `NOT NULL`. [V-src]
- Interspire: per-list `subscriberid`; email `varchar(200)`; uniqueness `(emailaddress, listid)` relies on a case-insensitive MySQL collation (comment in `GetCIOp()`). [V-src]

### Re-subscribe after unsubscribe
- Mailchimp: a self-unsubscribed contact "needs to sign up again on your Mailchimp-hosted signup form"; admin-bulk-unsubscribed contacts can be manually resubscribed; reconfirmation email recommended. https://mailchimp.com/help/resubscribe-a-contact [C: search summary]. Third-party forms are reported to silently fail to change status [C].
- Klaviyo: needs explicit consent; the "Resubscribe" action lifts manual suppression but not deliverability suppressions [V].
- Brevo: contact detail page, one at a time [C].
- listmonk: public form re-subscribes; a regression (#2976) briefly blocked re-subscribe on both list types and was fixed the same day. https://github.com/knadh/listmonk/issues/2976 [V]
- Interspire: admin edits status to Active; a re-import skips them [V-src].

### Duplicate resolution on import
- Mailchimp: upsert by hash; `status_if_new` applies only to new addresses; `skip_merge_validation` available. https://mailchimp.com/developer/marketing/api/list-members/add-or-update-list-member/ [V]
- Klaviyo: import has the option "update subscription status for all imported contacts to subscribed", which Klaviyo staff say is what re-subscribes previously unsubscribed people. Users report unsubscribes being overridden by re-imports and want suppressions to persist "as they did with their previous tool". https://community.klaviyo.com/marketing-30/importing-a-new-list-overrides-the-existing-suppressed-unsubscribed-profiles-8497 [C]
- listmonk: `upsert-subscriber` has **two independent flags**: overwrite name/attribs, and overwrite subscription status. A blocklisted subscriber always yields `unsubscribed` rows. [V-src]
- Interspire: duplicate means skip (counted) or, with Overwrite, update email/list/custom fields; already-unsubscribed means skipped and reported; banned means skipped and reported. [V-src `functions/subscribers_import.php`]

---

## 8. Q7: What they get wrong

**Mailchimp**
1. Field type immutable; deletion destroys data; 30-field cap. [V]
2. One status per audience mixes consent and deliverability (`cleaned`); per-audience duplication means unsubscribes do not propagate. [V via Intuit help; C]
3. Unsubscribed contacts count toward the bill; the same address in N audiences is N contacts. [V rule; C example]
4. Permanent delete blocks re-add, surfaced through an opaque API error ("Forgotten Email Not Subscribed"). [C]
5. `transactional` is an API status value while the help article says it is not a contact status. [V inconsistency]

**Klaviyo**
1. List membership is not consent, and segments include non-consented profiles by default. Safe by documentation, error-prone by default. [V]
2. Imports can silently re-subscribe previously unsubscribed people via an option. [C]
3. Data type is a property of the segment condition, not of the data. [V]
4. The 90-day lockout on manual re-suppression after an unsuppress is odd. [V]

**Brevo**
1. Unsubscribe from one list means unsubscribe from all marketing; multi-brand accounts cannot isolate brands. [C]
2. Attribute name and type immutable; delete = data loss. [C]
3. Blocklist labelling blurs unsubscribe and block; unblock is one at a time. [C]

**listmonk** (closest comparable)
1. No typed fields, no validation, no field registry. Every segment needs hand casts. [V]
2. Segments are raw SQL typed by admins. No safe builder; the builder proposal went stale. [V]
3. Suppression has no reason/source/actor and lives on the row that deletion removes. [V-src; deletion consequence is I]
4. `unconfirmed` means different things depending on list setting. [V]
5. `PUT /api/subscribers/{id}`: "All parameters must be set, if not, the subscriber will be removed from all previously assigned lists." https://listmonk.app/docs/apis/subscribers/ [V]
6. No consent log; IP capture off by default. [V-src]
7. Single-tenant, no workspaces. [I, not verified this session]

**Interspire** (what NM migrates from)
1. Per-list duplication; no global contact. [V-src]
2. Denormalised counters (`subscribecount`, `unsubscribecount`, `bouncecount`) maintained by hand, so they drift. [V-src]
3. Suppression in two mechanisms; ban list has no reason/source/actor. [V-src]
4. Unsubscribe history overwritten on each re-unsubscribe. [V-src]
5. Email `varchar(200)`; times are unix ints in a server-configured timezone; no `updated_at` on subscribers; custom dates stored in per-field configurable order; checkboxes PHP-serialized; encryption irreversible. [V-src]

---

## 9. What NM should do

Everything in this section is [I] (my recommendation), resting on the evidence above.

### 9.1 Spec weaknesses versus the industry standard (fix before the M1 schema is cut)

1. **`contacts.status` is undefined and is the biggest design risk.** Every product with a contact-level status eventually conflates consent with deliverability (Mailchimp `cleaned`, Interspire `bounced`, listmonk `blocklisted`). **No marketing-eligibility column on the contact.** The contact holds identity and attributes. Eligibility is *derived*: `exists membership(subscribed) AND NOT suppressed(address, workspace, instance) AND topic not opted out`. Put that predicate in **one** SQL function or view used by segment counts, the send-time check (invariant 2) and the preference page, so "audience count" and "actually sent" cannot disagree. A denormalised cache is an optimisation, never the source of truth.
2. **`unconfirmed` is the listmonk trap.** Use `pending | subscribed | unsubscribed` for membership state. `pending` never receives marketing regardless of list setting. Record opt-in mode (single/double) on the event, not inside the state's meaning.
3. **Topics are a feature with no data model.** Cheapest coherent design: a topic is a **list with `visibility = public|private`** (listmonk's list type) plus a default of opt-in or opt-out. That keeps four concepts (lists, tags, segments, suppressions) and no fifth. The preference centre shows public lists.
4. **Every campaign declares one "subscription scope" list.** Segments have no unsubscribe target, so a campaign sent to a segment still needs a list whose membership the one-click unsubscribe flips (Customer.io requires a topic on every message; listmonk unsubscribes from the campaign's lists, per `unsubscribe-by-campaign`). Default unsubscribe scope = that list. The preference page offers "unsubscribe from everything", which writes a workspace-level `unsubscribe_all` suppression. This avoids Brevo's multi-brand complaint while keeping a true global opt-out.
5. **Suppression must outlive the contact.** Key suppressions on normalised email (and a stored hash), **not** by FK to `contacts`. GDPR erase removes the contact row, attributes, events and consent *detail*, and keeps only the minimum needed to honour the objection (address hash, reason, timestamp). listmonk loses the block on delete; Mailchimp's "deleted can't be re-added" blocks even a legitimate re-signup. Whether retaining a hash is lawful in your jurisdictions needs counsel; I flag it, I do not assert it.
6. **Suppression rows need provenance and reversibility.** The spec has reason/source/timestamp; add `created_by`, `lifted_at`, `lifted_by`, `lift_reason`, and never hard-delete. Interspire's ban list has none of these.
7. **Field types are too few for the migration source.** Interspire has dropdown, radio, multi-checkbox, multiline text and date; the spec has text, number, date, boolean. Add `select`, `multi_select` and `datetime`. Multiline is text with a UI hint.
8. **No policy for field delete or type change.** See 9.3.
9. **The spec does not say that import must never alter consent state.** See 9.5.

### 9.2 Recommended contact core (smallest coherent shape)

- `contacts`: `id` (uuid v7), `workspace_id`, `email` (citext, normalised), `attributes` JSONB keyed by immutable field keys, `timezone`, timestamps; `UNIQUE(workspace_id, email)`. The id is stable so the email can be edited (Mailchimp separates `contact_id` from its hash id; Klaviyo uses `id`). Never use email as a join key elsewhere (recipients, tags, events reference `contact_id`).
- Normalisation: trim, lowercase, IDN domains to ASCII. Do **not** strip plus-tags or Gmail dots. Allow 254 characters (Interspire truncates at 200).
- `list_memberships(contact_id, list_id, state, state_changed_at)` plus append-only `membership_events(contact_id, list_id, from, to, method, source, actor, ip, user_agent, at)`. The state column caches the latest event.
- `suppressions(id, workspace_id NULL = instance, match_kind address|domain, value, email_hash, reason, source_ref, created_at, created_by, lifted_at, lifted_by, lift_reason)`. Reasons: `hard_bounce, soft_bounce_limit, complaint, unsubscribe_all, manual, invalid_address, imported_legacy, erasure_request`. Domain matching preserves Interspire's `@domain` capability.
- Reasons carry a **stickiness class**. `unsubscribe_all` and `manual` are liftable by a workspace admin with consent evidence. `hard_bounce`, `complaint`, `invalid_address` are liftable only by an owner with an audit entry (Klaviyo keeps hard bounces even on resubscribe; Brevo warns of legal and reputation exposure).

### 9.3 Custom fields: be clearly better than the typed-but-brittle camp

- Immutable `key` (`[a-z][a-z0-9_]{0,62}`), mutable `label`, `type`, `options` (for select), `required`, `default`. Reserved keys rejected. This fixes Mailchimp/Brevo "name and type cannot change".
- v1 types: `text, number, boolean, date, datetime, select, multi_select`. `url`/`email`/`phone` as validation hints on text can come later.
- Store canonical JSON values (number as JSON number, date as `YYYY-MM-DD`, datetime as RFC 3339 UTC, multi_select as an array). The segment compiler casts from the definition, so nobody hand-casts like in listmonk.
- **Delete means archive first**: hidden from UI, segments and merge tags, data retained for a configurable window (e.g. 30 days) with one-click restore, then purge. Mailchimp's own help recommends "hide" as the workaround for its destructive delete; make it the default.
- **Type change is a conversion job with a dry-run report** (n convertible, n failing, examples). Never silent coercion, never "delete and recreate". Mailchimp, Brevo and Interspire cannot do this.
- Limits are safety caps, not billing: configurable max fields per workspace (suggest 200), max attribute JSON size per contact (suggest 64 KB), per-type value limits. Mailchimp's 30/80 and 255-byte limits are plan artefacts, not good design.
- Opportunity: an "indexed" flag per field creating a Postgres expression index, so segment counts stay instant at 500k+ contacts. listmonk's raw-SQL segments are only fast if the admin knows how to index.

### 9.4 Lists / tags / segments: wording to put in the spec

- **List** = a consent container (membership state, opt-in mode, visibility).
- **Tag** = free-form internal label set by humans, API, forms or automation. No consent meaning. Never visible to the subscriber.
- **Segment** = computed, read-only membership using list state, tags, fields, events. Cannot be hand-edited (Klaviyo rule). "Snapshot to list" is an explicit one-time copy.
- Only a membership in `subscribed` state implies consent. Not a list, tag or segment. The segment builder should include "can receive marketing" as a default-on condition (Klaviyo makes it optional, which is the footgun).

### 9.5 Import, API and re-subscribe rules

1. **Import never changes `unsubscribed` or suppressed state.** Matches Interspire's safest behaviour and reports them separately ("skipped: already unsubscribed N, suppressed N"). This defuses the Klaviyo complaint class.
2. Setting `subscribed` through import or API requires an explicit consent payload (`method`, `timestamp`, evidence/source, importer attestation) and an admin-level permission. Without it, new contacts get **no membership**, the NEVER_SUBSCRIBED / Mailchimp "non-subscribed" equivalent, not `subscribed`.
3. Per-import update modes: `skip`, `fill blank only`, `overwrite fields`, separate from any consent behaviour. listmonk's two independent flags are the right instinct; "fill blank" is the improvement.
4. A public-form re-subscribe after `unsubscribe_all` or a list unsubscribe is allowed, goes through DOI (`pending`) and writes a new consent event. Re-subscribe through API or import is not.
5. API semantics: membership changes use explicit endpoints (`subscribe`, `unsubscribe`); contact `PATCH` cannot silently drop memberships. Avoid listmonk's PUT-removes-your-lists trap.
6. Email change is allowed (stable id). The *new* address is checked against suppressions. The *old* address's suppression stays with the address.
7. Instance-wide suppression is visible to instance admins; workspaces see only "suppressed (instance)" with no detail. Otherwise it leaks cross-tenant facts and fights invariant 8.
8. Scope defaults (owner decision, make configurable): hard bounce and invalid address = **instance-wide** (the mailbox is gone for every brand and brands share relay reputation). Complaint and `unsubscribe_all` = **workspace** (consent is to a brand), with complaints also alerting the instance admin.
9. Soft bounces count but do not suppress until a configurable threshold (Klaviyo: 7 consecutive; listmonk: configurable). Detailed bounce classification belongs to M3.

### 9.6 Double opt-in and consent log

- `pending` expires (default 7 to 14 days, configurable), then is marked `expired`; the consent event stays. Interspire and listmonk leave cleanup to manual housekeeping.
- The consent log is **append-only with no UPDATE/DELETE grant** at the DB-role level. Fields: `contact_id`, email snapshot, list, action (`granted | confirmed | withdrawn`), method (`form | api | import | admin | one_click | preference_center | legacy_import`), source detail (form id/URL, import id, user id), IP, user agent, timestamp, **form/consent-text version hash**, confirmation token id.
- IP capture on by default with a documented per-workspace switch (listmonk defaults it off, which weakens proof).
- On GDPR erase: purge personal detail (IP, UA, email) but keep a pseudonymised stub (hash, action, time) if counsel agrees. Document it.

### 9.7 Interspire migration constraints that affect the M1 schema (import itself is M6)

Schema facts are [V-src]. Mapping choices are [I].

- **Merge by email across lists.** IEM has one row per (list, email). Collapse to one NM contact per (workspace, lowercased trimmed email). Field values can differ per row and IEM has no `updated_at`; use a deterministic rule (latest `subscribedate` wins per field, non-empty beats empty) and log conflicts in the import report.
- **State mapping:** `confirmed='0'` => `pending`. `unsubscribed>0` => membership `unsubscribed` (event time = the timestamp, method `legacy_import`) **and** a suppression. Spec section 10 says unsubscribes become suppressions, but IEM unsubscribes are per list, so a workspace-wide suppression over-suppresses people who left one list only. That is the conservative legal choice but loses reach. **Make scope a flag (`list` or `workspace`), default `workspace`, and show counts in the dry run.** The owner should decide.
- `bounced>0` => read `list_subscriber_bounces.bouncetype`: `hard` => `hard_bounce` suppression; `soft` and `unknown` => record only. `bouncemessage` feeds the suppression detail.
- `banned_emails`: `list='g'` => workspace suppression `manual`; list-id bans => list-scoped suppression; entries that are a bare domain or start with `@` become **domain** suppressions (needs `match_kind = domain`).
- Consent evidence: `requestdate/requestip`, `confirmdate/confirmip`, `subscribedate`, `formid` => consent_log rows with `method = legacy_import`.
- **Traps:** custom dates are strings in each field's configured day/month/year order; checkboxes are PHP-`serialize()`d; "Numbers only" is length-constrained so it likely holds phone or ID digit strings. Map it to `text` unless all values are numeric and the admin agrees, or leading zeros are lost. Encrypted fields cannot be read without the app key. Timestamps are ints in the server's configured timezone. Email is `varchar(200)`.
- IEM `unsubscribearea` is a one-character area code, not a reason. Map to a generic `legacy_unsubscribe` and keep the raw code in `source_ref`.

### 9.8 Where NM can be clearly better

1. **Suppression with provenance, stickiness, role-gated undo and erasure safety.** Nobody has all four; Klaviyo has three.
2. **Typed, evolvable, non-destructive custom fields** (archive, dry-run type conversion). Mailchimp, Brevo and Interspire are all destructive.
3. **One eligibility predicate** shared by counts and sending.
4. **Import that cannot re-subscribe anyone** by construction.
5. **A consent log that stores the wording version**, append-only.
6. **Per-brand isolation with a true global opt-out** (fixes Brevo's multi-brand complaint) via the campaign scope list.
7. **Safe, typed segments on JSONB** with optional per-field expression indexes, instead of listmonk's raw SQL.

### 9.9 What not to copy
- Mailchimp's 30-field and 255-byte limits, and "a contact in N audiences = N contacts".
- Klaviyo's 90-day lockout and its type-per-condition model.
- Brevo's global-by-default marketing unsubscribe.
- listmonk's blocklist-as-subscriber-status and its context-dependent `unconfirmed`.
- Interspire's hand-maintained counters (compute from events and running totals, as the spec's `campaign_stats` approach already does).

---

## 10. Confidence and gaps

- **Verified from source or docs:** Interspire schema and behaviour (read the code); listmonk schema and queries; Mailchimp field types, limits, statuses and consent fields; Klaviyo consent, suppression reasons and rules; Brevo contact, attribute and blocked-reason APIs; Kit state enum; Customer.io and Resend models.
- **Relied on search summaries (direct fetch returned 403 or 404):** Brevo help-centre articles (attributes, blocklist), Mailchimp resubscribe article, Mailchimp's billing rationale (Intuit-hosted help), Kit help definitions, Klaviyo property limits. Treat as [C].
- **Not found:** Brevo DOI representation and consent-proof fields; per-property hard limits in Klaviyo and Brevo; how Klaviyo handles mixed-type property values; formal Mailchimp definitions of `pending` and `transactional`; whether Kit has one list or many.
- **Not researched:** HubSpot (marketing-contact billing concept), and Postmark/SES/OCI account-level suppression lists (relevant to M3). If OCI keeps its own suppression list that NM cannot see, NM's `invalid_address` and `hard_bounce` suppressions should be exportable and reconcilable.
- Legal remarks (hash retention under GDPR, consent wording) are engineering observations, not legal advice.
