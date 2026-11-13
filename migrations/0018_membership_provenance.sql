-- SPDX-License-Identifier: AGPL-3.0-only

-- How a membership came to exist (spec M1b section 7 and 15.1). The consent log in M1d has to be
-- able to explain every membership, and a row written without this can never be explained later.
--
-- Nullable, and NO DEFAULT, on purpose. A `not null default 'api'` would stamp every existing
-- membership (every UI one, every bulk/subscribe one) with a provenance that is false, and would
-- do it in the migration that creates the column. NULL means "written before provenance was
-- recorded", which is true. `import` and `form` have no writer yet (M1c and M1d).
alter table list_memberships add column source text
  check (source in ('ui','api','import','form','bulk'));
