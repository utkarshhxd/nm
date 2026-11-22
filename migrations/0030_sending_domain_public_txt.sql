-- SPDX-License-Identifier: AGPL-3.0-only

-- The DKIM record to publish for a domain's stored key, kept beside the key so that showing it
-- never means decrypting the key.
--
-- The settings list returns this for every row to anyone who may read it, a viewer included.
-- Deriving it on read meant a private key decrypted once per row per request, for a value that
-- only changes when the key does, and plaintext key material in the process for a caller who has
-- no business causing a decryption at all. It is derived once, when the key is saved
-- (`sending_domains::routes`), from the same `mail::dkim::public_key_txt` the signer uses.
--
-- Nothing here can check that the text matches the key: that is the writers' care, and it is the
-- same statement that writes the key that writes this. What the table can refuse is the one
-- drift that is detectable, a published record with no key behind it. The converse (a key with
-- no record) is left alone on purpose: 0028's own tests insert keyed rows by hand, and a row
-- written before this column existed has none. No deployed instance has one: M2a had not shipped
-- when this was added.
alter table sending_domains add column dkim_public_txt text;

alter table sending_domains
  add constraint sending_domains_public_txt_needs_key
  check (dkim_public_txt is null or dkim_private_key_encrypted is not null);

comment on column sending_domains.dkim_public_txt is
  'The `v=DKIM1; k=rsa; p=...` TXT value for the key in `dkim_private_key_encrypted`, derived when '
  'the key was saved. Public by design; the API returns it so an operator can publish it. Null '
  'when there is no key.';
