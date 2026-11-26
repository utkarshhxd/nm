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
//! | answered `455`, its own limit (spec M2c §4.7.4) | [`Outcome::Deferred`] | yes, by the next relay or later, no attempt spent |
//! | answered any other 4xx, `421` included | [`Outcome::Transient`] | yes, with backoff |
//! | answered 5xx | [`Outcome::Failed`] | no |
//! | could not be connected to at all | [`Outcome::Deferred`] | yes, as `455` |
//! | refused to negotiate (TLS, AUTH) | [`Outcome::Transient`] | yes |
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

/// How long a whole submission gets, greeting to final reply. Under [`JOB_LEASE`] with room to
/// spare: a send that outlives the lease has its row re-claimed by another worker, and the
/// message goes twice. lettre's async client puts **no** timeout on a read, so without this a
/// relay that accepts the connection and then says nothing waits forever.
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a worker holds a claimed job before another worker may take the row. Named here
/// because [`SESSION_TIMEOUT`] means nothing except against it, and the two cannot be allowed to
/// cross: a test can be told to use a shorter session, but nothing can ship one that is longer
/// than the lease.
pub const JOB_LEASE: Duration = Duration::from_secs(120);

const _: () = assert!(
    SESSION_TIMEOUT.as_secs() < JOB_LEASE.as_secs(),
    "a session may not outlive the job lease: the row would be re-claimed mid-send"
);

// AND THE TIGHTER BOUND, WHICH IS THE ONE THAT ACTUALLY BINDS TODAY. A tick stops starting sends
// once it has spent half the lease (`worker::Worker`'s budget), so the last send it starts may
// begin at `JOB_LEASE / 2` and run for a whole `SESSION_TIMEOUT`. For that send to finish inside
// the lease it claimed under, `SESSION_TIMEOUT <= JOB_LEASE / 2`.
//
// At 60 and 120 that holds with **zero slack**: 60 + 60 is the lease exactly, leaving only the
// round trip that records the outcome. It is to spec and nothing here is being changed to widen
// it — but it means the next person to raise `SESSION_TIMEOUT`, for a relay that is slow to
// answer, silently crosses the lease and starts sending some messages twice. This assertion is
// why that arrives as a compile error with this paragraph attached instead of as a duplicate in
// somebody's inbox. Raising it means raising `JOB_LEASE` to at least twice it.
const _: () = assert!(
    SESSION_TIMEOUT.as_secs() <= JOB_LEASE.as_secs() / 2,
    "a send started at the end of a tick's budget (half the lease) must still finish inside \
     that lease: SESSION_TIMEOUT may not exceed JOB_LEASE / 2"
);

/// The phrase OCI puts in the reply when the recipient is on its suppression list, whatever the
/// code in front of it. Matched case-insensitively against the whole reply.
const SUPPRESSION_NOTICE: &str = "suppression for user";

/// The first words of what lettre writes for a connection that was never made.
///
/// lettre's `Error` exposes `is_transient`, `is_permanent`, `is_client`, `is_tls` and
/// `is_response`, and none of them answers the question this classification turns on: whether the
/// session was ever open. A refused connection and a socket that broke after `DATA` are both
/// "some I/O went wrong", and only the message tells them apart. So this one string is read, and
/// `a_refused_connection_is_deferred` in `tests/relay_transport.rs` goes red the day lettre
/// rewords it — at which point every unreachable relay would start recording `Unknown` instead of
/// retrying, which is a lost message, not a double send.
const NEVER_CONNECTED: &str = "Connection error";

/// How long a relay that answered `455`, or could not be reached, is left alone: its bucket is
/// drained for this long (spec M2c §4.7.4). OCI publishes no `Retry-After`.
pub const DEFER_COOL_DOWN: Duration = Duration::from_secs(60);

/// The cool-down when the `455` says the limit is a daily one, which a minute will not cure.
pub const DAILY_COOL_DOWN: Duration = Duration::from_secs(3_600);

/// What a `455` says when the limit it hit is the day's (OCI's 50,000), matched case-insensitively.
const PER_DAY: &str = "per day";

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
    /// The relay, not the message, could not take it: a `455`, or no connection at all. Nothing
    /// was submitted. Offer it to the next relay, or later, without spending an attempt; this
    /// relay's bucket is drained for `cool_down`.
    Deferred { reason: String, cool_down: Duration },
}

