# 0002. Suppression hashing

Status: accepted (M1a). Spec: `.scratch/email-platform/specs/2026-11-11-m1a-contact-core.md` §6. Code: `src/crypto/mod.rs`, `src/suppressions/mod.rs`.

## Decision

`suppressions.email_hmac` is HMAC-SHA256 of the normalised address, under a key derived from the instance secret and domain-separated with the constant `nm:suppression:v1`. Plaintext addresses are kept for ordinary suppressions, because an admin manages and exports their blocklist. The hash exists for the one row type whose plaintext must be destroyed: the GDPR erasure tombstone, which keeps the block alive after the address itself is erased.

**Domain suppression is exact-match.** A `domain` row for `spam.test` blocks addresses at `spam.test` only. `mail.spam.test` stays mailable. A send-time check must not assume subdomains are covered.

## Alternative rejected

A plain SHA-256 of the address. Email addresses are an enumerable space, so a leaked suppression table falls to a dictionary attack and discloses precisely who complained or asked to be erased. A keyed HMAC makes the hashes useless without the instance secret.

Subdomain-inclusive domain matching was also rejected: it blocks more than the admin named, and a suffix match needs a public-suffix list to avoid blocking `co.uk`.

## This is a one-way door

The hashes cannot be recomputed from anything stored, because erasure destroyed the address. If `nm:suppression:v1` changes, or the key derivation changes, every stored hash becomes unreproducible. New lookups no longer match the old rows, and **every erased person silently becomes mailable again**, with no error anywhere. The constant is pinned by the test `suppression_hmac_is_pinned`. A future scheme must add a `v2` alongside `v1`, check both, and never replace `v1`.

Losing the instance secret already loses the encrypted relay credentials, so this adds no new catastrophic-loss mode. It does add a quieter one: a restore under a different secret unblocks the erased.

## Cost if wrong

Exact-match domains cost an admin one row per subdomain they want blocked. If that proves too narrow, add an `include_subdomains` flag; the stored rows stay valid. A wrong hash scheme cannot be corrected after the fact.

Keeping an HMAC of an erased address is standard practice and generally defended under GDPR Article 17(3). It has not been reviewed by counsel (spec §16.1). Dropping it means an erased person can be re-imported and mailed.
