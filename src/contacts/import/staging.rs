// SPDX-License-Identifier: AGPL-3.0-only

//! `COPY` into a per-import staging table: the one place each CSV row is touched individually.
//!
//! Everything after this module is set-based SQL, because the budget is a 1M-row file in under two
//! minutes. So each row is normalised, hashed, given an id and validated exactly once — here, on
//! its way into `COPY`. A row that fails any of that never reaches staging; it becomes an
//! `import_errors` row, and the rows after it carry on.
//!
//! **The other half of this module is [`upsert_from_staging`]**, which turns the whole staged file
//! into contacts, memberships and tags in four statements and touches no row on its own. The two
//! belong together: what
//! the `COPY` stages — a normalised address, its HMAC, and a null wherever the file said nothing —
//! is exactly what lets the upsert be a set operation.
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
    ColumnTarget, ContactField, Dedup, ImportJob,
    csv::{split_list_entry, split_multi, split_multi_raw, unescape_cell},
    routes::MAX_IMPORT_COLUMNS,
};
use crate::{
    consent,
    contacts::{validate_attributes, validate_name},
    crypto::{secrets::Secrets, suppression_hmac},
    custom_fields::{Definition, FieldKind},
    error::{ApiError, ApiResult},
    lists::{MembershipSource, validate_name as validate_tag_name},
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
    // Whether the header row has gone by, which is not the same as `row == 1`: a file may begin
    // with blank lines, and those are not rows.
    let mut header_done = !import.has_header;

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
        // The FIRST NON-BLANK record is the header, not line 1: a file that begins with a blank
        // line would otherwise have its real header parsed as data and every column named
        // `Column N`.
        //
        // Lossily, unlike every data cell below: a header here is only the label an error row
        // names a column by, so a replacement character in one costs a legible message and
        // nothing else. Refusing the header would refuse the whole file over its first line.
        //
        // A header the parser threw away is NOT one of those, and falls through to be an error row
        // like any other record: with no cells there is nothing to name the columns by, and a
        // silent `continue` here would return zero rows and zero errors for a file whose first
        // line carries an unterminated `"` — a success on a failure path.
        if !header_done {
            header_done = true;
            if oversize.is_none() {
                // NUL dropped for the same reason [`cut`] drops it: a header name is what an
                // error row's `column_name` is, and that is a `text` column too.
                headers = cells
                    .iter()
                    .map(|f| unescape_cell(&String::from_utf8_lossy(f).replace('\0', "")))
                    .collect();
                continue;
            }
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
    // What `COPY` says it took, against what was rendered for it. A mismatch is a rendering bug —
    // a quote or a newline escaped wrongly, so one line became two — and this is the one place it
    // is still cheap to see. Without it the next stage reads data that silently does not match the
    // file, and the counts it reports are wrong with nothing saying so.
    let accepted = copy.finish().await?;
    if accepted != counts.staged.unsigned_abs() {
        return Err(ApiError::Internal(anyhow::anyhow!(
            "COPY took {accepted} rows of the {} rendered for it",
            counts.staged
        )));
    }

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

/// What the upsert made of the staged rows.
///
/// **The five are mutually exclusive and sum to [`StagingCounts::total_rows`]**: every row of the
/// file lands in exactly one of them, which is what spec 7.5's breakdown means. `failed` carries
/// the `COPY`'s own refusals forward, so this one struct is the whole apportionment and the caller
/// has nothing left to add up.
///
/// `collapsed` is its own count and not part of `skipped`. A row the FILE de-duplicated against an
/// earlier row of its own is not a row the DATABASE already had, and folding one into the other
/// reports "these were already in your contacts" for rows that never were.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpsertCounts {
    /// Rows that became a new contact.
    pub imported: i64,
    /// Rows applied to a contact that already existed.
    pub updated: i64,
    /// Rows left alone because the contact already existed (`skip existing`).
    pub skipped: i64,
    /// Rows a LATER row of the same file names the same address as.
    pub collapsed: i64,
    /// The `COPY`'s refusals, plus every row refused for an erasure here.
    pub failed: i64,
    /// **Not one of the five.** Subscriptions this import asked for and did not get, because the
    /// address is already `unsubscribed` on that list (spec 7.5). The row itself is still an
    /// `imported` or an `updated` one — only the subscription was declined — so adding this to the
    /// five would count that row twice and break the sum above.
    pub memberships_declined: i64,
    /// **Not one of the five, and not a membership count.** Contacts this import created or
    /// updated whose address, or whose domain, is suppressed here or instance-wide. They are
    /// imported (suppression is about mailing, not existence) and they are not mailable, and
    /// spec 7.5 wants the number said out loud rather than discovered at send time.
    pub suppressed: i64,
}

/// What an `import_errors` row says about a row whose address has been erased on request.
///
/// Fixed text, and **both writers file it with a null `value`**. Nothing of the tombstone — not
/// its note, its source, its date, nor who asked — may reach a refusal, because the refusal is
/// read by whoever uploaded the file. Neither may the address itself: `0014_suppressions.sql`
/// nulls `suppressions.email` on an erasure so the address is gone, and writing the file's copy
/// of it into `import_errors.value` would put it back into a table
/// `GET /imports/{id}/errors` serves for ever. The row number says which line of the operator's
/// own file to look at, which is what they need and all they need.
const ERASED_REASON: &str =
    "address-erased: this address was erased at its owner's request and may not be re-created";

/// Turns the staged rows into contacts: one `insert … select` for the whole file.
///
/// **The caller must be in the transaction `copy_into_staging` ran in** — the staging table is
/// `on commit drop` and cannot be reached from anywhere else — and that transaction's `COMMIT` is
/// what makes this work durable.
///
/// **`copy_failed` is [`StagingCounts::failed`] and is the only number the caller supplies**: the
/// rows the `COPY` itself refused never reached staging, so this is the one term the statements
/// below cannot see for themselves. Everything else — how many rows staged included — is counted
/// here, in the same statement that computes the rest, so that a caller who passes a
/// `StagingCounts` from a DIFFERENT `COPY` cannot silently shift `collapsed`. The five counts
/// still come out of one place, already apportioned.
///
/// Four rules, in the order they apply to a row:
///
/// 1. **The file de-duplicates itself first**, last row wins (`distinct on (email) … order by
///    row_number desc`), and the rows it drops are `collapsed`. This is about the file alone, so
///    it happens before anything looks at the database.
/// 2. **An erased address is never re-created** (spec decision 1). The check is an anti-join
///    against the `gdpr_erasure` tombstones on the HMAC `copy_into_staging` already computed per
///    row — not `suppressions::is_erased`, whose per-address advisory lock is not an option a
///    million times over. Matched rows are `failed`, with [`ERASED_REASON`].
/// 3. **The dedup mode decides what an existing contact gets**: updated field by field, skipped,
///    or the whole import refused before it writes anything.
/// 4. **A blank cell leaves the stored value alone** (spec 7.3). See the `do update` below.
/// 5. **Then the contacts that survived all of that get their lists and their tags**, in
///    [`write_memberships_and_tags`], which is where spec 7.5's "may and may not" lives.
pub async fn upsert_from_staging(
    conn: &mut PgConnection,
    import: &ImportJob,
    copy_failed: i64,
) -> ApiResult<UpsertCounts> {
    let table = staging_table(import.id);
    let ws = import.workspace_id;

    // `fail on duplicate` is a pre-flight and not an `on conflict` arm: the operator asked for
    // their file back untouched, and a per-row importer would already have written every row
    // ahead of the duplicate before it found it. The address in the refusal is one the operator
    // supplied in their own file.
    if import.dedup == Dedup::FailOnDuplicate {
        let clash: Option<String> = sqlx::query_scalar(AssertSqlSafe(format!(
            "select s.email from {table} s \
               join contacts c on c.workspace_id = $1 and c.email = s.email \
              order by s.row_number limit 1"
        )))
        .bind(ws)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(email) = clash {
            return Err(ApiError::ConflictDetail {
                code: "import-duplicate-address",
                detail: format!(
                    "`{email}` is already a contact in this workspace, and this import was set \
                     to fail on a duplicate"
                ),
            });
        }
    }

    // What is left of this import's error budget. `copy_into_staging` has already spent some of
    // it, and the cap is per import, not per stage.
    // The macro, not the runtime query: it names no staging table, so there is nothing dynamic in
    // it, and `cargo sqlx prepare` then holds it to the schema the way every other fixed statement
    // in this tree is held (see `suppressions::is_erased`).
    let written = sqlx::query_scalar!(
        r#"select count(*) as "n!" from import_errors where workspace_id = $1 and import_id = $2"#,
        ws,
        import.id,
    )
    .fetch_one(&mut *conn)
    .await?;
    let budget = (MAX_IMPORT_ERRORS as i64 - written).max(0);

    let action = match import.dedup {
        // DECISION 4, IN SQL. A cell the file left blank, and a column it never mapped, both reach
        // `excluded` as the empty string or NULL, and `coalesce(nullif(…))` then leaves what is
        // stored alone — plain `excluded.col` would wipe a stored name with a blank cell. The same
        // rule reaches `attributes` by key instead of by value: `||` writes the keys this row gave
        // a value for and touches no other.
        Dedup::UpdateExisting => {
            "do update set \
               first_name = coalesce(nullif(excluded.first_name, ''), contacts.first_name), \
               last_name = coalesce(nullif(excluded.last_name, ''), contacts.last_name), \
               timezone = coalesce(nullif(excluded.timezone, ''), contacts.timezone), \
               attributes = contacts.attributes || excluded.attributes, \
               updated_at = now()"
        }
        Dedup::SkipExisting | Dedup::FailOnDuplicate => "do nothing",
    };

    // `AssertSqlSafe`: the only interpolation is `staging_table`'s fixed prefix with the import id
    // in hex, and `action`, which is one of the two literals above. Nothing a client sent reaches
    // this string.
    let (staged_rows, picked, erased, inserted, updated): (i64, i64, i64, i64, i64) =
        sqlx::query_as(AssertSqlSafe(format!(
            "with picked as ( \
                 select distinct on (email) * from {table} order by email, row_number desc \
             ), \
             live as ( \
                 select s.* from picked s \
                  where not exists (select 1 from suppressions where reason = 'gdpr_erasure' \
                                      and email_hmac = s.email_hmac \
                                      and (workspace_id = $1 or workspace_id is null)) \
             ), \
             erased as ( \
                 select s.row_number, s.email from picked s \
                  where not exists (select 1 from live l where l.row_number = s.row_number) \
             ), \
             reported as ( \
                 insert into import_errors \
                     (import_id, workspace_id, row_number, column_name, reason, value) \
                 select $2, $1, e.row_number, 'email', $4, null::text \
                   from erased e order by e.row_number limit $3 \
                 returning 1 \
             ), \
             up as ( \
                 insert into contacts \
                     (id, workspace_id, email, first_name, last_name, timezone, attributes) \
                 select l.id, $1, l.email, coalesce(l.first_name, ''), \
                        coalesce(l.last_name, ''), l.timezone, l.attributes \
                   from live l \
                 on conflict (workspace_id, email) {action} \
                 returning (xmax = 0) as inserted \
             ) \
             select (select count(*) from {table}), (select count(*) from picked), \
                    (select count(*) from erased), \
                    (select count(*) from up where inserted), \
                    (select count(*) from up where not inserted)"
        )))
        .bind(ws)
        .bind(import.id)
        .bind(budget)
        .bind(ERASED_REASON)
        .fetch_one(&mut *conn)
        .await?;

    let (declined, suppressed) = write_memberships_and_tags(conn, import).await?;

    // THE ANTI-JOIN AGAIN, OVER THE IDS THIS IMPORT INSERTED, as the last statement before the
    // caller's COMMIT. `c.id = s.id` matches only rows the insert above created: an updated
    // contact kept its own id and the staged one was discarded.
    //
    // RESIDUAL WINDOW: an erasure that commits between this statement's snapshot and the caller's
    // COMMIT is still missed. Its own transaction deleted the contacts it could see, and this
    // import's rows were not visible to it yet, so neither side sees the other and the contact
    // survives its erasure. The window is milliseconds and it is not closed here; see
    // `.scratch/email-platform/issues/20-an-erasure-can-race-an-imports-commit.md` for the two
    // ways to close it.
    let residual = (budget - erased.min(budget)).max(0);
    let gone: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "with gone as ( \
             delete from contacts c using {table} s \
              where c.workspace_id = $1 and c.id = s.id \
                and exists (select 1 from suppressions where reason = 'gdpr_erasure' \
                              and email_hmac = s.email_hmac \
                              and (workspace_id = $1 or workspace_id is null)) \
             returning s.row_number, c.email \
         ), \
         reported as ( \
             insert into import_errors \
                 (import_id, workspace_id, row_number, column_name, reason, value) \
             select $2, $1, g.row_number, 'email', $4, null::text \
               from gone g order by g.row_number limit $3 \
             returning 1 \
         ) \
         select count(*) from gone"
    )))
    .bind(ws)
    .bind(import.id)
    .bind(residual)
    .bind(ERASED_REASON)
    .fetch_one(&mut *conn)
    .await?;

    // `gone` can only name contacts this call's insert created: its ids come from the staging
    // table, and those are v7s minted row by row in `stage_row` that exist nowhere else. If that
    // ever stops holding, `imported` goes negative and the operator's report is nonsense — so say
    // so here rather than clamping, which would hide the break that caused it.
    debug_assert!(
        inserted >= gone,
        "the erasure sweep deleted {gone} contacts, but this import inserted {inserted}"
    );
    Ok(UpsertCounts {
        imported: inserted - gone,
        updated,
        // Only `skip existing` leaves any: in the other two modes every live row is inserted or
        // updated.
        skipped: picked - erased - inserted - updated,
        collapsed: staged_rows - picked,
        failed: copy_failed + erased + gone,
        memberships_declined: declined,
        suppressed,
    })
}

