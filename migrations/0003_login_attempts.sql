-- SPDX-License-Identifier: AGPL-3.0-only

-- `email` is the normalised address submitted (empty if it was not an email at all); it need not
-- belong to a user. Sign-in lockout counts rows with `succeeded = false`.
create table login_attempts (id bigint generated always as identity primary key, email text not null, ip inet,
  succeeded boolean not null, at timestamptz not null default now());
create index login_attempts_email_at on login_attempts (email, at);
create index login_attempts_ip_at on login_attempts (ip, at);
