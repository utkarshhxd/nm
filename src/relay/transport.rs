// SPDX-License-Identifier: AGPL-3.0-only

//! Submitting a message to a relay, and deciding what happened to it.
//!
//! **Anything that could have been accepted is [`Outcome::Unknown`], and an `Unknown` is never
//! retried** (spec §5.8). A person receiving a *second* copy of a confirmation email is a worse
//! failure than one not arriving, because the second copy is indistinguishable from the product
//! being broken. This module therefore loses a message that could have been retried safely rather
//! than risk sending it twice:
//!
//! | the relay | the outcome | retried |
//! |---|---|---|
//! | accepted it (2xx) | [`Outcome::Sent`] | no |
//! | answered 4xx, `421` and `455` included | [`Outcome::Transient`] | yes, with backoff |
//! | answered 5xx | [`Outcome::Failed`] | no |
//! | could not be connected to, or refused to negotiate | [`Outcome::Transient`] | yes |
//! | went quiet, or the socket broke | [`Outcome::Unknown`] | **never** |
//!
//! And one that reads backwards: **an acceptance that names a suppression is a failure.** OCI
//! Email Delivery accepts mail to a suppressed address rather than rejecting it, answering `250`
//! with a suppression notice. Recording that as a delivery would tell the operator that someone
//! received mail who did not.

use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
    time::Duration,
};

use lettre::{
    AsyncTransport, Tokio1Executor,
    address::Envelope,
    transport::smtp::{
        AsyncSmtpTransport, Error as SmtpError, PoolConfig, authentication::Credentials,
        response::Response,
    },
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{Relay, Security};

/// How long the TCP connection and the TLS handshake get.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a whole submission gets, greeting to final reply. Under the worker's 120-second job
/// lease with room to spare: a send that outlives the lease has its row re-claimed by another
/// worker, and the message goes twice. lettre's async client puts **no** timeout on a read, so
/// without this a relay that accepts the connection and then says nothing waits forever.
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(60);

/// The phrase OCI puts in the reply when the recipient is on its suppression list, whatever the
/// code in front of it. Matched case-insensitively against the whole reply.
const SUPPRESSION_NOTICE: &str = "suppression for user";

/// The first words of what lettre writes for a connection that was never made.
///
/// lettre's `Error` exposes `is_transient`, `is_permanent`, `is_client`, `is_tls` and
/// `is_response`, and none of them answers the question this classification turns on: whether the
/// session was ever open. A refused connection and a socket that broke after `DATA` are both
/// "some I/O went wrong", and only the message tells them apart. So this one string is read, and
/// `a_refused_connection_is_transient` in `tests/relay_transport.rs` goes red the day lettre
/// rewords it — at which point every unreachable relay would start recording `Unknown` instead of
/// retrying, which is a lost message, not a double send.
const NEVER_CONNECTED: &str = "Connection error";

/// What the relay did with one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Accepted. `response` is the relay's own reply, which is what ties this message to its log.
    Sent { response: String },
    /// Offer it again later.
    Transient(String),
    /// It will never be accepted. Do not offer it again.
    Failed(String),
    /// It may have been accepted. **Do not offer it again.**
    Unknown(String),
}

/// One pooled lettre client per relay id, rebuilt when the relay's configuration changes.
pub struct Transports {
    /// Connects in the clear, whatever the relay's `Security` says. Tests only; see
    /// [`Transports::plaintext_for_tests`].
    plaintext: bool,
    clients: Mutex<HashMap<Uuid, Client>>,
}

