// SPDX-License-Identifier: AGPL-3.0-only

//! Password hashing, secrets at rest, and token helpers.
//!
//! All randomness here (salts, nonces, tokens) comes from the OS CSPRNG via
//! `rand::rngs::SysRng`. Nothing in this module logs or formats key material,
//! plaintexts or tokens.

pub mod password;
pub mod secrets;
pub mod tokens;

use secrets::Secrets;

/// Domain-separation label for the suppression-hash key.
///
/// **ONE-WAY DOOR. Never change this string.** `suppressions.email_hmac` holds HMAC-SHA256 of an
/// address under a key derived with this label, and an erasure tombstone has no plaintext left to
/// recompute it from. Changing the label makes every stored hash unreproducible: `is_suppressed`
/// would stop matching them, and every erased person (and anyone else whose row was hashed) would
/// silently become mailable again. `suppression_hmac_is_pinned` fails if this changes.
const SUPPRESSION_DOMAIN: &str = "nm:suppression:v1";

/// HMAC-SHA256 of an address that `users::normalize_email` has already normalised, keyed from the
/// instance secret and domain-separated by `SUPPRESSION_DOMAIN`. A keyed MAC and not a plain digest:
/// addresses are an enumerable space, so a bare SHA-256 of a suppression list would fall to a
/// dictionary and a leaked table would name who complained or asked to be erased.
pub fn suppression_hmac(secrets: &Secrets, normalised_email: &str) -> [u8; 32] {
    tokens::hmac_sha256(
        secrets.derive_key(SUPPRESSION_DOMAIN).as_slice(),
        normalised_email.as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecretKey;

    fn hex(h: [u8; 32]) -> String {
        h.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Pins the one-way door. The expected value was computed independently with
    /// `openssl dgst -sha256 -mac HMAC`, not with this code: a subkey of HMAC(master, label), then
    /// HMAC(subkey, address).
    #[test]
    fn suppression_hmac_is_pinned() {
        let secrets = Secrets::new(&SecretKey([7u8; 32]));
        assert_eq!(
            hex(suppression_hmac(&secrets, "a@b.test")),
            "7345f6dcf1b2d147b1ce893ae0215e75692ca58e8f23a8ac2ab2a087ca36d440"
        );
    }

    #[test]
    fn suppression_hmac_depends_on_the_key_and_the_address() {
        let a = Secrets::new(&SecretKey([7u8; 32]));
        let b = Secrets::new(&SecretKey([8u8; 32]));
        assert_ne!(
            suppression_hmac(&a, "x@y.test"),
            suppression_hmac(&b, "x@y.test")
        );
        assert_ne!(
            suppression_hmac(&a, "x@y.test"),
            suppression_hmac(&a, "z@y.test")
        );
        assert_ne!(
            suppression_hmac(&a, "x@y.test"),
            tokens::sha256(b"x@y.test"),
            "must not be a bare digest"
        );
    }
}
