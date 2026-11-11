-- SPDX-License-Identifier: AGPL-3.0-only

-- `email` is the normalised address submitted; it need not belong to a user, and is null when the
-- submitted address did not normalise. `ip` is the client address, IPv6 masked to its /64 (the
-- limiter's bucket). A row is written as a failure before the password is checked and flipped to
-- `succeeded = true` on success; sign-in lockout counts rows with `succeeded = false`.
create table login_attempts (id bigint generated always as identity primary key, email text, ip inet,
  succeeded boolean not null, at timestamptz not null default now());
create index login_attempts_email_at on login_attempts (email, at);
create index login_attempts_ip_at on login_attempts (ip, at);
-- For the inline cleanup (`where at < ...`).
create index login_attempts_at on login_attempts (at);
