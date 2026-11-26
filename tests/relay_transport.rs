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
    transport::{DAILY_COOL_DOWN, DEFER_COOL_DOWN, JOB_LEASE, Outcome, Transports},
};
use uuid::Uuid;

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

/// What the two tests that have to watch a session elapse bound it at, in place of the minute the
/// product uses. Long enough that a loopback connection is established first, short enough that
/// the suite does not wait.
const INJECTED_SESSION: Duration = Duration::from_secs(1);

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
        Outcome::Deferred { reason, .. } => reason,
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

/// OCI Email Delivery answers `455` when the account is over its rate (spec M2c §4.7.4). That is the
/// relay's limit, not the message's: `Deferred`, which spends no attempt, with the relay left alone
/// for a minute — or for an hour when the reply says the limit is the day's, which a minute will not
/// cure. Every other 4xx stays `Transient`, so a reply nobody has seen yet lands on the retrying
/// side.
#[tokio::test]
async fn a_455_is_deferred_for_a_minute_and_a_daily_455_for_an_hour() {
    let outcome = send_to(Script::Smtp(AfterData::Reply(
        "455 4.7.1 Server busy, try again later",
    )))
    .await;
    assert!(
        matches!(&outcome, Outcome::Deferred { cool_down, .. } if *cool_down == DEFER_COOL_DOWN),
        "{outcome:?}"
    );
    assert_eq!(DEFER_COOL_DOWN, Duration::from_secs(60));
    assert!(text(&outcome).contains("455"), "{outcome:?}");

    let daily = send_to(Script::Smtp(AfterData::Reply(
        "455 4.7.1 Sending limit exceeded: 50000 messages Per Day",
    )))
    .await;
    assert!(
        matches!(&daily, Outcome::Deferred { cool_down, .. } if *cool_down == DAILY_COOL_DOWN),
        "{daily:?}"
    );
    assert_eq!(DAILY_COOL_DOWN, Duration::from_secs(3_600));
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

/// Nothing was submitted, so there is nothing to be ambiguous about, and it is the relay's trouble,
/// not the message's: deferred, to the next relay or later, spending no attempt.
#[tokio::test]
async fn a_refused_connection_is_deferred() {
    let addr = Sink::unused_addr().await;
    let outcome = Transports::plaintext_for_tests()
        .send(&relay(addr), None, &envelope(), BYTES)
        .await;
    assert!(
        matches!(&outcome, Outcome::Deferred { cool_down, .. } if *cool_down == DEFER_COOL_DOWN),
        "{outcome:?}"
    );
}

/// **Review Focus 1.** The sink accepts the connection and then says nothing, ever. With no
/// timeout of our own, lettre's async client waits forever: a worker tick blocks past its lease,
/// another worker re-claims the job row, and the message goes twice — the one outcome the spec
/// names as worse than not sending, arriving by way of a hang rather than a retry bug.
///
/// The session is injected at one second so the suite does not spend a real minute watching a
/// socket. Three things hold that honest and all three must stay: this test, which proves a
/// session is bounded at all; `a_transports_bounds_a_session_at_the_shipped_constant` in the
/// module's own tests, which proves the default is `SESSION_TIMEOUT` and not whatever a test
/// asked for; and a `const` assertion beside the constants, which proves `SESSION_TIMEOUT` is
/// under [`JOB_LEASE`] at compile time.
///
/// The assertion is a bound, not an equality: how long the send takes depends on the machine, and
/// the only quantity that carries meaning is whether it finished before the lease ran out.
#[tokio::test]
async fn a_relay_that_never_replies_does_not_outlive_the_lease() {
    let sink = Sink::start(Script::Silence).await;
    let transports = Transports::plaintext_for_tests().with_session_timeout(INJECTED_SESSION);
    let started = Instant::now();
    let outcome = tokio::time::timeout(
        JOB_LEASE,
        transports.send(&relay(sink.addr), None, &envelope(), BYTES),
    )
    .await
    .expect("send outlived the 120 s job lease: another worker would re-claim the row");
    let elapsed = started.elapsed();
    assert!(elapsed < JOB_LEASE, "{elapsed:?}");
    assert!(
        matches!(outcome, Outcome::Unknown(_)),
        "a hang could have been a hang after DATA, and so could have been accepted: {outcome:?}"
    );
}

/// **A stalled session must not take the next message down with it.** When the wrapper fires, the
/// connection is abandoned mid-conversation and lettre parks it back in its pool. The next
/// message is then handed a connection that owes an answer to the last one, and times out too:
/// one stalled send becomes a run of them, and every one of those is an `Unknown` that is never
/// retried.
///
/// The sink takes the first message and then owes its answer forever — the stall has to come
/// after `DATA`, because a connection that never finished being opened is dropped rather than
/// parked, and there is nothing stale to inherit. The second connection answers.
#[tokio::test]
async fn a_timed_out_session_is_not_left_in_the_pool() {
    let sink = Sink::start(Script::FirstThen(
        AfterData::Hang,
        AfterData::Reply("250 2.0.0 Ok: second"),
    ))
    .await;
    let transports = Transports::plaintext_for_tests().with_session_timeout(INJECTED_SESSION);
    let relay = relay(sink.addr);

    let first = transports.send(&relay, None, &envelope(), BYTES).await;
    assert!(matches!(first, Outcome::Unknown(_)), "{first:?}");

    // lettre parks an abandoned connection from a task of its own, so without this the next send
    // simply outruns it and the pool is empty for a reason that has nothing to do with the fix.
    // The stale connection would then be waiting for message three instead. This is a wait for
    // another task to run, not a measurement: a slow machine makes the test pass for the wrong
    // reason, never fail for one.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let second = transports.send(&relay, None, &envelope(), BYTES).await;
    assert!(
        matches!(&second, Outcome::Sent { response } if response.contains("second")),
        "the stalled connection was reused and took this message with it: {second:?}"
    );
}

/// **Invariant 5**, and the reason it is written down: `254` is a positive reply, so lettre hands
/// it over as an acceptance and nothing but the suppression check stands between it and a row
/// saying the message was delivered. The code is not matched on — the phrase is — so OCI
/// answering `250` or `254` makes no difference to what is recorded.
#[tokio::test]
async fn a_254_naming_a_suppression_is_failed() {
    let reply = "254 4.7.1 Suppression for user reader@dest.test";
    let outcome = send_to(Script::Smtp(AfterData::Reply(reply))).await;
    assert!(
        !matches!(outcome, Outcome::Sent { .. }),
        "a 254 suppression was recorded as a delivery: {outcome:?}"
    );
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert!(
        text(&outcome).contains("Suppression for user reader@dest.test"),
        "{outcome:?}"
    );
}

/// `plaintext_for_tests` is fenced to this machine, so the constructor cannot put a relay's
/// password on an open socket to somewhere else even if something called it.
#[tokio::test]
async fn a_plaintext_transport_refuses_a_host_that_is_not_loopback() {
    let mut relay = relay(Sink::unused_addr().await);
    relay.host = "smtp.example.test".to_owned();
    let outcome = Transports::plaintext_for_tests()
        .send(&relay, Some("hunter2"), &envelope(), BYTES)
        .await;
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert!(!text(&outcome).contains("hunter2"));
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
