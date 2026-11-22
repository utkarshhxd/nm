-- SPDX-License-Identifier: AGPL-3.0-only

-- The explicit acknowledgement spec §6 asks for before sending from a domain whose DNS checks are
-- not passing: somebody with admin looked at the failures and said "send anyway".
--
-- A timestamp and not a boolean, so the question the audit log is asked afterwards, "when did
-- someone accept this", has an answer on the row itself. Null means not acknowledged.
--
-- It is cleared in the same statement that clears `checks` whenever the configuration the checks
-- described is edited (`sending_domains::routes::update_sending_domain`), because an
-- acknowledgement that survives a change is an acknowledgement of something else. Nothing in the
-- database enforces that: the column is as good as the writers' care, and the sender's refusal is
-- what reads it.
alter table sending_domains add column send_unverified_acknowledged_at timestamptz;

comment on column sending_domains.send_unverified_acknowledged_at is
  'When an admin acknowledged sending from this domain while its DNS checks are not all passing. '
  'Null means they have not. Reset whenever `checks` is reset, since it speaks for the configuration '
  'that was in place when it was given. The send-side refusal that reads it lives with the code '
  'that confirms a send, not here.';
