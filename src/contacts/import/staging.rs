// SPDX-License-Identifier: AGPL-3.0-only

//! `COPY` into a per-import staging table: the one place each CSV row is touched individually.
//!
//! Everything after this module is set-based SQL, because the budget is a 1M-row file in under two
//! minutes. So each row is normalised, hashed, given an id and validated exactly once — here, on
//! its way into `COPY`. A row that fails any of that never reaches staging; it becomes an
//! `import_errors` row, and the rows after it carry on.
//!
//! **The file is parsed incrementally with `csv_core`**, which is the `csv` crate's own parser
//! without the blocking `io::Read` around it. That is what lets an `AsyncRead` feed `COPY`
//! without either holding the whole file in memory or bridging to a blocking thread (the
//! connection is borrowed by the `COPY` for the whole stream, so nothing else may touch it).
//!
//! **What "without holding the whole file" costs is a cap on one record**, because a CSV record is
//! only over when the file says so: an unterminated `"` makes the rest of the upload one record,
//! and a file of nothing but delimiters one row of millions of fields. So the parser's two buffers
//! are bounded by [`MAX_RECORD_BYTES`] and [`MAX_IMPORT_COLUMNS`], and a record past either is
//! consumed and refused at constant memory while the rows after it are still read. An import's
//! memory is therefore a row's worth, whatever is in the file.
//!
//! Three decisions this module implements rather than revisits:
//!
//! 1. **Ids are `Uuid::now_v7()`, minted in Rust.** Never `gen_random_uuid()`: a selection's
//!    snapshot is `id <= as_of` (see `contacts::selection`), so an id that is not time-ordered
//!    escapes that bound entirely and every imported contact silently falls out of it.
//! 2. **The address is `users::normalize_email`**, the function the rest of the system uses. An
//!    import that normalises differently creates a contact that can never be matched again.
//! 3. **`crypto::suppression_hmac` is computed per row into a staging column**, so the erasure
//!    rule can be an anti-join in the next stage. `suppressions::is_erased` takes a per-address
//!    advisory lock, which is not an option a million times over.

use std::fmt::Write as _;

use serde_json::{Map, Value, json};
use sqlx::{AssertSqlSafe, PgConnection, Postgres, QueryBuilder};
use tokio::io::{AsyncRead, AsyncReadExt};
use uuid::Uuid;

use super::{
    ColumnTarget, ContactField, ImportJob,
    csv::{split_list_entry, split_multi, split_multi_raw, unescape_cell},
    routes::MAX_IMPORT_COLUMNS,
};
use crate::{
    contacts::{validate_attributes, validate_name},
    crypto::{secrets::Secrets, suppression_hmac},
    custom_fields::{Definition, FieldKind},
    error::{ApiError, ApiResult},
    users::normalize_email,
    workspaces::validate_timezone,
};

/// Rendered rows buffered before one `send` down the `COPY` stream.
const COPY_CHUNK: usize = 64 * 1024;

/// Bytes read from the file per `read`.
const READ_CHUNK: usize = 64 * 1024;

/// The most `import_errors` rows one import writes. A 1M-row file of garbage is a million
/// failures, and both the memory they are held in and the table they land in are inside this
/// import's own budget; past the first few thousand the operator has learnt what is wrong with
/// their file. `StagingCounts::failed` still counts every failed row, so the report is never wrong
/// about how many there were — only about how many it can show.
pub const MAX_IMPORT_ERRORS: usize = 10_000;

/// An offending value is stored cut to this many characters. One cell can be a megabyte, and
/// `import_errors.value` exists to show the operator what failed, not to keep a copy of the file.
const MAX_ERROR_VALUE_CHARS: usize = 200;

/// The most field data one record may hold before it is refused unread.
///
/// It bounds the parser's output buffer, which is the only thing in this module that grows with
/// the file rather than with a row. Without it one unterminated `"` makes the rest of the upload a
/// single record, and `MAX_IMPORT_BYTES` (512 MiB) becomes the bound: the final doubling would
/// hold the old 512 MiB and the new 1 GiB at once, per import running. A megabyte is some five
/// hundred times a fat contact row with a dozen attributes, so nothing an operator meant to import
/// reaches it — and a record that does is refused **by its size**, in those words, because telling
/// someone their row is too wide when their cell is too long sends them looking in the wrong
/// place.
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;

/// The staging columns, in the order every statement here writes them.
const COLUMNS: &str =
    "row_number, id, email, email_hmac, first_name, last_name, timezone, attributes, tags, lists";

