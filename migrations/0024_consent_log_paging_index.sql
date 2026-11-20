-- SPDX-License-Identifier: AGPL-3.0-only

-- The index the consent-history panel pages on, and the reason it is keyed on `id` and not on
-- `created_at`.
--
-- Postgres `now()` is transaction-start time, and a membership write and its consent row share a
-- transaction; both set-based writers (`contacts::selection::run_recorded` and the importer's
-- `write_memberships_and_tags`) insert a whole batch in one statement. So `created_at` ties
-- routinely — it is not a total order on this table and never will be. A keyset page on a
-- non-unique key either skips the rows that share the boundary value or returns them twice, and
-- the panel it skips them from is the one that has to answer "prove this person agreed".
--
-- `consent_log.id` is UUID v7, which is time-ordered, so `id desc` is a total order AND
-- newest-first: one key does both jobs.
--
-- `0020`'s index of the same name, on `(workspace_id, contact_id, created_at desc)`, is replaced
-- rather than left beside this one. Nothing has ever read this table in `created_at` order and
-- nothing may, for the reason above, so keeping it would be a second index to maintain on every
-- consent write — and a consent write happens inside every membership write in the product.
-- `0020` itself is not edited: it has been applied, and changing an applied migration's bytes
-- makes `sqlx migrate run` refuse on every database that has already run it.
drop index consent_log_by_contact;
create index consent_log_by_contact on consent_log (workspace_id, contact_id, id desc);
