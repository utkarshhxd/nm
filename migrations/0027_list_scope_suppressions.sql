-- SPDX-License-Identifier: AGPL-3.0-only

-- A third suppression scope, `list`: "never put this address back on THIS list".
--
-- Why it is here and not in `list_memberships`. A per-list unsubscribe lived only in that row,
-- and two routine Editor actions destroyed it with its consent history: deleting the contact
-- (`list_memberships` and `consent_log` both cascade on `contacts`) and deleting the list
-- (`list_memberships` cascades on `lists`). After either, an import re-subscribed the person and
-- the log could not say they had ever left — and because the importer resolves a file's list by
-- NAME, delete-recreate-import resurrected everyone who had ever left that list. `suppressions`
-- is already the table that outlives the thing it protects: it is keyed on the address rather
-- than on a contact row, and `gdpr_erasure` already proves a block can survive the destruction of
-- its subject. Spec §8 invariant 1, amended 2026-11-20.
--
-- **Keyed on the NAME, with the id as well.** `lists` carries `unique (workspace_id, name)` and
-- the importer matches a file's list by name, so the name is what "the same list" means to the
-- importer and to a human, and it is the only identifier that survives delete-and-recreate.
-- `list_id` is the useful join while the list lives and goes null when it dies, which is how a
-- row says "the list this refusal was about is gone".
--
-- **The id is the only thing a reader ever matches on, and the triggers below are what makes
-- that safe.** A list that dies orphans its rows; a list born with that name adopts them again,
-- and a rename carries the name along. So `list_id` is filled for every list that exists, and a
-- reader needs no second, name-shaped branch that a future reader could forget. The name is what
-- the refusal survives *on*, not what anything matches on.

alter table suppressions
  add column list_id uuid,
  add column list_name text,
  -- `set null (list_id)`, not plain `set null`: the composite reference would otherwise null
  -- `workspace_id` too and break the shape check below, which would make deleting a list fail.
  -- `consent_log` (0020) references `lists` the same way for the same reason.
  add constraint suppressions_list_fkey
    foreign key (workspace_id, list_id) references lists(workspace_id, id)
    on delete set null (list_id);

alter table suppressions drop constraint suppressions_scope_check;
alter table suppressions add constraint suppressions_scope_check
  check (scope in ('address', 'domain', 'list'));

-- A reason of its own, and not `unsubscribed_all`, which means the whole workspace. Nothing in
-- Rust can name it: `suppressions::Reason` has no variant for it, so the one route that creates a
-- suppression (`POST /suppressions`) cannot write one even if its own checks were removed.
alter table suppressions drop constraint suppressions_reason_check;
alter table suppressions add constraint suppressions_reason_check
  check (reason in
    ('unsubscribed_all', 'bounced', 'complained', 'manual', 'gdpr_erasure', 'unsubscribed_list'));

-- The two are each other's only use, in both directions: a list-scope row cannot carry a reason
-- that belongs to an address, and `unsubscribed_list` cannot be smuggled onto an address row,
-- where every address-level reader would honour it as a block on all mail.
alter table suppressions add constraint suppressions_list_reason
  check ((scope = 'list') = (reason = 'unsubscribed_list'));

-- The shape check gains its `list` branch. A list row looks like an address row — plaintext and
-- hash, no domain — plus the list it is about, and it is never instance-wide, because a list
-- belongs to exactly one workspace. The plaintext is what `mailable_list_members` and the
-- importer match on, exactly as they match an address row; `suppressions::erase_address` deletes
-- it with the rest when the address is erased, so nothing readable survives an erasure.
alter table suppressions drop constraint suppressions_check;
alter table suppressions add constraint suppressions_check
  check (case scope
           when 'address' then domain is null and email_hmac is not null
           when 'domain' then domain is not null and email_hmac is null and email is null
           when 'list' then domain is null and email is not null and email_hmac is not null
                            and list_name is not null and workspace_id is not null
         end);

-- The two new columns belong to the new scope and to nothing else.
alter table suppressions add constraint suppressions_list_columns
  check (scope = 'list' or (list_id is null and list_name is null));