/// What the file amounted to. The five row-level counts on `imports` are the next stage's to
/// apportion; these are what the `COPY` itself saw, and `total_rows = staged + failed` always.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StagingCounts {
    /// Data rows read. The header row is not one, and neither is a blank line.
    pub total_rows: i64,
    pub staged: i64,
    /// Rows that were refused. At most [`MAX_IMPORT_ERRORS`] of them were written out.
    pub failed: i64,
}

/// The staging table's name. `simple` because a hyphen is not an identifier character; 32 hex
/// digits and a fixed prefix are 47 bytes, inside Postgres's 63, and nothing an id can spell is
/// injectable.
pub fn staging_table(import_id: Uuid) -> String {
    format!("import_staging_{}", import_id.simple())
}

/// A record the parser threw away as it read it, rather than hand over: it was past one of the
/// two caps that keep a row's memory a row's worth. Such a record comes back with no fields at
/// all, so nothing downstream reads the buffers it was rewound out of. Each is its own
/// `import_errors` reason: too long and too wide send the operator to different places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Oversize {
    /// More than [`MAX_IMPORT_COLUMNS`] fields.
    Fields,
    /// More than [`MAX_RECORD_BYTES`] of field data.
    Bytes,
}

/// One rejected row, held until the `COPY` gives the connection back.
struct RowError {
    row: i64,
    column: Option<String>,
    reason: String,
    value: Option<String>,
}

/// Streams `file` into `import_staging_<id>` with one `COPY`, and writes an `import_errors` row
/// for each row it refused.
///
/// **The caller must be inside a transaction**: the table is `on commit drop`, so a dropped
/// connection cannot leave it behind, and outside a transaction it would go before the caller
/// could read it. The error rows are written on the same connection and so commit with that
/// transaction; a caller that wants the per-row report to survive an aborted upsert has to commit
/// them separately.
pub async fn copy_into_staging(
    conn: &mut PgConnection,
    secrets: &Secrets,
    import: &ImportJob,
    file: impl AsyncRead + Unpin,
) -> ApiResult<StagingCounts> {
    // Without it every row fails for one reason, which is a mapping mistake and not a million
    // row-level ones.
    if !import
        .mapping
        .contains(&ColumnTarget::Field(ContactField::Email))
    {
        return Err(ApiError::UnprocessableDetail {
            code: "mapping-needs-email",
            detail: "one column must be mapped to `email`".to_owned(),
        });
    }
    let table = staging_table(import.id);
    // A null first/last/timezone means the file said nothing (the column is unmapped, or the cell
    // was blank), which is spec 7.3's "a blank cell leaves the stored value alone": the next stage
    // coalesces rather than clearing. `tags` and `lists` are jsonb and not `text[]` so that one
    // escaping rule reaches the database instead of two nested ones (a CSV field holding an array
    // literal holding a quoted name).
    // `AssertSqlSafe`: `table` is `staging_table`'s fixed prefix and the import id in hex, and the
    // rest is a literal. Nothing a client sent reaches this string.
    sqlx::query(AssertSqlSafe(format!(
        "create temporary table {table} (
           row_number bigint not null, id uuid not null,
           email text not null, email_hmac bytea not null,
           first_name text, last_name text, timezone text,
           attributes jsonb not null, tags jsonb not null, lists jsonb not null
         ) on commit drop"
    )))
    .execute(&mut *conn)
    .await?;

    let mut csv = Csv::new(file, import.delimiter);
    let (mut out, mut ends) = (vec![0u8; 8 * 1024], vec![0usize; 64]);
    let (mut headers, mut errors) = (Vec::<String>::new(), Vec::<RowError>::new());
    let mut counts = StagingCounts::default();
    let mut buf = String::with_capacity(COPY_CHUNK + 4096);
    let mut row = 0i64;

    let mut copy = conn
        .copy_in_raw(&format!("copy {table} ({COLUMNS}) from stdin (format csv)"))
        .await?;
    // `let … else` and not `while let`: a `while let` scrutinee's temporaries live for the whole
    // body, and the `&mut out` among them would collide with reading the record out of `out`.
    loop {
        let Some((nend, oversize)) = csv
            .next(&mut out, &mut ends)
            .await
            .map_err(|e| ApiError::Internal(e.into()))?
        else {
            break;
        };
        row += 1;
        let cells = fields(&out, &ends[..nend]);
        // A blank line is not a row, the way the `csv` crate also reads it. It still advances the
        // row number, so an error row points at the line the operator's spreadsheet shows.
        if oversize.is_none() && cells.len() <= 1 && cells.first().is_none_or(|f| f.is_empty()) {
            continue;
        }
        // Lossily, unlike every data cell below: a header here is only the label an error row
        // names a column by, so a replacement character in one costs a legible message and
        // nothing else. Refusing the header would refuse the whole file over its first line.
        if row == 1 && import.has_header {
            headers = cells
                .iter()
                .map(|f| unescape_cell(&String::from_utf8_lossy(f)))
                .collect();
            continue;
        }
        counts.total_rows += 1;
        match stage_row(secrets, import, &headers, row, &cells, oversize) {
            Ok(line) => {
                counts.staged += 1;
                buf.push_str(&line);
                if buf.len() >= COPY_CHUNK {
                    copy.send(std::mem::take(&mut buf).into_bytes()).await?;
                }
            }
            Err(e) => {
                counts.failed += 1;
                if errors.len() < MAX_IMPORT_ERRORS {
                    errors.push(e);
                }
            }
        }
    }
    if !buf.is_empty() {
        copy.send(buf.into_bytes()).await?;
    }
    copy.finish().await?;

    // Chunked because Postgres takes at most 65535 bind parameters in one statement, and six
    // columns times the error cap is past that.
    for chunk in errors.chunks(1000) {
        let mut q = QueryBuilder::<Postgres>::new(
            "insert into import_errors \
             (import_id, workspace_id, row_number, column_name, reason, value) ",
        );
        q.push_values(chunk, |mut b, e| {
            b.push_bind(import.id)
                .push_bind(import.workspace_id)
                .push_bind(e.row)
                .push_bind(e.column.clone())
                .push_bind(e.reason.clone())
                .push_bind(e.value.clone());
        });
        q.build().execute(&mut *conn).await?;
    }
    Ok(counts)
}