/// One pooled lettre client per relay id, rebuilt when the relay's configuration changes.
pub struct Transports {
    /// Connects in the clear, whatever the relay's `Security` says. Tests only; see
    /// [`Transports::plaintext_for_tests`].
    plaintext: bool,
    /// [`SESSION_TIMEOUT`] everywhere but the test that has to watch one elapse.
    session_timeout: Duration,
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
    /// The password's digest. Not a way to keep the secret out of memory — the client this key
    /// belongs to holds the plaintext in lettre's `Credentials` for exactly as long — but a key
    /// that can be compared, cloned and kept without copying the password around to do it.
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
            session_timeout: SESSION_TIMEOUT,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Bounds a session at something other than [`SESSION_TIMEOUT`].
    ///
    /// For the one test that has to watch a session elapse, which would otherwise take a real
    /// minute. `a_transports_bounds_a_session_at_the_shipped_constant` holds the other end: the
    /// default is the constant, so this cannot quietly become the duration the product ships.
    #[doc(hidden)]
    pub fn with_session_timeout(self, session_timeout: Duration) -> Transports {
        Transports {
            session_timeout,
            ..self
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
            Err(outcome) => return outcome,
        };
        match tokio::time::timeout(self.session_timeout, transport.send_raw(envelope, bytes)).await
        {
            Ok(Ok(response)) => accepted(&response),
            Ok(Err(e)) => refused(&e),
            Err(_elapsed) => {
                // The connection was abandoned mid-conversation, and lettre parks it back in its
                // pool none the wiser. The next message would be handed that connection, answered
                // by whatever the relay still owes this one, and time out as well — one stalled
                // send becoming a run of them. Dropping the client drops the pool with it.
                self.evict(relay.id);
                // Where it hung is not visible from out here: it could have been the greeting,
                // and it could have been the reply to a message the relay had already queued.
                Outcome::Unknown(format!(
                    "{} did not answer within {} s; the message may have been accepted",
                    relay.host,
                    self.session_timeout.as_secs()
                ))
            }
        }
    }

    /// Forgets the pooled client for `relay_id`. The next message builds a new one.
    fn evict(&self, relay_id: Uuid) {
        self.clients
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&relay_id);
    }

    /// The pooled client for `relay`, built if this is the first message or if anything about the
    /// connection has changed since the last one.
    fn client(
        &self,
        relay: &Relay,
        password: Option<&str>,
    ) -> Result<AsyncSmtpTransport<Tokio1Executor>, Outcome> {
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

/// A client for `relay`, or the outcome to record instead.
///
/// The two ways this fails are not the same failure. A port outside `u16` is a relay that cannot
/// be connected to however often it is tried. Building the TLS parameters reads the machine's
/// certificate store and its crypto provider, and that can fail for reasons that have nothing to
/// do with this message — recording `Failed` there would destroy a message over an environment
/// hiccup, since `Failed` means never offer it again.
fn build(
    relay: &Relay,
    password: Option<&str>,
    plaintext: bool,
) -> Result<AsyncSmtpTransport<Tokio1Executor>, Outcome> {
    let port = u16::try_from(relay.port)
        .map_err(|_| Outcome::Failed(format!("port {} is not a port", relay.port)))?;
    // The fence under `plaintext_for_tests`: in the clear, this reaches the loopback interface and
    // nothing else. A relay's password on an open socket to another machine is the one way that
    // constructor could do harm, and it cannot be reached from here.
    if plaintext && !is_loopback(&relay.host) {
        return Err(Outcome::Failed(format!(
            "{} is not loopback: a plaintext transport may not leave this machine",
            relay.host
        )));
    }
    let mut builder = if plaintext {
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&relay.host)
    } else {
        match relay.security {
            Security::ImplicitTls => AsyncSmtpTransport::<Tokio1Executor>::relay(&relay.host),
            Security::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&relay.host),
        }
        .map_err(|e| Outcome::Transient(format!("{} cannot be connected to: {e}", relay.host)))?
    };
    builder = builder
        .port(port)
        // lettre uses this for the connect, and for nothing else in the async client: everything
        // after it is bounded by `SESSION_TIMEOUT` in `send`.
        .timeout(Some(CONNECT_TIMEOUT))
        // This does not cap concurrent sessions, whatever the column is called: lettre opens a
        // new connection whenever the parked list is empty, and `max_size` only bounds how many
        // are kept on recycle. What holds the product to what a relay will take is the token
        // bucket, not this.
        .pool_config(PoolConfig::new().max_size(relay.max_connections.max(1) as u32));
    // No username, no AUTH. A username with no password is a half-configured relay, and offering
    // an empty password in its place would just turn that into a credential error.
    if let (Some(username), Some(password)) = (relay.username.as_deref(), password) {
        builder = builder.credentials(Credentials::new(username.to_owned(), password.to_owned()));
    }
    Ok(builder.build())
}

