// SPDX-License-Identifier: AGPL-3.0-only

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use rand::{TryRng, rngs::SysRng};

use crate::error::FieldError;

const MIN_CHARS: usize = 12;
const MAX_CHARS: usize = 256;

/// A real hash of a throwaway password, made with the spec parameters. Used
/// only by `burn_verify`; it matches nothing anyone can sign in with.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$Re/u29ckp2jwMTCwZyRYvQ$CaJ4cmXyt6NmQ/CYZirkOmTLnBRaKvIg30HJX9BPnPk";

/// argon2id, m=19456 KiB, t=2, p=1. Set explicitly so a change to the crate's
/// defaults cannot silently weaken the spec parameters.
fn hasher() -> Argon2<'static> {
    let params = Params::new(19456, 2, 1, None).expect("valid argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash with a fresh 16-byte salt from the OS CSPRNG. Returns a PHC string.
pub fn hash_password(plain: &str) -> anyhow::Result<String> {
    let mut salt = [0u8; 16];
    SysRng
        .try_fill_bytes(&mut salt)
        .map_err(|_| anyhow::anyhow!("OS RNG unavailable"))?;
    let phc = hasher()
        .hash_password_with_salt(plain.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hashing failed: {e}"))?;
    Ok(phc.to_string())
}

/// True only if `plain` matches `phc`. A malformed `phc` costs one full
/// argon2 run, like a wrong password, so the two are not told apart by timing.
pub fn verify_password(plain: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => hasher().verify_password(plain.as_bytes(), &parsed).is_ok(),
        Err(_) => {
            burn_verify();
            false
        }
    }
}

/// One argon2 verification against a fixed dummy hash, so an unknown user
/// costs the same as a known user with a wrong password.
pub fn burn_verify() {
    let parsed = PasswordHash::new(DUMMY_HASH).expect("DUMMY_HASH is a valid PHC string");
    let _ = hasher().verify_password(b"burn-verify-never-matches", &parsed);
}

/// 12..=256 characters (not bytes).
pub fn validate_password(plain: &str) -> Result<(), FieldError> {
    // `take` bounds the scan: an enormous input is not walked to the end.
    let n = plain.chars().take(MAX_CHARS + 1).count();
    if n < MIN_CHARS {
        Err(FieldError::new(
            "password",
            format!("must be at least {MIN_CHARS} characters"),
        ))
    } else if n > MAX_CHARS {
        Err(FieldError::new(
            "password",
            format!("must be at most {MAX_CHARS} characters"),
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_uses_spec_params() {
        let h = hash_password("correct horse battery").unwrap();
        assert!(h.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(verify_password("correct horse battery", &h) && !verify_password("wrong", &h));
    }

    #[test]
    fn hashes_are_salted() {
        let a = hash_password("correct horse battery").unwrap();
        let b = hash_password("correct horse battery").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn malformed_phc_is_just_false() {
        assert!(!verify_password("x", ""));
        assert!(!verify_password("x", "not a phc string"));
        assert!(!verify_password("x", "$argon2id$v=19$m=19456,t=2,p=1$"));
    }

    #[test]
    fn dummy_hash_is_a_valid_spec_hash() {
        assert!(DUMMY_HASH.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        // Parses and is a real verification, not an early parse failure.
        assert!(!verify_password("anything", DUMMY_HASH));
        burn_verify();
    }

    #[test]
    fn password_length_bounds() {
        let f = |n: usize| validate_password(&"a".repeat(n));
        assert!(f(11).is_err());
        assert!(f(12).is_ok());
        assert!(f(256).is_ok());
        assert!(f(257).is_err());
        // 11 characters, 22 bytes: counts characters, not bytes.
        assert!(validate_password("ééééééééééé").is_err());
        // 12 characters, 24 bytes.
        assert!(validate_password("éééééééééééé").is_ok());
        let e = validate_password("short").unwrap_err();
        assert_eq!(e.field, "password");
    }
}
