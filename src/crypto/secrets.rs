// SPDX-License-Identifier: AGPL-3.0-only

use std::fmt;

use aes_gcm::{
    Aes256Gcm, Key, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use rand::{TryRng, rngs::SysRng};

use crate::config::SecretKey;

const VERSION: u8 = 0x01;
const KEY_ID: u8 = 0x01;
const HEADER_LEN: usize = 2;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("encrypted blob is too short")]
    Truncated,
    #[error("unsupported encrypted blob version")]
    UnsupportedVersion,
    #[error("unsupported encryption key id")]
    UnsupportedKeyId,
    /// Wrong key or modified data; the two are deliberately not told apart.
    #[error("decryption failed")]
    Authentication,
}

/// AES-256-GCM under the master key.
///
/// Blob layout: `[version 0x01][key id 0x01][12-byte nonce][ciphertext || tag]`.
/// The version and key id are authenticated as associated data.
pub struct Secrets {
    cipher: Aes256Gcm,
}

/// Hand-written so the key can never reach a log through `{:?}`.
impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secrets(***)")
    }
}

impl Secrets {
    pub fn new(key: &SecretKey) -> Secrets {
        Secrets {
            cipher: Aes256Gcm::new(&Key::<Aes256Gcm>::from(key.0)),
        }
    }

    /// Encrypts under a fresh random nonce, so equal plaintexts give different blobs.
    pub fn encrypt(&self, plain: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        // Fail closed: a broken OS RNG must never yield a predictable nonce.
        SysRng
            .try_fill_bytes(&mut nonce)
            .expect("OS RNG unavailable");
        let aad = [VERSION, KEY_ID];
        let sealed = self
            .cipher
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: plain,
                    aad: &aad,
                },
            )
            .expect("AES-GCM encryption only fails on inputs beyond 2^36 bytes");
        let mut blob = Vec::with_capacity(HEADER_LEN + NONCE_LEN + sealed.len());
        blob.extend_from_slice(&aad);
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&sealed);
        blob
    }

    pub fn decrypt(&self, blob: &[u8]) -> Result<Vec<u8>, SecretsError> {
        let (header, rest) = blob
            .split_first_chunk::<HEADER_LEN>()
            .ok_or(SecretsError::Truncated)?;
        let [version, key_id] = *header;
        if version != VERSION {
            return Err(SecretsError::UnsupportedVersion);
        }
        if key_id != KEY_ID {
            return Err(SecretsError::UnsupportedKeyId);
        }
        let (nonce, sealed) = rest
            .split_first_chunk::<NONCE_LEN>()
            .ok_or(SecretsError::Truncated)?;
        if sealed.len() < TAG_LEN {
            return Err(SecretsError::Truncated);
        }
        self.cipher
            .decrypt(
                &Nonce::from(*nonce),
                Payload {
                    msg: sealed,
                    aad: header,
                },
            )
            .map_err(|_| SecretsError::Authentication)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SecretKey;

    fn secrets() -> Secrets {
        Secrets::new(&SecretKey([7u8; 32]))
    }

    #[test]
    fn secrets_round_trip_with_fresh_nonce() {
        let s = secrets();
        let (a, b) = (s.encrypt(b"x"), s.encrypt(b"x"));
        assert_ne!(a, b);
        assert_eq!(s.decrypt(&a).unwrap(), b"x");
        assert_eq!(s.decrypt(&b).unwrap(), b"x");
        assert_eq!(s.decrypt(&s.encrypt(b"")).unwrap(), b"");
    }

    #[test]
    fn blob_layout() {
        let blob = secrets().encrypt(b"hello");
        assert_eq!(blob[0], 0x01);
        assert_eq!(blob[1], 0x01);
        // header 2 + nonce 12 + plaintext 5 + tag 16
        assert_eq!(blob.len(), 2 + 12 + 5 + 16);
    }

    #[test]
    fn tampered_blob_fails() {
        let s = secrets();
        let blob = s.encrypt(b"secret");
        for i in 0..blob.len() {
            let mut t = blob.clone();
            t[i] ^= 1;
            assert!(s.decrypt(&t).is_err(), "flip at byte {i} was accepted");
        }
    }

    #[test]
    fn unknown_version_or_key_id_fails() {
        let s = secrets();
        let mut blob = s.encrypt(b"x");
        blob[0] = 2;
        assert!(matches!(
            s.decrypt(&blob),
            Err(SecretsError::UnsupportedVersion)
        ));
        let mut blob = s.encrypt(b"x");
        blob[1] = 9;
        assert!(matches!(
            s.decrypt(&blob),
            Err(SecretsError::UnsupportedKeyId)
        ));
    }

    #[test]
    fn other_key_fails() {
        let blob = secrets().encrypt(b"x");
        assert!(Secrets::new(&SecretKey([8u8; 32])).decrypt(&blob).is_err());
    }

    #[test]
    fn short_blobs_error_without_panicking() {
        let s = secrets();
        let blob = s.encrypt(b"x");
        // Every strict prefix is attacker-controlled garbage: must be Err, never a panic.
        for n in 0..blob.len() {
            assert!(
                s.decrypt(&blob[..n]).is_err(),
                "prefix of {n} bytes accepted"
            );
        }
    }

    #[test]
    fn debug_does_not_print_key() {
        assert_eq!(format!("{:?}", secrets()), "Secrets(***)");
    }
}
