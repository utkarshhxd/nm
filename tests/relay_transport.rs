// SPDX-License-Identifier: AGPL-3.0-only

//! What the transport records about a submission, driven against `common::smtp_sink`.
//!
//! The ruling under test (spec §5.8): **anything that could have been accepted is `Unknown`, and
//! `Unknown` is never retried.** A person receiving a second copy of a confirmation email is a
//! worse failure than one not arriving, because the second copy is indistinguishable from the
//! product being broken. So a message that could have been retried safely is lost instead.

mod common;

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use common::smtp_sink::{AfterData, Script, Sink};
use lettre::address::Envelope;
use nm::relay::{
    Relay, Security,
    transport::{Outcome, Transports},
};
use uuid::Uuid;

/// How long a worker holds a claimed job before another worker may take the row (Task 8). A
/// `send` that outlives this is how a message goes twice without a bug in the retry logic.
const LEASE: Duration = Duration::from_secs(120);

fn relay(addr: SocketAddr) -> Relay {
    Relay {
        id: Uuid::now_v7(),
        workspace_id: Uuid::now_v7(),
        name: "sink".to_owned(),
        host: addr.ip().to_string(),
        port: i32::from(addr.port()),
        // `plaintext_for_tests` ignores this. It is set to what a real relay carries, so no test
        // here rests on a relay that was configured without TLS.
        security: Security::StartTls,
        username: None,
        max_connections: 1,
        rate_limit_per_minute: 60,
        enabled: true,
        failover_order: 0,
    }
}

fn envelope() -> Envelope {
    Envelope::new(
        Some("news@brand.test".parse().expect("sender")),
        vec!["reader@dest.test".parse().expect("recipient")],
    )
    .expect("envelope")
}

const BYTES: &[u8] =
    b"From: news@brand.test\r\nTo: reader@dest.test\r\nSubject: hi\r\n\r\nthe body\r\n";

/// Submits one message to a sink running `script`, and returns what the transport made of it.
async fn send_to(script: Script) -> Outcome {
    let sink = Sink::start(script).await;
    Transports::plaintext_for_tests()
        .send(&relay(sink.addr), None, &envelope(), BYTES)
        .await
}

/// The text an outcome carries, whichever outcome it is.
fn text(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Sent { response } => response,
        Outcome::Transient(s) | Outcome::Failed(s) | Outcome::Unknown(s) => s,
    }
}

#[tokio::test]
async fn a_250_is_sent() {
    let outcome = send_to(Script::Smtp(AfterData::Reply(
        "250 2.0.0 Ok: queued as ABC123",
    )))
    .await;
    let Outcome::Sent { response } = &outcome else {
        panic!("{outcome:?}");
    };
    // The relay's own words, kept: an operator asking "did it go?" gets the relay's answer, and
    // the queue id is the only thing that ties this row to the relay's log.
    assert!(response.contains("queued as ABC123"), "{response}");
    assert!(response.contains("250"), "{response}");
}

#[tokio::test]
async fn a_451_is_transient() {
    let outcome = send_to(Script::Smtp(AfterData::Reply("451 4.3.0 try again later"))).await;
    assert!(matches!(outcome, Outcome::Transient(_)), "{outcome:?}");
    assert!(text(&outcome).contains("451"), "{outcome:?}");
    assert!(text(&outcome).contains("try again later"), "{outcome:?}");
}

#[tokio::test]
async fn a_550_is_failed() {
    let outcome = send_to(Script::Smtp(AfterData::Reply(
        "550 5.1.1 no such user here",
    )))
    .await;
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert!(text(&outcome).contains("550"), "{outcome:?}");
    assert!(text(&outcome).contains("no such user here"), "{outcome:?}");
}

/// OCI Email Delivery answers `455` when the account is over its rate, and the message has to be
/// offered again later. Classed off the 4xx family, not off a list of codes, so a reply nobody
/// has seen yet lands on the retrying side.
#[tokio::test]
async fn a_455_is_transient() {
    let outcome = send_to(Script::Smtp(AfterData::Reply(
        "455 4.7.1 Server busy, try again later",
    )))
    .await;
    assert!(matches!(outcome, Outcome::Transient(_)), "{outcome:?}");
    assert!(text(&outcome).contains("455"), "{outcome:?}");
}

