-- SPDX-License-Identifier: AGPL-3.0-only

-- `relays.max_connections` does not cap concurrent connections, and 0028's comment said it did.
--
-- The comment is the authoritative description of the column, and this one described a guarantee
-- the product does not make. The value reaches lettre as `PoolConfig::max_size`, which bounds how
-- many connections are *parked* for reuse when a session is recycled; a send that finds the
-- parked list empty opens a new connection rather than waiting for one. What bounds how many
-- sessions are open at once is `NM_WORKER_CONCURRENCY` (default 4), which is a property of the
-- worker process and not of the relay: `max_connections` 2 with `NM_WORKER_CONCURRENCY` 8 opens
-- eight. `src/relay/transport.rs` says this correctly beside the `pool_config` call, and this is
-- the other half of saying it.
--
-- What holds the product to what a relay will take is `relay_buckets`, which is shared through
-- this database for exactly that reason. The connection cap spec §2.4 asks for is not built;
-- `issues/42` carries it.
--
-- 0028 is not edited to say this: it has been applied and its checksum taken, and `issues/40`
-- exists because an applied migration was edited. Where the two disagree, this wins.
comment on column relays.max_connections is
  'How many SMTP connections to this relay ONE sender process keeps parked for reuse: lettre''s '
  'pool `max_size`, per process. NOT a cap on concurrent connections — a send that finds no '
  'parked connection opens a new one, so the number open at once is bounded by the worker''s '
  'concurrency (`NM_WORKER_CONCURRENCY`), not by this. What keeps the product inside what the '
  'relay will take is `rate_limit_per_minute` and the `relay_buckets` row that spends it.';
