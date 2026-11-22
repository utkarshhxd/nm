-- SPDX-License-Identifier: AGPL-3.0-only

-- The three tables the sending core reads: where mail leaves through (`relays`), which addresses a
-- workspace may send from (`sending_domains`), and how fast a relay may be driven (`relay_buckets`).
--
-- Every index a table will ever need is in its `create table` below. `sqlx::migrate!` wraps every
-- migration in a transaction (0026 section 6 has the long version), so `create index concurrently`
-- is not available later; these tables are empty today and that is the cheap moment.
--
-- Every table carries `workspace_id` and every reference between them is composite and carries it
-- too, as `form_lists` and `consent_log` do (0023, 0020, 0026): a row of one workspace naming a row
-- of another cannot be written, and the handler's 404 is backed by the database's refusal.

-- ---------------------------------------------------------------------------------------------
-- 1. `relays`
create table relays (
  id uuid primary key,
  workspace_id uuid not null references workspaces (id) on delete cascade,
  name text not null,
  host text not null,
  port int not null check (port between 1 and 65535),
  security text not null check (security in ('implicit_tls', 'starttls')),
  username text,
  password_encrypted bytea,
  max_connections int not null default 4 check (max_connections > 0),
  rate_limit_per_minute int not null check (rate_limit_per_minute > 0),
  enabled bool not null default true,
  failover_order int not null default 0,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  constraint relays_workspace_name_key unique (workspace_id, name),
  -- Not redundant with the primary key: Postgres requires a unique constraint on exactly these
  -- columns for `relay_buckets`' composite foreign key to point at.
  constraint relays_workspace_id_id_key unique (workspace_id, id)
);

-- What the sender asks on every pick: "the enabled relays of this workspace, in failover order".
-- Partial, so a disabled relay costs the hot path nothing; `id` makes the order total.
create index relays_failover on relays (workspace_id, failover_order, id) where enabled;

comment on table relays is
  'SMTP relay configuration: the places this workspace''s mail leaves through. Several may exist; '
  'the sender takes the enabled ones in (failover_order, id) order and falls to the next when one '
  'is down. `password_encrypted` is an AES-256-GCM blob from src/crypto/secrets.rs and is never '
  'returned by the API. `rate_limit_per_minute` is what the relay''s operator told us it allows, '
  'and `relay_buckets` is what makes us keep to it.';

comment on column relays.max_connections is
  'Concurrent SMTP connections ONE sender process may hold open to this relay. Per process, not '
  'shared: the connection pool lives in the process, unlike relay_buckets, which is shared through '
  'this database precisely because a rate limit cannot be held per process. Two worker processes '
  'may therefore hold twice this many open, and an operator running more than one must size it '
  'with that in mind. A relay that caps connections per client refuses the extras at connect time.';
comment on column relays.failover_order is
  'Lower is tried first. Ties break on id, so the order is always total and never flips between '
  'two runs of the same query.';

-- ---------------------------------------------------------------------------------------------
-- 2. `sending_domains`
create table sending_domains (
  id uuid primary key,
  workspace_id uuid not null references workspaces (id) on delete cascade,
  domain text not null,
  from_name text,
  signing text not null check (signing in ('relay', 'app')),
  dkim_selector text not null,
  dkim_private_key_encrypted bytea,
  return_path_domain text,
  checks jsonb not null default '{}'::jsonb,
  checked_at timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  constraint sending_domains_workspace_domain_key unique (workspace_id, domain),
  -- Same guard `0026` puts on `jobs.payload` and on a form's `fields`: jsonb accepts a bare
  -- scalar or a null as happily as an object, and every reader here indexes into it by key. A
  -- `checks` of `7` or `"ok"` would deserialize to an error at the one moment the DNS checker's
  -- result is being read, which is while an operator is waiting on the settings page.
  constraint sending_domains_checks_is_object check (jsonb_typeof(checks) = 'object'),
  -- The composite parent key, for the same reason `relays` carries one: a later table that
  -- references a sending domain must carry `workspace_id` into the reference, and Postgres needs
  -- a unique on exactly these two columns to point at. M2c's per-recipient `messages` rows are
  -- the expected first child. Free now, a migration later.
  constraint sending_domains_workspace_id_id_key unique (workspace_id, id),
  -- An app-signed domain without a key cannot sign anything, so it cannot exist. The API refuses
  -- it with a field error first; this is the backstop for every writer that is not the API.
  constraint sending_domains_app_signing_has_key
    check (signing = 'relay' or dkim_private_key_encrypted is not null)
);

comment on table sending_domains is
  'The from-domains a workspace may send as, with their DKIM settings. `signing = ''relay''` means '
  'the relay signs and we hold no key; `signing = ''app''` means we sign, and then '
  '`dkim_private_key_encrypted` (an AES-256-GCM blob from src/crypto/secrets.rs) must exist, which '
  'sending_domains_app_signing_has_key enforces. A campaign may use only a domain listed here.';

comment on column sending_domains.checks is
  'The DNS checker''s last result, as one document. Its shape belongs to the Rust that writes it '
  'and may grow without a migration; the four checks are deliberately not four columns.';
comment on column sending_domains.checked_at is
  'When `checks` was written. Null means the checker has never run for this domain.';

-- ---------------------------------------------------------------------------------------------
-- 3. `relay_buckets`
create table relay_buckets (
  relay_id uuid primary key,
  workspace_id uuid not null,
  -- A bucket that goes negative stops refusing anyone and nothing goes red. The taking statement
  -- refills by elapsed time and then subtracts; if that arithmetic is ever wrong (a refill that
  -- overshoots, a subtraction not predicated on the balance) this turns the bug into a violation
  -- at the moment it is made, as `jobs_throttle_retention` does for 0025's rule.
  tokens int not null constraint relay_buckets_tokens_non_negative check (tokens >= 0),
  refilled_at timestamptz not null default now(),
  constraint relay_buckets_relay_fk foreign key (workspace_id, relay_id)
    references relays (workspace_id, id) on delete cascade
);

comment on table relay_buckets is
  'One token bucket per relay, shared by every worker, so the relay''s rate limit is kept across '
  'processes and not per process. This bucket is the only thing standing between us and a `455` '
  'the relay publishes no `Retry-After` for: once a relay throttles us there is no signal telling '
  'us when to try again, so we must not get throttled. Taking a token and refilling are one '
  'statement on this row; `refilled_at` is when `tokens` was last topped up. '
  'relay_buckets_tokens_non_negative is what makes a mistake in that arithmetic loud: subtracting '
  'more than the balance holds -- a take not predicated on the balance, or one predicated on the '
  'pre-refill value -- would otherwise leave a negative balance that refuses nobody, which is the '
  'exact failure this table exists to prevent, with nothing going red. A refill that overshoots '
  'is the opposite mistake and this constraint does not catch it; the cap belongs in the taking '
  'statement.';