/// The fields of one record, as `csv_core` leaves them: one buffer and the end offset of each.
fn fields<'a>(out: &'a [u8], ends: &[usize]) -> Vec<&'a [u8]> {
    let mut start = 0;
    ends.iter()
        .map(|&end| {
            let field = &out[start..end];
            start = end;
            field
        })
        .collect()
}

/// `value` as `import_errors` stores it.
fn cut(value: &str) -> Option<String> {
    Some(value.chars().take(MAX_ERROR_VALUE_CHARS).collect())
}

/// One CSV row rendered for `COPY`, or the first thing wrong with it.
///
/// One error row per failed row and not one per failed cell: `failed` and `import_errors` then
/// agree on what a row is, and the operator fixes the first fault and runs it again anyway.
fn stage_row(
    secrets: &Secrets,
    job: &ImportJob,
    headers: &[String],
    row: i64,
    cells: &[&[u8]],
    oversize: Option<Oversize>,
) -> Result<String, RowError> {
    let name = |i: usize| {
        headers
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("Column {}", i + 1))
    };
    let bad = |i: usize, reason: &str, value: &str| RowError {
        row,
        column: Some(name(i)),
        reason: reason.to_owned(),
        value: cut(value),
    };

    // A ROW WITH MORE FIELDS THAN THE HEADER IS AN ERROR ROW, NOT A TRUNCATION. The reader is
    // flexible on purpose (Task 3 caps and counts the header only), and an extra field means the
    // delimiter turned up where this file's own shape did not expect it — an unquoted separator
    // inside a value. Every mapped cell from there on may therefore be the cell to its left, and
    // truncating writes that plausible-looking wrong data into the right-looking columns with
    // nothing saying so. Extra fields that are all empty are a trailing delimiter, which is
    // common, harmless, and dropped in silence.
    //
    // A record the parser threw away unread is refused here too, in its own words: a row over
    // `MAX_RECORD_BYTES` is told it is too long, not too wide, because the two faults send the
    // operator to different places in their file.
    let too_wide = cells.len() > job.mapping.len()
        && cells[job.mapping.len()..].iter().any(|f| !f.is_empty());
    if oversize.is_some() || too_wide {
        let reason = match oversize {
            Some(Oversize::Bytes) => format!(
                "the row is longer than the {MAX_RECORD_BYTES} bytes one row may hold; \
                 an unclosed quote earlier in the file can do this"
            ),
            Some(Oversize::Fields) => format!(
                "the row has more than {MAX_IMPORT_COLUMNS} fields and the file has {} columns",
                job.mapping.len()
            ),
            None => format!(
                "the row has {} fields and the file has {} columns",
                cells.len(),
                job.mapping.len()
            ),
        };
        return Err(RowError {
            row,
            column: None,
            reason,
            value: None,
        });
    }

    let (mut email, mut first, mut last, mut tz) = (None, None, None, None);
    let (mut attrs, mut tags, mut lists) = (Map::new(), Vec::new(), Vec::new());
    for (i, target) in job.mapping.iter().enumerate() {
        if *target == ColumnTarget::Ignore {
            continue;
        }
        let raw = cells.get(i).copied().unwrap_or(b"");
        // Task 3's sniff decodes lossily so the operator is shown something; staging may not,
        // because a replacement character is a different address and a different name. A file
        // exported as Latin-1 is told so, with the line and the column, and is never a 500.
        let Ok(text) = std::str::from_utf8(raw) else {
            return Err(bad(
                i,
                "this cell is not valid UTF-8; re-export the file as UTF-8",
                &String::from_utf8_lossy(raw),
            ));
        };
        let cell = unescape_cell(text);
        match target {
            ColumnTarget::Ignore => unreachable!("skipped above"),
            ColumnTarget::Field(ContactField::Email) => {
                email = Some(normalize_email(&cell).map_err(|e| bad(i, &e.message, &cell))?);
            }
            ColumnTarget::Field(f @ (ContactField::FirstName | ContactField::LastName)) => {
                if !cell.trim().is_empty() {
                    let value =
                        validate_name(f.header(), &cell).map_err(|e| bad(i, &e.message, &cell))?;
                    if *f == ContactField::FirstName {
                        first = Some(value);
                    } else {
                        last = Some(value);
                    }
                }
            }
            ColumnTarget::Field(ContactField::Timezone) => {
                if !cell.trim().is_empty() {
                    tz = Some(
                        validate_timezone(cell.trim())
                            .map_err(|e| bad(i, &e.message, &cell))?
                            .to_owned(),
                    );
                }
            }
            ColumnTarget::Field(ContactField::Tags) => tags.extend(split_multi(&cell)),
            ColumnTarget::Field(ContactField::Lists) => {
                for entry in split_multi_raw(&cell) {
                    let (list, status) = split_list_entry(entry);
                    lists.push(json!({"name": list, "status": status}));
                }
            }
            // A blank cell is not a null: it leaves the stored value alone (spec 7.3), so it is
            // simply not in the object the next stage merges.
            ColumnTarget::Custom(key) if !cell.is_empty() => {
                let def = job
                    .defs
                    .iter()
                    .find(|d| d.key == *key)
                    .ok_or_else(|| bad(i, "no such custom field", &cell))?;
                attrs.insert(key.clone(), typed(def, &cell));
            }
            ColumnTarget::Custom(_) => {}
        }
    }
    let attrs = validate_attributes(&job.defs, &Value::Object(attrs)).map_err(|errs| {
        let e = &errs[0];
        let key = e.field.strip_prefix("attributes.").unwrap_or(&e.field);
        let at = job
            .mapping
            .iter()
            .position(|t| *t == ColumnTarget::Custom(key.to_owned()));
        match at {
            Some(i) => bad(
                i,
                &e.message,
                &String::from_utf8_lossy(cells.get(i).copied().unwrap_or(b"")),
            ),
            None => RowError {
                row,
                column: Some(key.to_owned()),
                reason: e.message.clone(),
                value: None,
            },
        }
    })?;

    let email =
        email.expect("the mapping names an email column, and a blank one fails to normalise");
    let mut hmac = String::with_capacity(66);
    hmac.push_str("\\x");
    for b in suppression_hmac(secrets, &email) {
        let _ = write!(hmac, "{b:02x}");
    }
    let mut line = String::with_capacity(256);
    let _ = write!(line, "{row}");
    for field in [
        Some(Uuid::now_v7().to_string()),
        Some(email),
        Some(hmac),
        first,
        last,
        tz,
        Some(attrs.to_string()),
        Some(Value::from(tags).to_string()),
        Some(Value::from(lists).to_string()),
    ] {
        line.push(',');
        push_field(&mut line, field.as_deref());
    }
    // Postgres stores no NUL in `text` or `jsonb`, and one in the stream would abort the whole
    // `COPY` — every good row in the file with it. The validators reject a control character in
    // every field that has one; a tag or a list name has none, so this is where they are caught.
    if line.contains('\0') {
        return Err(RowError {
            row,
            column: None,
            reason: "a cell contains a NUL byte, which cannot be stored".to_owned(),
            value: None,
        });
    }
    line.push('\n');
    Ok(line)
}

