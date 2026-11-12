-- SPDX-License-Identifier: AGPL-3.0-only

-- The one definition of "may this list member be emailed": subscribed, and neither the address
-- nor its domain suppressed, in this workspace or instance-wide. The mailable count uses it now;
-- the send-time recipient resolution must use it later, never a fresh copy of the rule.
--
-- A view, not a STABLE SQL function: a function containing EXISTS is not inlined by the planner,
-- which measured roughly twice as slow.
--
-- FOUR separate NOT EXISTS clauses, not two with `or` inside. An OR-qualified anti-join cannot
-- use the partial indexes and degrades to a sequential scan. Ugly, measured, deliberate: do not
-- merge them.
--
-- Erasure tombstones have `email is null` and match nothing here. That is correct: an erased
-- contact has no `contacts` row to count. Tombstones are enforced on the single-address path,
-- `suppressions::is_suppressed`, which matches on the HMAC.
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
