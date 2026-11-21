-- SPDX-License-Identifier: AGPL-3.0-only

-- The preference center writes memberships, and `0018` left no truthful value for them.
--
-- `0018`'s own words: "NULL means 'written before provenance was recorded', which is true", and
-- its check admits `ui, api, import, form, bulk` only. A preference-center write is none of those.
-- The three available answers were a false `'ui'`, a `NULL` that asserts something false about
-- when the row was written, or this. `consent_log.source` has known `preference_center` since
-- `0020`; this is the two tables agreeing on what a source is, rather than one of them lying.
-- `'form'` stays reserved for the hosted signup forms, which are a different surface with a
-- different rule: a form follows the list's `double_optin` and may write `pending`, where the
-- preference center never does.
--
-- **The two-step form, from `issues/24`.** `0021` widened `consent_log`'s check with a plain
-- `drop constraint` / `add constraint`, which takes `ACCESS EXCLUSIVE` and validates every row —
-- correct there, on a table that was empty. `list_memberships` is the biggest table in the
-- product: an import writes a million rows into it. So the scan is moved out of the exclusive
-- lock. `drop` and `add ... not valid` are both metadata-only and instant; `validate constraint`
-- takes `SHARE UPDATE EXCLUSIVE`, which blocks neither reads nor writes.
--
-- The window in which the constraint exists unvalidated is what made this the wrong trade for a
-- small table, and is exactly what makes it the right one here. Nothing can enter the window
-- anyway: the old constraint's value set is a subset of the new one, so no row that satisfied the
-- old one can fail the new one, and the validation is a formality that marks the catalog.
alter table list_memberships drop constraint list_memberships_source_check;
alter table list_memberships add constraint list_memberships_source_check
  check (source in ('ui','api','import','form','bulk','preference_center')) not valid;
alter table list_memberships validate constraint list_memberships_source_check;