/// A field as `COPY ... (format csv)` reads it. An unquoted empty field is NULL and a quoted one
/// is the empty string, which is the whole reason `None` writes nothing at all.
fn push_field(out: &mut String, value: Option<&str>) {
    let Some(value) = value else { return };
    out.push('"');
    for c in value.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
}

/// The cell in the shape its field's kind takes. Anything that does not fit stays a string, which
/// `validate_attributes` then refuses in its own words: this decides the shape, that one function
/// decides what is valid, and there is no second opinion about `-5-5` or `1e999`.
fn typed(d: &Definition, cell: &str) -> Value {
    match d.kind {
        FieldKind::Number => cell
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| match cell.parse::<f64>() {
                // `1e999` parses to an infinity, which is not a JSON number and not a quantity
                // anyone meant to import.
                Ok(f) if f.is_finite() => Value::from(f),
                _ => Value::from(cell),
            }),
        FieldKind::Boolean => match cell.to_ascii_lowercase().as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::from(cell),
        },
        // What `export::attr_text` writes: `;`-joined with `\`, `;` and `:` escaped.
        FieldKind::MultiSelect => Value::from(split_multi(cell)),
        _ => Value::from(cell),
    }
}

/// `csv_core` over an `AsyncRead`: the parser is incremental, so refilling the input buffer is all
/// that stands between an async file and a record.
struct Csv<R> {
    reader: csv_core::Reader,
    source: R,
    input: Vec<u8>,
    start: usize,
    end: usize,
    eof: bool,
}

