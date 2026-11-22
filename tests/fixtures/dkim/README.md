<!-- SPDX-License-Identifier: AGPL-3.0-only -->

# DKIM test keys

**Throwaway keys, generated with `openssl` for the tests and committed in the clear. An instance
must never sign with any of them, and none of them may be pasted into a sending domain.**

They exist so the DKIM tests run against real RSA rather than a stand-in: `src/mail/dkim.rs`
signs and verifies with them, and `tests/sending_domains.rs` saves them through the API.

| File | What it is | What it is for |
| --- | --- | --- |
| `signing-2048-pkcs8.pem` | 2048-bit RSA, `BEGIN PRIVATE KEY` | the key most tests sign with |
| `signing-2048-pkcs1.pem` | 2048-bit RSA, `BEGIN RSA PRIVATE KEY` | **a different key**, in the other armour. The armour is the point: a stored key is DER with it stripped, so nothing records which form it arrived in and `key_from_pem` must try both. Different rather than the same key on purpose — `a_key_loads_from_either_armour_and_from_the_der_alone` asserts the two public records differ, which is what stops its main assertion being trivially true |
| `other-2048-pkcs8.pem` | a second 2048-bit RSA key | the key published at a selector that does not match |
| `too-small-1024-pkcs1.pem` | 1024-bit RSA | refused: under DKIM's and `ring`'s floor |
| `not-rsa-ec.pem` | P-256 EC | refused: not RSA |
| `public-2048.pem` | the public half of `signing-2048-pkcs8.pem` | refused: a public key is not a signing key |
