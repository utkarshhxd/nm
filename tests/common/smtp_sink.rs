// SPDX-License-Identifier: AGPL-3.0-only

//! An in-process SMTP server that can be driven to any outcome the transport classifies.
//!
//! It speaks the little of the protocol a submission needs — greeting, `EHLO`, `MAIL`, `RCPT`,
//! `DATA`, the terminating `.` — and is told up front what to answer and when to stop answering.
//! Plain TCP on `127.0.0.1`: a test cannot present a certificate a TLS client would accept, so
//! the transport is built with [`Transports::plaintext_for_tests`].
//!
//! [`Transports::plaintext_for_tests`]: nm::relay::transport::Transports::plaintext_for_tests

use std::net::SocketAddr;

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, tcp::OwnedReadHalf},
    task::{JoinHandle, JoinSet},
};

/// What the sink does once it has read the whole message body.
#[derive(Clone, Copy)]
pub enum AfterData {
    /// Answer with this line, with no trailing CRLF: `"250 2.0.0 Ok"`, `"451 4.3.0 later"`, …
    Reply(&'static str),
    /// Close the connection without answering. The message was read, and the sender cannot know
    /// whether it was queued.
    Drop,
    /// Hold the connection open and never answer. Same ambiguity as `Drop`, except that the
    /// sender is left waiting rather than told anything, and the session is already established —
    /// which is the state a pooled connection can be abandoned in.
    Hang,
}

/// How a connection to the sink goes.
#[derive(Clone, Copy)]
pub enum Script {
    /// Speak SMTP, and do this at the end of the body.
    Smtp(AfterData),
    /// Accept the connection and say nothing at all, forever — not even the greeting.
    Silence,
    /// Do the first thing on the first connection and the second on every one after it: a relay
    /// that stalls one message and then recovers, which is what tells a stale pooled connection
    /// from a stale relay.
    FirstThen(AfterData, AfterData),
}

pub struct Sink {
    pub addr: SocketAddr,
    accepting: JoinHandle<()>,
}

impl Drop for Sink {
    fn drop(&mut self) {
        // The accept task owns the `JoinSet`, so aborting it aborts the connections with it.
        self.accepting.abort();
    }
}

impl Sink {
    /// Starts a sink on a port the kernel chooses, and returns once it is listening.
    pub async fn start(script: Script) -> Sink {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the sink");
        let addr = listener.local_addr().expect("the sink's address");
        let accepting = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            let mut accepted = 0u32;
            while let Ok((stream, _)) = listener.accept().await {
                accepted += 1;
                let speaking = match script {
                    Script::Silence => None,
                    Script::Smtp(after) => Some(after),
                    Script::FirstThen(first, rest) => {
                        Some(if accepted == 1 { first } else { rest })
                    }
                };
                match speaking {
                    None => connections.spawn(async move {
                        // Held, not closed: a closed socket is a different outcome entirely.
                        let _silent = stream;
                        std::future::pending::<()>().await;
                    }),
                    Some(after) => connections.spawn(async move {
                        let _ = serve(stream, after).await;
                    }),
                };
            }
        });
        Sink { addr, accepting }
    }

    /// An address with nothing listening on it: the kernel chose the port and we handed it back.
    pub async fn unused_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind to find a free port");
        listener.local_addr().expect("the free port")
    }
}

/// Answers one connection. An I/O error is the far end going away, which ends the connection and
/// nothing else.
async fn serve(stream: TcpStream, after: AfterData) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    writer.write_all(b"220 sink.test ESMTP\r\n").await?;

    loop {
        let Some(line) = read_line(&mut reader).await? else {
            return Ok(());
        };
        let command = String::from_utf8_lossy(&line)
            .trim_end()
            .to_ascii_uppercase();

        let reply: &[u8] = if command.starts_with("EHLO") || command.starts_with("HELO") {
            // No extensions are advertised: nothing this product submits needs one, and a sink
            // that offered AUTH would have to be a credential store as well.
            b"250 sink.test\r\n"
        } else if command.starts_with("MAIL") {
            b"250 2.1.0 sender ok\r\n"
        } else if command.starts_with("RCPT") {
            b"250 2.1.5 recipient ok\r\n"
        } else if command.starts_with("NOOP") || command.starts_with("RSET") {
            b"250 2.0.0 ok\r\n"
        } else if command.starts_with("QUIT") {
            writer.write_all(b"221 2.0.0 bye\r\n").await?;
            return Ok(());
        } else if command.starts_with("DATA") {
            writer.write_all(b"354 end with <CRLF>.<CRLF>\r\n").await?;
            loop {
                match read_line(&mut reader).await? {
                    // The sender went away mid-body; there is nothing left to answer.
                    None => return Ok(()),
                    Some(line) if line.as_slice() == b".\r\n" => break,
                    Some(_) => {}
                }
            }
            match after {
                AfterData::Reply(reply) => {
                    writer.write_all(reply.as_bytes()).await?;
                    writer.write_all(b"\r\n").await?;
                    continue;
                }
                AfterData::Drop => return Ok(()),
                // Open, read, and owing an answer forever.
                AfterData::Hang => std::future::pending().await,
            }
        } else {
            b"500 5.5.1 unknown command\r\n"
        };
        writer.write_all(reply).await?;
    }
}

/// The next line, terminator and all, or `None` at the end of the stream. Bytes, not text: the
/// body is whatever the sender wrote, and the sink does not get to reject it for not being UTF-8.
async fn read_line(reader: &mut BufReader<OwnedReadHalf>) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).await? == 0 {
        Ok(None)
    } else {
        Ok(Some(line))
    }
}