impl<R: AsyncRead + Unpin> Csv<R> {
    fn new(source: R, delimiter: u8) -> Csv<R> {
        Csv {
            // The default terminator is CRLF-aware: `\r\n` ends one record, so the `\n` after a
            // `\r` does not start an empty one. A BOM is stripped by the parser itself.
            reader: csv_core::ReaderBuilder::new().delimiter(delimiter).build(),
            source,
            input: vec![0; READ_CHUNK],
            start: 0,
            end: 0,
            eof: false,
        }
    }

    /// The next record, as the number of fields written into `ends` and, when the parser threw it
    /// away instead, why. `None` at the end of the file.
    ///
    /// **Both buffers are capped, and a record past either cap is consumed at constant memory**:
    /// the offsets rewind, the parser keeps running to the record's end so the rows after it are
    /// still read, and the caller gets the reason to put in an `import_errors` row. `ends` stops
    /// at [`MAX_IMPORT_COLUMNS`] + 1 and `out` at [`MAX_RECORD_BYTES`], so neither the file's size
    /// nor a stray quote in it decides how much memory one import holds.
    async fn next(
        &mut self,
        out: &mut Vec<u8>,
        ends: &mut Vec<usize>,
    ) -> std::io::Result<Option<(usize, Option<Oversize>)>> {
        let (mut nout, mut nend) = (0, 0);
        let mut oversize: Option<Oversize> = None;
        loop {
            if oversize.is_some() {
                // The record is already refused; keep the parser moving over the rest of it with
                // the buffers rewound, so neither one grows for it. What is in them is then
                // nonsense — `read_record` writes each end as a position in the whole record, so
                // after a rewind they no longer index `out` at all — which is why a refused
                // record is handed back with no fields rather than with these.
                nout = 0;
                nend = 0;
            }
            if self.start == self.end && !self.eof {
                self.end = self.source.read(&mut self.input).await?;
                self.start = 0;
                self.eof = self.end == 0;
            }
            let (result, nin, wrote, ended) = self.reader.read_record(
                &self.input[self.start..self.end],
                &mut out[nout..],
                &mut ends[nend..],
            );
            self.start += nin;
            nout += wrote;
            nend += ended;
            match result {
                csv_core::ReadRecordResult::InputEmpty => {}
                csv_core::ReadRecordResult::OutputFull => {
                    if out.len() >= MAX_RECORD_BYTES {
                        oversize = oversize.or(Some(Oversize::Bytes));
                    } else {
                        out.resize((out.len() * 2).min(MAX_RECORD_BYTES), 0);
                    }
                }
                csv_core::ReadRecordResult::OutputEndsFull => {
                    if ends.len() > MAX_IMPORT_COLUMNS {
                        oversize = oversize.or(Some(Oversize::Fields));
                    } else {
                        ends.resize((ends.len() * 2).min(MAX_IMPORT_COLUMNS + 1), 0);
                    }
                }
                csv_core::ReadRecordResult::Record => {
                    return Ok(Some((if oversize.is_some() { 0 } else { nend }, oversize)));
                }
                csv_core::ReadRecordResult::End => return Ok(None),
            }
        }
    }
}
