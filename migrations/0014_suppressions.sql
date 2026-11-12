-- SPDX-License-Identifier: AGPL-3.0-only

-- The standing blocks: "never email this address, whatever else says otherwise". Everything else
-- in M1a organises people; this table protects them.
--
-- `workspace_id` null means instance-wide: it applies to every workspace, and only an instance
-- admin may create or lift such a row.
--
-- Plaintext `email` AND `email_hmac` both exist, and that is not redundancy. A blocklist is a list
-- of addresses an admin manages, reads and exports, so a hash alone would make it unusable. The
-- hash exists for the one row that must outlive its plaintext: the GDPR erasure tombstone
-- (`reason = 'gdpr_erasure'`, `email` null), which keeps the block alive after the address itself
-- is destroyed. It is HMAC-SHA256 under a key derived from the instance secret, not a plain
-- digest: addresses are an enumerable space, so a plain digest of this table would be reversible
-- by dictionary and a leaked copy would name exactly who complained or asked to be erased.
-- ONE-WAY DOOR: the derivation string lives in `crypto::suppression_hmac`. Changing it makes every
-- stored hash unreproducible and silently unblocks every erased person.
create table suppressions (id uuid primary key,
  workspace_id uuid references workspaces(id) on delete cascade,
  scope text not null check (scope in ('address','domain')),
  email text, domain text, email_hmac bytea,
  reason text not null check (reason in
    ('unsubscribed_all','bounced','complained','manual','gdpr_erasure')),
  source text not null default '', note text,
  created_at timestamptz not null default now(),
  created_by_user_id uuid references users(id) on delete set null,
  -- Lifting sets this rather than deleting the row, so the blocklist keeps its history.
  lifted_at timestamptz,
  lifted_by_user_id uuid references users(id) on delete set null,
  -- Enforced here, not by convention: an address row has a hash and no domain; a domain row has a
  -- domain and neither an email nor a hash, since there is no address to hash.
  check (case scope
           when 'address' then domain is null and email_hmac is not null
           when 'domain' then domain is not null and email_hmac is null and email is null
         end),
  check (reason <> 'gdpr_erasure' or (scope = 'address' and email is null)));

-- Partial on `lifted_at is null`, which is what lets an address be suppressed again after a lift.
-- `nulls not distinct` so two instance-wide rows (null workspace) for one address collide too.
create unique index suppressions_live on suppressions (workspace_id, scope, email_hmac, domain)
  nulls not distinct where lifted_at is null;

create index suppressions_lookup_address on suppressions (email)
  where lifted_at is null and scope = 'address';
create index suppressions_lookup_domain on suppressions (domain)
  where lifted_at is null and scope = 'domain';
-- The single-address path (`suppressions::is_suppressed`): the only lookup that sees a tombstone.
create index suppressions_lookup_hmac on suppressions (email_hmac) where lifted_at is null;

-- Not in the spec's four: the suppressions list pages by id within one workspace (or the
-- instance), lifted rows included, and none of the partial indexes above can serve that.
create index suppressions_by_workspace on suppressions (workspace_id, id desc);
