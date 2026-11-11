-- SPDX-License-Identifier: AGPL-3.0-only

-- `totp_*_enc` hold Secrets blobs (AES-256-GCM); `totp_pending_secret_enc` is a secret awaiting its
-- first confirmed code. `totp_last_step` is the highest accepted 30-second step (replay protection).
alter table users add column totp_secret_enc bytea, add column totp_pending_secret_enc bytea,
  add column totp_enabled_at timestamptz, add column totp_last_step bigint;
-- `code_hash` is sha256 of the normalised code (lowercase, hyphen removed); `used_at` marks it spent.
create table recovery_codes (id uuid primary key, user_id uuid not null references users(id) on delete cascade,
  code_hash bytea not null, used_at timestamptz);
create index recovery_codes_user on recovery_codes (user_id);