/// A UUIDv7 built in SQL, because Postgres 16 has no `uuidv7()` and a tag's id may not be random:
/// `GET /tags` is `order by id desc` and its documented contract is "Newest first", so a `v4` id
/// would scatter every imported tag through that list for ever. 48 bits of millisecond timestamp,
/// the version nibble `7`, the variant nibble `8`, and 72 bits of randomness around them — the
/// same layout `Uuid::now_v7()` writes, and `clock_timestamp()` rather than `now()` so that tags
/// made in one statement order by the millisecond they were made in. **Within one millisecond they
/// order by their random bits, which is to say arbitrarily** — the ordering this buys is against
/// everything created before and after the import, not among a handful of tags made together, and
/// nothing asserts the finer claim because it is not true.
///
/// A tag is the only row this module creates whose count is not known before the statement runs.
/// Minting these ids in Rust would mean reading every distinct tag name in the file into memory
/// first, which is the one thing this module refuses to do (see [`MAX_RECORD_BYTES`]).
const SQL_UUID_V7: &str = "(lpad(to_hex((extract(epoch from clock_timestamp()) * 1000)::bigint), \
                            12, '0') || '7' || substr(md5(random()::text), 1, 3) \
                            || '8' || substr(md5(random()::text), 1, 15))::uuid";

