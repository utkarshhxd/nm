-- SPDX-License-Identifier: AGPL-3.0-only

-- Supports the inline sweep in `sessions::resolve` (`delete from sessions where expires_at <= now()`).
-- Migration 0002 indexes only `user_id`, so expired rows had nothing supporting their removal and
-- the table grew without bound.
create index sessions_expires_at on sessions (expires_at);