/// **An acceptance that names a suppression is a delivery failure.** OCI accepts mail to a
/// suppressed address rather than rejecting it, answering `250` with a suppression notice. A
/// pipeline that reads that as delivery lies to the operator about who received mail.
#[tokio::test]
async fn a_250_naming_a_suppression_is_failed() {
    let reply = "250 2.0.0 Ok: Suppression for user reader@dest.test";
    let outcome = send_to(Script::Smtp(AfterData::Reply(reply))).await;
    assert!(
        !matches!(outcome, Outcome::Sent { .. }),
        "a suppression notice was recorded as a delivery: {outcome:?}"
    );
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert!(
        text(&outcome).contains("Suppression for user reader@dest.test"),
        "the reason does not carry the relay's reply: {outcome:?}"
    );
}

/// The relay read the whole message and then went away without answering. It may have queued it.
/// Retrying is how the same person gets it twice, so this is never retried.
#[tokio::test]
async fn a_drop_after_data_is_unknown() {
    let outcome = send_to(Script::Smtp(AfterData::Drop)).await;
    assert!(
        matches!(outcome, Outcome::Unknown(_)),
        "a connection lost after the body was submitted must not be retried: {outcome:?}"
    );
}

/// Nothing was submitted, so there is nothing to be ambiguous about: try again.
#[tokio::test]
async fn a_refused_connection_is_transient() {
    let addr = Sink::unused_addr().await;
    let outcome = Transports::plaintext_for_tests()
        .send(&relay(addr), None, &envelope(), BYTES)
        .await;
    assert!(matches!(outcome, Outcome::Transient(_)), "{outcome:?}");
}

/// **Review Focus 1.** The sink accepts the connection and then says nothing, ever. With no
/// timeout of our own, lettre's async client waits forever: a worker tick blocks past its lease,
/// another worker re-claims the job row, and the message goes twice — the one outcome the spec
/// names as worse than not sending, arriving by way of a hang rather than a retry bug.
///
/// The assertion is a bound, not an equality: how long the send takes depends on the machine, and
/// the only quantity that carries meaning is whether it finished before the lease ran out.
#[tokio::test]
async fn a_relay_that_never_replies_does_not_outlive_the_lease() {
    let sink = Sink::start(Script::Silence).await;
    let started = Instant::now();
    let outcome = tokio::time::timeout(
        LEASE,
        Transports::plaintext_for_tests().send(&relay(sink.addr), None, &envelope(), BYTES),
    )
    .await
    .expect("send outlived the 120 s job lease: another worker would re-claim the row");
    let elapsed = started.elapsed();
    assert!(elapsed < LEASE, "{elapsed:?}");
    assert!(
        matches!(outcome, Outcome::Unknown(_)),
        "a hang could have been a hang after DATA, and so could have been accepted: {outcome:?}"
    );
}

/// The transport keeps one pooled client per relay id. If the operator moves the relay to another
/// host, mail has to follow — a cached client would keep submitting to the old one.
#[tokio::test]
async fn a_relay_that_moved_is_not_submitted_to_the_old_host() {
    let old = Sink::start(Script::Smtp(AfterData::Reply("250 2.0.0 Ok: the old host"))).await;
    let new = Sink::start(Script::Smtp(AfterData::Reply("250 2.0.0 Ok: the new host"))).await;
    let transports = Transports::plaintext_for_tests();

    let mut relay = relay(old.addr);
    let first = transports.send(&relay, None, &envelope(), BYTES).await;
    assert!(text(&first).contains("the old host"), "{first:?}");

    relay.port = i32::from(new.addr.port());
    let second = transports.send(&relay, None, &envelope(), BYTES).await;
    assert!(
        text(&second).contains("the new host"),
        "the move was ignored and the message went to the old host: {second:?}"
    );
}

/// Spec §8, invariant 7: a relay's password reaches no log line, no audit row and no error. The
/// sink offers no authentication mechanism, so the attempt fails and the failure is turned into
/// an outcome — which is the one place a configured password could escape as text.
#[tokio::test]
async fn a_failure_to_authenticate_does_not_carry_the_password() {
    const PASSWORD: &str = "hunter2-correct-horse";
    let sink = Sink::start(Script::Smtp(AfterData::Reply("250 2.0.0 Ok"))).await;
    let mut relay = relay(sink.addr);
    relay.username = Some("ocid1.user.oc1..smtp".to_owned());
    let outcome = Transports::plaintext_for_tests()
        .send(&relay, Some(PASSWORD), &envelope(), BYTES)
        .await;
    assert!(
        !text(&outcome).contains(PASSWORD),
        "the password reached an outcome's text"
    );
    // Nothing was submitted: the session never opened.
    assert!(matches!(outcome, Outcome::Transient(_)), "{outcome:?}");
}