/// Puts the imported contacts on their lists and their tags on them: one statement, after the
/// contacts exist and before the erasure sweep (whose `delete` cascades through both tables, so a
/// contact taken back takes its memberships and tags with it). Returns the two counts that are
/// **not** among [`UpsertCounts`]'s five: `memberships_declined` and `suppressed`.
///
/// Spec 7.5 is the whole of it. An import **may** add tags and subscribe a contact to a list. It
/// **may not** resurrect an unsubscribed membership, and here that is not a special case but the
/// general rule: `on conflict … do nothing` — **an import never touches a membership that already
/// exists**. That is stronger than §7.5 asks for and it is stronger on purpose:
///
/// - An `unsubscribed` row stays unsubscribed, with its `unsubscribed_at` where the unsubscribe
///   put it. This is the M1a bulk-subscribe lesson: unsubscribing removes a row from the send, it
///   does not un-send.
/// - A `pending` row is not promoted (an import cannot confirm a double opt-in) and a `subscribed`
///   one is not demoted back to `pending`.
/// - `source` is left as the first writer set it. The consent log's answer to "how did this person
///   get on this list" is that first write, and an import that stamped `import` over a `ui` row
///   would destroy the answer while changing nothing else.
///
/// Note `list_memberships` and `contact_tags`, not `memberships` — which is which *users* belong
/// to a workspace, and would compile.
///
/// Two rules the spec left open, decided here:
///
/// 1. **A list the FILE names is matched by name against this workspace's lists, and an import
///    never creates one.** A tag only labels; a list receives mail, and has a `double_optin`
///    setting and a consent story that a misspelt cell may not invent. A name that matches nothing
///    writes nothing.
/// 2. **An entry whose status is `unsubscribed` is not a request to subscribe anyone**, so it
///    writes nothing and declines nothing. Any other status is a subscription request, and the
///    LIST decides what is written — `pending` on a double-opt-in list, `subscribed` otherwise —
///    exactly as `lists::add_member` does. An import may not hand itself a status the UI could not.
///
///    **What rule 2 costs, written down rather than discovered.** Dropping the entry carries no
///    record of the unsubscribe forward, so re-importing an export into a FRESH workspace leaves
///    the address with no membership at all — and a LATER import of that address, from a file
///    without the `:unsubscribed` marker, would then subscribe it, where a preserved
///    `unsubscribed` row would have been protected by the `do nothing` above. Nobody becomes
///    mailable by the import that drops the entry; what is lost is the protection on the next one.
async fn write_memberships_and_tags(
    conn: &mut PgConnection,
    import: &ImportJob,
) -> ApiResult<(i64, i64)> {
    let table = staging_table(import.id);
    let written = match import.dedup {
        // `skip existing` wrote nothing for a row whose contact already existed, and "nothing" has
        // to reach its tags and its lists too. The staged id is the tell: it is the contact's id
        // only where THIS import's insert created the contact.
        Dedup::SkipExisting => "c.id = l.id",
        Dedup::UpdateExisting | Dedup::FailOnDuplicate => "true",
    };
    // `AssertSqlSafe`: the only interpolations are `staging_table`'s fixed prefix with the import
    // id in hex, `written` (one of the two literals above) and [`SQL_UUID_V7`]. Nothing a client
    // sent reaches this string; the list the operator chose and the `import` provenance are bound.
    let (declined, suppressed, written_contacts, written_lists): (i64, i64, Vec<Uuid>, Vec<Uuid>) = sqlx::query_as(AssertSqlSafe(format!(
        // `picked` and `live` again, and for the same two reasons as the statement before this
        // one: the file de-duplicates itself before anything looks at the database, and an erased
        // address is touched by nothing — not even by a tag on the contact row it already had.
        "with picked as ( \
             select distinct on (email) * from {table} order by email, row_number desc \
         ), \
         live as ( \
             select s.* from picked s \
              where not exists (select 1 from suppressions where reason = 'gdpr_erasure' \
                                  and email_hmac = s.email_hmac \
                                  and (workspace_id = $1 or workspace_id is null)) \
         ), \
         target as ( \
             select l.tags, l.lists, c.id as contact_id, c.email, c.email_domain \
               from live l \
               join contacts c on c.workspace_id = $1 and c.email = l.email \
              where {written} \
         ), \
         named as ( \
             select t.contact_id, btrim(e->>'name') as name \
               from target t, lateral jsonb_array_elements(t.lists) e \
              where lower(btrim(coalesce(e->>'status', ''))) <> 'unsubscribed' \
         ), \
         wanted as ( \
             select t.contact_id, l.id as list_id, l.double_optin \
               from target t join lists l on l.workspace_id = $1 and l.id = $2 \
             union \
             select n.contact_id, l.id, l.double_optin \
               from named n join lists l on l.workspace_id = $1 and l.name = n.name \
         ), \
         declined as ( \
             select 1 from wanted w \
               join list_memberships m on m.workspace_id = $1 \
                    and m.contact_id = w.contact_id and m.list_id = w.list_id \
              where m.status = 'unsubscribed' \
         ), \
         added as ( \
             insert into list_memberships \
                 (workspace_id, contact_id, list_id, status, subscribed_at, source) \
             select $1, w.contact_id, w.list_id, \
                    case when w.double_optin then 'pending' else 'subscribed' end, \
                    case when w.double_optin then null else now() end, $3 \
               from wanted w \
             on conflict (contact_id, list_id) do nothing \
             returning contact_id, list_id \
         ), \
         row_tags as ( \
             select t.contact_id, btrim(v.value) as name \
               from target t, lateral jsonb_array_elements_text(t.tags) v \
              where btrim(v.value) <> '' \
         ), \
         made as ( \
             insert into tags (id, workspace_id, name) \
             select {SQL_UUID_V7}, $1, n.name from (select distinct name from row_tags) n \
             on conflict (workspace_id, name) do nothing \
             returning id, name \
         ), \
         known as ( \
             select g.id, g.name from tags g \
              where g.workspace_id = $1 and g.name in (select name from row_tags) \
             union all \
             select id, name from made \
         ), \
         tagged as ( \
             insert into contact_tags (workspace_id, contact_id, tag_id) \
             select distinct $1, r.contact_id, k.id \
               from row_tags r join known k on k.name = r.name \
             on conflict (contact_id, tag_id) do nothing \
             returning 1 \
         ), \
         suppressed as ( \
             select 1 from target t \
              where exists (select 1 from suppressions s where s.lifted_at is null \
                              and s.scope = 'address' and s.email = t.email \
                              and (s.workspace_id = $1 or s.workspace_id is null)) \
                 or exists (select 1 from suppressions s where s.lifted_at is null \
                              and s.scope = 'domain' and s.domain = t.email_domain \
                              and (s.workspace_id = $1 or s.workspace_id is null)) \
         ) \
         select (select count(*) from declined), (select count(*) from suppressed), \
                coalesce((select array_agg(contact_id order by contact_id, list_id) from added), '{{}}'), \
                coalesce((select array_agg(list_id order by contact_id, list_id) from added), '{{}}')"
    )))
    .bind(import.workspace_id)
    .bind(import.list_id)
    .bind(MembershipSource::Import)
    .fetch_one(&mut *conn)
    .await?;
    // One row per membership this statement wrote, in the same transaction. A membership it left
    // alone (`do nothing`) is no consent change and gets none. No IP or user agent: the importing
    // operator's are not the subscriber's, and none is invented.
    let cx = consent::Context {
        source_ref: Some(import.id.to_string()),
        ..consent::Context::default()
    };
    let (contacts, lists) = (&written_contacts, &written_lists);
    let source = consent::Source::Import;
    let action = consent::Action::Subscribed;
    consent::record_many(
        conn,
        import.workspace_id,
        (contacts, lists),
        action,
        source,
        &cx,
    )
    .await
    .map_err(consent::scrub)?;
    Ok((declined, suppressed))
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
///
/// **NUL characters are dropped, not kept and not refused.** `value` is a `text` column and
/// Postgres stores no NUL in one, so a cell holding one would make the whole error insert fail —
/// and that insert carries every refusal the file produced, so one bad character would cost the
/// operator the report on all of them AND turn the import into a 500. The offending cell is shown
/// for what else is wrong with it; a character that cannot be displayed is no loss in it.
fn cut(value: &str) -> Option<String> {
    Some(
        value
            .chars()
            .filter(|c| *c != '\0')
            .take(MAX_ERROR_VALUE_CHARS)
            .collect(),
    )
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
    let too_wide =
        cells.len() > job.mapping.len() && cells[job.mapping.len()..].iter().any(|f| !f.is_empty());
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
            // A CSV IS A TRUST BOUNDARY, AND THERE IS NO CONSTRAINT BEHIND THIS ONE:
            // `migrations/0013_tags.sql` declares a bare `name text`, and the only rule a tag name
            // has ever had is `lists::validate_name`, which `POST /tags` and `PUT /tags/{id}` both
            // go through. Staging a name that rule would refuse creates a row the API can read and
            // can never rewrite — `GET /tags` returns it and the UI renders it, and the first
            // rename fails validation on a value the operator never typed. A megabyte-long tag, or
            // one holding a newline, arrives this way and no other. So the import uses the API's
            // own validator: one rule for a tag name, whichever door it came through.
            //
            // An entry that is blank once trimmed is dropped and not refused. A `;;` is the file
            // saying nothing in that slot, the way a blank cell is (spec 7.3), and there is no tag
            // in it to create. Anything else that fails refuses the row, like every other cell
            // this function validates: one error row per failed row, naming the column.
            ColumnTarget::Field(ContactField::Tags) => {
                for tag in split_multi(&cell) {
                    if tag.trim().is_empty() {
                        continue;
                    }
                    tags.push(validate_tag_name(&tag).map_err(|e| bad(i, &e.message, &tag))?);
                }
            }
            // A LIST NAME IS MATCHED, NEVER CREATED, so it does not take `validate_tag_name` the
            // way the arm above does: a name over 100 characters simply matches no list and
            // writes nothing, and refusing the row for its length would be this importer
            // inventing a rule the lists API never applied to it.
            //
            // The CHARACTER rule it does take, and for a reason the other arms never had to
            // state. This cell is carried to the next stage as `jsonb`, so a control character is
            // not stored as itself but escaped — and `\u0000` is the one escape Postgres refuses
            // to convert to `text`. It refuses it for the whole `COPY`, not for the row: every
            // good row in the file goes with it, the request is a 500, and `import_errors` is
            // empty, so the operator is told nothing about a file with one bad character in it.
            // Caught here, it is one error row naming the column, like every other bad cell.
            //
            // The status too: it rides in the same object and reaches `jsonb` the same way.
            ColumnTarget::Field(ContactField::Lists) => {
                for entry in split_multi_raw(&cell) {
                    let (list, status) = split_list_entry(entry);
                    let status_chars = status.iter().flat_map(|s| s.chars());
                    if list.chars().chain(status_chars).any(char::is_control) {
                        return Err(bad(
                            i,
                            "a list name may not contain a control character",
                            entry,
                        ));
                    }
                    lists.push(json!({"name": list, "status": status}));
                }
            }
            // A blank cell is not a null: it leaves the stored value alone (spec 7.3), so it is
            // simply not in the object the next stage merges. `trim` BOTH SIDES of this arm, like
            // the name and timezone arms above: a cell of one space is a blank cell in a
            // spreadsheet, and a cell of `" gold "` is the operator naming the option `gold` —
            // guarding on the trimmed cell and then storing the untrimmed one stores `" pro "`
            // with its padding and turns a good `select` row into an error row.
            ColumnTarget::Custom(key) if !cell.trim().is_empty() => {
                let def = job
                    .defs
                    .iter()
                    .find(|d| d.key == *key)
                    .ok_or_else(|| bad(i, "no such custom field", &cell))?;
                attrs.insert(key.clone(), typed(def, cell.trim()));
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

    // `copy_into_staging` refuses a mapping with no email column up front, so this is unreachable
    // — but it is unreachable on a request path, where a panic is a 500 and a dropped connection.
    // A `RowError` costs the same and says the same thing.
    let email = email.ok_or_else(|| RowError {
        row,
        column: None,
        reason: "this row has no email address, and the mapping must name a column for one"
            .to_owned(),
        value: None,
    })?;
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
    // NO LAST-LINE NUL GUARD HERE, because there is no last line a guard could read. Postgres
    // stores no NUL in `text` or in `jsonb`, and one anywhere in this stream aborts the whole
    // `COPY`. But by this point a NUL in a `text` column has already been refused by that
    // column's validator — every arm above rejects control characters — and a NUL in one of the
    // three `jsonb` columns is no longer a NUL: `serde_json` wrote it as the six characters
    // `\u0000`, which `line.contains('\0')` cannot see and Postgres refuses anyway. A guard here
    // would be unreachable and would read as cover the arms above are the only ones giving.
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
        FieldKind::Number => {
            cell.parse::<i64>()
                .map(Value::from)
                .unwrap_or_else(|_| match cell.parse::<f64>() {
                    // `1e999` parses to an infinity, which is not a JSON number and not a quantity
                    // anyone meant to import.
                    Ok(f) if f.is_finite() => Value::from(f),
                    _ => Value::from(cell),
                })
        }
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
    /// at [`MAX_IMPORT_COLUMNS`] and `out` at [`MAX_RECORD_BYTES`], so neither the file's size nor
    /// a stray quote in it decides how much memory one import holds.
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
                // the offsets rewound, so nothing accumulates for it. `out` may still double
                // under it — a refused-for-its-fields record whose remainder does not fit still
                // takes the `OutputFull` arm — but only up to `MAX_RECORD_BYTES`, which is what
                // bounds this loop's memory either way. What is in the buffers is then nonsense —
                // `read_record` writes each end as a position in the whole record, so after a
                // rewind they no longer index `out` at all — which is why a refused record is
                // handed back with no fields rather than with these.
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
                    // `>=`, and the cap is `MAX_IMPORT_COLUMNS` and not one more: a record of
                    // exactly `MAX_IMPORT_COLUMNS` fields is inside the cap and needs exactly that
                    // many offsets, so once `ends` holds them a record still asking for room has
                    // more fields than the cap allows.
                    if ends.len() >= MAX_IMPORT_COLUMNS {
                        oversize = oversize.or(Some(Oversize::Fields));
                    } else {
                        ends.resize((ends.len() * 2).min(MAX_IMPORT_COLUMNS), 0);
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
