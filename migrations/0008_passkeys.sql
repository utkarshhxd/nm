-- SPDX-License-Identifier: AGPL-3.0-only

-- `passkey` is the serialised webauthn-rs `Passkey` (public key, signature counter, backup flags);
-- `credential_id` repeats its id so a sign-in can find the row. No private key material is ever
-- held by the server.
create table passkeys (id uuid primary key, user_id uuid not null references users(id) on delete cascade,
  credential_id bytea not null unique, passkey jsonb not null, name text not null,
  created_at timestamptz not null default now(), last_used_at timestamptz);
create index passkeys_user on passkeys (user_id);
-- One in-flight ceremony. `state` is the serialised webauthn-rs ceremony state: it is the server's
-- half of the challenge, so it is never returned by the API, logged or audited. A challenge is
-- deleted when used and is dead after `expires_at` (5 minutes after it is issued). `user_id` is
-- null for a sign-in, which starts before anyone is known. `ip` is the requester's rate-limit bucket
-- (`rate_limit::bucket`) for a sign-in challenge, which anyone can start for free: it bounds how
-- many live ones one address may hold. Null for a registration, or when the address is unknown.
create table auth_challenges (id uuid primary key, user_id uuid references users(id) on delete cascade,
  kind text not null check (kind in ('passkey_register','passkey_login')), state jsonb not null, expires_at timestamptz not null,
  ip inet);
-- For the inline cleanup of expired challenges (`where expires_at <= now()`).
create index auth_challenges_expires on auth_challenges (expires_at);
-- For the per-address cap on live sign-in challenges.
create index auth_challenges_ip on auth_challenges (ip, expires_at);