/// Whether `host` names this machine. A name other than `localhost` is not resolved: a plaintext
/// transport is for an in-process sink, and anything that needs resolving is not one.
fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
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
    if e.is_transient() && e.status().is_some_and(|code| code.to_string() == "455") {
        // OCI's own limit: the relay is full, the message is fine. Wait out the day when it says so.
        let cool_down = if reason.to_ascii_lowercase().contains(PER_DAY) {
            DAILY_COOL_DOWN
        } else {
            DEFER_COOL_DOWN
        };
        Outcome::Deferred { reason, cool_down }
    } else if e.is_transient() {
        // Any other 4xx to any command, `421` included.
        Outcome::Transient(reason)
    } else if e.is_permanent() {
        Outcome::Failed(reason)
    } else if reason.starts_with(NEVER_CONNECTED) {
        // No session at all, so nothing was submitted: another relay, or this one later.
        Outcome::Deferred {
            reason,
            cool_down: DEFER_COOL_DOWN,
        }
    } else if e.is_tls() || e.is_client() || e.is_transport_shutdown() {
        // The session was refused before the message was offered. Nothing was submitted, so
        // there is nothing to be ambiguous about.
        Outcome::Transient(reason)
    } else {
        // A broken socket or a reply that could not be read. lettre reports a connection lost
        // before the greeting exactly as it reports one lost after the terminating `.`, so this
        // is read as the second: the relay may have queued the message.
        Outcome::Unknown(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> Relay {
        Relay {
            id: Uuid::now_v7(),
            workspace_id: Uuid::now_v7(),
            name: "oci".to_owned(),
            host: "smtp.email.eu-frankfurt-1.oci.oraclecloud.com".to_owned(),
            port: 587,
            security: Security::StartTls,
            username: Some("ocid1.user.oc1..smtp".to_owned()),
            max_connections: 4,
            rate_limit_per_minute: 1800,
            enabled: true,
            failover_order: 0,
        }
    }

    /// **Everything a connection is made from is in the key.** The client is kept per relay id
    /// and reused until the key changes, so a part left out of it is a part an operator can
    /// change to no effect — rotate the password and that workspace's mail stops going out, the
    /// old secret still being offered, with nothing in the product to explain it. No sink can
    /// catch this: the cached client is the one thing a test that drives `send` cannot see.
    #[test]
    fn everything_a_connection_is_made_from_is_in_the_key() {
        let base = relay();
        let key = Key::of(&base, Some("pw"));
        assert!(
            key == Key::of(&base, Some("pw")),
            "an unchanged relay must reuse its client"
        );

        let moved = |what: &str, change: fn(&mut Relay)| {
            let mut relay = base.clone();
            change(&mut relay);
            assert!(
                key != Key::of(&relay, Some("pw")),
                "a changed {what} must build a new client"
            );
        };
        moved("host", |r| r.host = "smtp.example.test".to_owned());
        moved("port", |r| r.port = 465);
        moved("security", |r| r.security = Security::ImplicitTls);
        moved("username", |r| r.username = None);
        moved("max_connections", |r| r.max_connections = 8);

        // The password is not on the relay row, so it is passed beside it.
        assert!(
            key != Key::of(&base, Some("rotated")),
            "a rotated password must build a new client"
        );
        assert!(
            key != Key::of(&base, None),
            "a password that was taken away must build a new client"
        );
    }

    /// The other end of [`Transports::with_session_timeout`]: whatever a test injects, what the
    /// product builds for itself is [`SESSION_TIMEOUT`].
    #[test]
    fn a_transports_bounds_a_session_at_the_shipped_constant() {
        assert_eq!(Transports::new().session_timeout, SESSION_TIMEOUT);
        assert_eq!(Transports::default().session_timeout, SESSION_TIMEOUT);
        assert_eq!(
            Transports::plaintext_for_tests().session_timeout,
            SESSION_TIMEOUT
        );
    }

    /// A port that is not a port will never become one; the certificate store that the TLS
    /// parameters are built from can be a bad minute on this machine. Only the first is `Failed`.
    /// The second is not exercised here: making it fail means breaking the host's cert store.
    #[test]
    fn a_port_that_is_not_a_port_is_failed_not_transient() {
        let mut relay = relay();
        relay.port = 70_000;
        assert!(matches!(
            build(&relay, None, false),
            Err(Outcome::Failed(_))
        ));
    }

    #[test]
    fn a_plaintext_transport_is_fenced_to_this_machine() {
        for here in ["127.0.0.1", "::1", "localhost"] {
            assert!(is_loopback(here), "{here}");
        }
        for away in ["smtp.example.test", "10.0.0.4", "0.0.0.0"] {
            assert!(!is_loopback(away), "{away}");
            let mut relay = relay();
            relay.host = away.to_owned();
            assert!(
                matches!(build(&relay, Some("pw"), true), Err(Outcome::Failed(_))),
                "a plaintext transport was built for {away}"
            );
        }
    }
}
