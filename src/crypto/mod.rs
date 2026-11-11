// SPDX-License-Identifier: AGPL-3.0-only

//! Password hashing, secrets at rest, and token helpers.
//!
//! All randomness here (salts, nonces, tokens) comes from the OS CSPRNG via
//! `rand::rngs::SysRng`. Nothing in this module logs or formats key material,
//! plaintexts or tokens.

pub mod password;
pub mod secrets;
pub mod tokens;
