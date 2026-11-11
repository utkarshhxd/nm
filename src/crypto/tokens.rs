// SPDX-License-Identifier: AGPL-3.0-only

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use rand::{TryRng, rngs::SysRng};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// `bytes` random bytes from the OS CSPRNG, base64url without padding.
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // Fail closed: a broken OS RNG must never yield a predictable token.
    SysRng.try_fill_bytes(&mut buf).expect("OS RNG unavailable");
    URL_SAFE_NO_PAD.encode(buf)
}

pub fn sha256(input: &[u8]) -> [u8; 32] {
    Sha256::digest(input).into()
}

/// HMAC-SHA256 (RFC 2104). Any key length is valid for HMAC.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

/// Constant-time in the contents. Unequal lengths return false at once, which
/// reveals only the length: callers compare fixed-size digests or tokens.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_token_url_safe_and_unique() {
        let a = random_token(32);
        assert_eq!(a.len(), 43);
        assert!(!a.contains(['+', '/', '=']));
        assert_ne!(a, random_token(32));
    }

    #[test]
    fn sha256_known_vector() {
        let h = sha256(b"abc");
        assert_eq!(
            h.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// RFC 4231, test case 2.
    #[test]
    fn hmac_sha256_known_vector() {
        let h = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            h.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn constant_time_eq_compares_content_and_length() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abc"));
    }
}