struct Client {
    key: Key,
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

/// Everything about a relay that decides how a connection is made and who it is made as. The
/// client holds parked connections built from these, so a change to any of them needs a new one.
#[derive(PartialEq, Eq)]
struct Key {
    host: String,
    port: i32,
    security: Security,
    username: Option<String>,
    max_connections: i32,
    /// The password's digest, never the password: this is held for as long as the process runs,
    /// and spec §8 invariant 7 keeps a relay's password out of everything but the SMTP session.
    password: Option<[u8; 32]>,
}

impl Key {
    fn of(relay: &Relay, password: Option<&str>) -> Key {
        Key {
            host: relay.host.clone(),
            port: relay.port,
            security: relay.security,
            username: relay.username.clone(),
            max_connections: relay.max_connections,
            password: password.map(|p| Sha256::digest(p.as_bytes()).into()),
        }
    }
}

impl Default for Transports {
    fn default() -> Transports {
        Transports::new()
    }
}

impl Transports {
    pub fn new() -> Transports {
        Transports {
            plaintext: false,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// A [`Transports`] that connects in the clear and ignores every relay's [`Security`].
    ///
    /// It exists for `tests/relay_transport.rs`, whose in-process sink speaks plain TCP on
    /// `127.0.0.1` and cannot present a certificate a TLS client would accept. Nothing in the
    /// product calls it and nothing should: it would put a relay's password on an open socket.
    #[doc(hidden)]
    pub fn plaintext_for_tests() -> Transports {
        Transports {
            plaintext: true,
            ..Transports::new()
        }
    }

    /// Submits `bytes` to `relay` and says what became of them. Never panics and never waits
    /// longer than [`SESSION_TIMEOUT`].
    pub async fn send(
        &self,
        relay: &Relay,
        password: Option<&str>,
        envelope: &Envelope,
        bytes: &[u8],
    ) -> Outcome {
        let transport = match self.client(relay, password) {
            Ok(transport) => transport,
            // The relay cannot be connected to as configured. Offering it again changes nothing.
            Err(reason) => return Outcome::Failed(reason),
        };
        match tokio::time::timeout(SESSION_TIMEOUT, transport.send_raw(envelope, bytes)).await {
            Ok(Ok(response)) => accepted(&response),
            Ok(Err(e)) => refused(&e),
            // Where it hung is not visible from out here: it could have been the greeting, and it
            // could have been the reply to a message the relay had already queued.
            Err(_elapsed) => Outcome::Unknown(format!(
                "{} did not answer within {} s; the message may have been accepted",
                relay.host,
                SESSION_TIMEOUT.as_secs()
            )),
        }
    }

    /// The pooled client for `relay`, built if this is the first message or if anything about the
    /// connection has changed since the last one.
    fn client(
        &self,
        relay: &Relay,
        password: Option<&str>,
    ) -> Result<AsyncSmtpTransport<Tokio1Executor>, String> {
        let key = Key::of(relay, password);
        // A `std::sync::Mutex` on an async worker: the critical section builds a client and clones
        // an `Arc`, and never awaits. A poisoned lock means another send panicked, which says
        // nothing about the map.
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(client) = clients.get(&relay.id)
            && client.key == key
        {
            return Ok(client.transport.clone());
        }
        let transport = build(relay, password, self.plaintext)?;
        clients.insert(
            relay.id,
            Client {
                key,
                transport: transport.clone(),
            },
        );
        Ok(transport)
    }
}

fn build(
    relay: &Relay,
    password: Option<&str>,
    plaintext: bool,
) -> Result<AsyncSmtpTransport<Tokio1Executor>, String> {
    let port =
        u16::try_from(relay.port).map_err(|_| format!("port {} is not a port", relay.port))?;
    let mut builder = if plaintext {
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&relay.host)
    } else {
        match relay.security {
            Security::ImplicitTls => AsyncSmtpTransport::<Tokio1Executor>::relay(&relay.host),
            Security::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&relay.host),
        }
        .map_err(|e| format!("{} cannot be connected to: {e}", relay.host))?
    };
    builder = builder
        .port(port)
        // lettre uses this for the connect, and for nothing else in the async client: everything
        // after it is bounded by `SESSION_TIMEOUT` in `send`.
        .timeout(Some(CONNECT_TIMEOUT))
        .pool_config(PoolConfig::new().max_size(relay.max_connections.max(1) as u32));
    // No username, no AUTH. A username with no password is a half-configured relay, and offering
    // an empty password in its place would just turn that into a credential error.
    if let (Some(username), Some(password)) = (relay.username.as_deref(), password) {
        builder = builder.credentials(Credentials::new(username.to_owned(), password.to_owned()));
    }
    Ok(builder.build())
}

/// The relay took the message — unless the reply says it went to a suppressed address.
fn accepted(response: &Response) -> Outcome {
    let mut reply = response.code().to_string();
    for line in response.message() {
        reply.push(' ');
        reply.push_str(line);
    }
    if reply.to_ascii_lowercase().contains(SUPPRESSION_NOTICE) {
        Outcome::Failed(reply)
    } else {
        Outcome::Sent { response: reply }
    }
}

/// What the relay said, or what went wrong in the session, read for whether the message could
/// still have been accepted.
fn refused(e: &SmtpError) -> Outcome {
    let reason = e.to_string();
    if e.is_transient() {
        // A 4xx to any command, `421` and OCI's `455` included.
        Outcome::Transient(reason)
    } else if e.is_permanent() {
        Outcome::Failed(reason)
    } else if e.is_tls()
        || e.is_client()
        || e.is_transport_shutdown()
        || reason.starts_with(NEVER_CONNECTED)
    {
        // Never connected, or the session was refused before the message was offered. Nothing
        // was submitted, so there is nothing to be ambiguous about.
        Outcome::Transient(reason)
    } else {
        // A broken socket or a reply that could not be read. lettre reports a connection lost
        // before the greeting exactly as it reports one lost after the terminating `.`, so this
        // is read as the second: the relay may have queued the message.
        Outcome::Unknown(reason)
    }
}
