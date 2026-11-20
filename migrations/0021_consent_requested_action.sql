-- SPDX-License-Identifier: AGPL-3.0-only

-- A membership that lands in `pending` is somebody who was ASKED, not somebody who agreed.
-- `0020` had no word for that, so a pending membership recorded `subscribed` — byte-identical to a
-- real single opt-in, with only `lists.double_optin` telling them apart. That flag is mutable and
-- unversioned, so flipping it later retroactively turned unconfirmed rows into proof of consent.
-- `requested` puts the distinction in the row itself, where nothing can change it afterwards.
--
-- `0020` is not amended: it has run. The check is dropped and re-added under the same name, which
-- names a column and a table and so names no person — a violation of it is echoed into the log by
-- `consent::scrub`.
alter table consent_log drop constraint consent_log_action_check;
alter table consent_log add constraint consent_log_action_check
  check (action in ('requested','subscribed','confirmed','unsubscribed','resubscribed','suppressed'));