-- `list_name` joins the live-uniqueness key, or two refusals by one address on different lists
-- collide. Existing address and domain rows have a null `list_name` and `nulls not distinct`
-- keeps treating two of them as equal, so their behaviour is unchanged.
drop index suppressions_live;
create unique index suppressions_live on suppressions
  (workspace_id, scope, email_hmac, domain, list_name)
  nulls not distinct where lifted_at is null;

-- Keyed on `list_id`: the only thing a reader matches on. It is also what the foreign key above
-- walks when a list is deleted and what the rename trigger walks. Partial, because every address
-- and domain row has a null `list_id` and belongs in neither.
create index suppressions_by_list on suppressions (workspace_id, list_id, email)
  where list_id is not null;
-- The orphans, by the name they are waiting on: what the adoption trigger below walks when a list
-- of that name is created or renamed.
create index suppressions_orphaned_by_name on suppressions (workspace_id, list_name)
  where lifted_at is null and scope = 'list' and list_id is null;

-- **A rename must not defeat it, and a delete-and-recreate must not either.** Renaming a list
-- would otherwise orphan every suppression that names it; recreating a deleted list would
-- otherwise leave its refusals orphaned while a live list of that name took members again.
-- Triggers rather than lines in the two handlers: a handler can be forgotten, and a second writer
-- of `lists.name` added later would not know to call it. This is the same "make it impossible"
-- shape `consent_log`'s append-only triggers use.
--
-- Adoption is what lets every reader match on `list_id` alone. It cannot collide: `new.id` is
-- fresh on insert, and two live orphans cannot share one `(workspace, address, name)` because
-- `suppressions_live` forbids it.
--
-- The `not exists` on the rename is the one collision this can meet: an orphaned refusal already
-- naming `new.name`, left by an earlier list of that name, for the same address. Renaming onto it
-- would violate `suppressions_live` and fail the operator's rename with a 500. That row keeps its
-- old name instead, which costs nothing — it still carries `list_id`, which is the only thing any
-- reader matches on, and the orphan beside it is adopted in the same statement run below.
create function suppressions_follow_list_name() returns trigger
  language plpgsql as $$
begin
  if tg_op = 'UPDATE' then
    update suppressions s set list_name = new.name
     where s.list_id = new.id
       and not exists (select 1 from suppressions o
                        where o.workspace_id = s.workspace_id and o.scope = 'list'
                          and o.lifted_at is null and o.list_id is null
                          and o.list_name = new.name and o.email_hmac = s.email_hmac);
  end if;
  update suppressions set list_id = new.id
   where workspace_id = new.workspace_id and scope = 'list' and lifted_at is null
     and list_id is null and list_name = new.name;
  return null;
end;
$$;

create trigger lists_creation_adopts_suppressions
  after insert on lists
  for each row
  execute function suppressions_follow_list_name();

create trigger lists_rename_follows_suppressions
  after update of name on lists
  for each row when (old.name is distinct from new.name)
  execute function suppressions_follow_list_name();

-- **The trap this change exists to avoid, closed in one place.** Every reader written before
-- today assumes a live suppression row for an address means that address is blocked outright.
-- The moment a third scope means "blocked for one list", a reader that misses it silently stops
-- ALL mail to somebody who left one list. This view is what an address-level reader selects from,
-- so a reader that forgets the distinction gets the old meaning rather than a new one. The Rust
-- side carries the other half: `suppressions::Scope` and `suppressions::Reason` have no variant
-- for `list` or `unsubscribed_list`, so an address-level query that does reach one of these rows
-- fails to decode it and the request errors, rather than reporting a block that is not there.
create view address_suppressions as
  select * from suppressions where scope in ('address', 'domain');

-- The one definition of "may this list member be emailed" gains its fifth clause. Still separate
-- `not exists` clauses and not one with `or` inside, for 0015's measured reason.
--
-- **One clause and not two, where the four above are two pairs.** The others pair a workspace-
-- scope arm with an instance-scope one; a list belongs to one workspace, `suppressions_check`
-- above requires `workspace_id` on a list row, and there is no instance-wide list to block.
create or replace view mailable_list_members as
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
                    and s.scope = 'domain' and s.domain = c.email_domain)
  and not exists (select 1 from suppressions s where s.lifted_at is null
                    and s.workspace_id = m.workspace_id
                    and s.scope = 'list' and s.email = c.email
                    and s.list_id = m.list_id);
