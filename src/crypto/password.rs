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
///
/// Rejects anything `validate_password` rejects, so a weak password cannot be
/// hashed and stored by a caller that forgot to validate. Callers still run
/// `validate_password` first to give the user a field-scoped error.
pub fn hash_password(plain: &str) -> anyhow::Result<String> {
    validate_password(plain).map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let mut salt = [0u8; 16];
    SysRng
        .try_fill_bytes(&mut salt)
        .map_err(|_| anyhow::anyhow!("OS RNG unavailable"))?;
    let phc = hasher()
        .hash_password_with_salt(plain.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hashing failed: {e}"))?;
    Ok(phc.to_string())
}

/// True only if `plain` matches `phc`.
///
/// Input longer than `MAX_CHARS * 4` bytes (the UTF-8 size of the longest valid
/// password) returns false at once: that is a property of the caller's own
/// input, not of the account, so it is no timing oracle and costs no argon2 run.
///
/// Every other failure, including a stored hash that is unparseable, names a
/// different algorithm, or lacks a salt or digest, runs `burn_verify()` first,
/// so it costs one argon2 run like a wrong password.
pub fn verify_password(plain: &str, phc: &str) -> bool {
    if plain.len() > MAX_CHARS * 4 {
        return false;
    }
    match PasswordHash::new(phc) {
        Ok(parsed)
            if parsed.algorithm == Algorithm::Argon2id.ident()
                && parsed.salt.is_some()
                && parsed.hash.is_some() =>
        {
            hasher().verify_password(plain.as_bytes(), &parsed).is_ok()
        }
        _ => {
            // Corrupt or foreign row: nothing to match, but worth an operator's attention.
            tracing::warn!("stored password hash is unparseable");
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
    fn foreign_or_incomplete_phc_is_just_false() {
        let h = hash_password("correct horse battery").unwrap();
        let salt_only = h.rsplit_once('$').unwrap().0.to_owned();
        let no_salt = "$argon2id$v=19$m=19456,t=2,p=1";
        let scrypt =
            "$scrypt$ln=15,r=8,p=1$c2FsdHNhbHRzYWx0$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNoaGFzaGhhc2g";
        for bad in [salt_only.as_str(), no_salt, scrypt] {
            assert!(!verify_password("correct horse battery", bad), "{bad}");
        }
        assert!(verify_password("correct horse battery", &h));
    }

    #[test]
    fn hash_password_enforces_length_bounds() {
        assert!(hash_password("short").is_err());
        assert!(hash_password("").is_err());
        assert!(hash_password(&"a".repeat(257)).is_err());
        assert!(hash_password(&"a".repeat(12)).is_ok());
    }

    #[test]
    fn verify_length_cap_is_in_bytes_not_chars() {
        // 256 four-byte characters = 1024 bytes: the longest valid password still verifies.
        let longest = "\u{1F600}".repeat(256);
        let h = hash_password(&longest).unwrap();
        assert!(verify_password(&longest, &h));
        // One byte over the cap is rejected outright.
        let over = format!("{longest}a");
        assert!(!verify_password(&over, &h));
    }

    #[test]
    fn dummy_hash_is_a_valid_spec_hash() {
        assert!(DUMMY_HASH.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        // Tied to `hasher()`, not a literal: changing the spec params fails this
        // until DUMMY_HASH is regenerated, so burn_verify cannot silently get cheaper.
        let parsed = PasswordHash::new(DUMMY_HASH).unwrap();
        // (Whole-`Params` equality is too strict: a parsed hash records output_len.)
        let (dummy, spec) = (
            Params::try_from(&parsed).unwrap(),
            hasher().params().clone(),
        );
        assert_eq!(
            (dummy.m_cost(), dummy.t_cost(), dummy.p_cost()),
            (spec.m_cost(), spec.t_cost(), spec.p_cost())
        );
        assert!(parsed.salt.is_some() && parsed.hash.is_some());
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
