// SPDX-License-Identifier: AGPL-3.0-only

//! `POST /imports`: the upload, and the server's reading of its head. Nothing there writes to
//! `contacts`; the run is a later request.
//!
//! That request is `POST /imports/{id}/start`, which is where the whole pipeline is finally wired
//! together and is **the only thing in `src/` that writes an import's counts**. Everything before
//! it computes numbers; this is what puts them on the row the report reads.
//!
//! And the reads an operator is left with: `GET /imports`, the workspace's imports newest first,
//! `GET /imports/{id}`, the report, and `GET /imports/{id}/errors`, the rows it rejected.

use std::path::{Path, PathBuf};

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Multipart,
        multipart::{MultipartError, MultipartRejection},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Acquire, PgConnection, PgPool, Postgres, Transaction};
use tokio::io::AsyncWriteExt;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{
    ColumnTarget, ContactField, Dedup, ImportJob,
    sniff::{DELIMITERS, sniff},
    staging::{copy_into_staging, upsert_from_staging},
    upload_path,
};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    custom_fields::{Definition, definitions_for},
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    lists,
    pagination::{Page, PageParams},
    workspaces::roles::Role,
};

use axum::extract::State;

/// The largest upload accepted, in bytes. The cap on rows is Task 8's and is 1M; at the few hundred
/// bytes a contact row takes with a dozen attributes that is well under this. A named limit and not
/// configuration, as `MAX_RULES` and `CHUNK_BYTES` are: it is a bound on what one request may make
/// the disk hold, and an operator who raises it should have to read this comment.
pub const MAX_IMPORT_BYTES: u64 = 512 * 1024 * 1024;

/// The most rows an import may carry. `spec.md` budgets **a 1M-row CSV at under two minutes**,
/// which is what the staging `COPY` and the set-based upsert were shaped against; a file past it
/// is one an operator watches until the request times out, and a run killed part-way is the worst
/// of both. So it is refused at the upload, naming the cap. A named limit and not configuration,
/// as `MAX_IMPORT_BYTES`, `MAX_RULES` and `CHUNK_BYTES` are.
///
/// Counted on LINE FEEDS as the file streams, which is not a count of records: a quoted cell
/// holding a line feed counts twice, and one line is discounted for a header the sniff has not
/// read yet, so a headed file at the cap is not refused for its header. Counting records exactly
/// means parsing the file, which is the work this cap exists to refuse to start.
pub const MAX_IMPORT_ROWS: u64 = 1_000_000;

/// The most columns a file may have. The run copies each file into a staging table with one column
/// per CSV column, and Postgres refuses a table over 1600 columns (and one near it fails with an
/// error nobody can read), so the cap sits well below that, leaving room for bookkeeping columns.
/// Checked at upload so the operator hears it before the mapping screen renders a thousand rows.
pub const MAX_IMPORT_COLUMNS: usize = 1000;

/// What the multipart framing may add to the file. The body limit is the file's cap plus this, so a
/// client cannot get past [`MAX_IMPORT_BYTES`] by padding the other parts; the file itself is
/// counted exactly, as it streams.
const MULTIPART_OVERHEAD: u64 = 1024 * 1024;

/// How much of the file's start is kept in memory to sniff. A hundred rows of a dozen columns fit
/// many times over; a file whose hundred rows do not is sniffed on fewer of them.
const HEAD_BYTES: usize = 256 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/workspaces/{workspace_id}/imports",
            axum::routing::get(list_imports)
                .post(create_import)
                .layer(DefaultBodyLimit::max(
                    (MAX_IMPORT_BYTES + MULTIPART_OVERHEAD) as usize,
                )),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/imports/{id}",
            axum::routing::get(get_import),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/imports/{id}/start",
            axum::routing::post(start_import),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/imports/{id}/errors",
            axum::routing::get(list_import_errors),
        )
}

/// The request body, for the document: the one part, named `file`.
#[derive(ToSchema)]
#[allow(dead_code)]
pub struct NewImport {
    /// The CSV file. Its name is kept as a label and is never used as a path.
    #[schema(format = Binary)]
    file: String,
}

#[derive(Serialize, ToSchema)]
pub struct UploadColumn {
    /// The header cell, or `Column N` when the file has no header row.
    pub name: String,
    /// Up to five non-blank values from the first 100 rows, as a spreadsheet shows them.
    pub samples: Vec<String>,
    /// `ignore` when the server is not sure: a blank costs one dropdown, a wrong guess writes the
    /// wrong column into the wrong field.
    pub proposed: ColumnTarget,
}

#[derive(Serialize, ToSchema)]
pub struct Upload {
    pub id: Uuid,
    /// One character: `,`, `;`, a tab or `|`.
    pub delimiter: String,
    /// A guess: the first row is a header unless it holds an email address.
    pub has_header: bool,
    pub columns: Vec<UploadColumn>,
    pub total_bytes: u64,
}

/// Removes the temporary file unless the import was recorded. A guard rather than cleanup on the
/// error path, because a handler's future is dropped when the client disconnects mid-upload, and
/// then there is no error path to run.
struct TempFile {
    path: PathBuf,
    keep: bool,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn io(e: std::io::Error) -> ApiError {
    ApiError::Internal(e.into())
}

fn bad_body(message: &str) -> ApiError {
    ApiError::Validation(vec![FieldError::new("body", message)])
}

fn too_large(limit: u64) -> ApiError {
    ApiError::PayloadTooLarge {
        code: "import-too-large",
        detail: format!("the file is over the {limit} byte limit"),
    }
}

/// What a file past [`MAX_IMPORT_ROWS`] is refused with. 422 and not 413: this is a file whose
/// shape this server will not run, not a body too large to hold.
fn too_many_rows() -> ApiError {
    ApiError::UnprocessableDetail {
        code: "too-many-rows",
        detail: format!("the file has more than {MAX_IMPORT_ROWS} rows"),
    }
}

/// A body that hit the framework's own limit is the same refusal as one that hit ours.
fn upload_err(e: MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        too_large(MAX_IMPORT_BYTES)
    } else {
        bad_body("malformed multipart body")
    }
}

/// The client's filename as a label: its last path component, without control characters, at most
/// 255 characters. It is stored and shown; nothing opens it.
fn label(raw: Option<&str>) -> String {
    let base = raw.unwrap_or("").rsplit(['/', '\\']).next().unwrap_or("");
    let name: String = base.chars().filter(|c| !c.is_control()).take(255).collect();
    if name.trim().is_empty() {
        "upload.csv".to_owned()
    } else {
        name
    }
}

/// Streams the `file` part to `path`, counting as it goes. Returns the filename label, the byte
/// count and the start of the file.
async fn receive(
    mut multipart: Multipart,
    path: &Path,
    limit: u64,
) -> ApiResult<(String, u64, Vec<u8>)> {
    let (mut found, mut total, mut head, mut lines) = (None, 0u64, Vec::new(), 0u64);
    while let Some(mut field) = multipart.next_field().await.map_err(upload_err)? {
        if field.name() != Some("file") {
            return Err(ApiError::Validation(vec![FieldError::new(
                field.name().unwrap_or("body"),
                "unknown part; send the CSV as one part named `file`",
            )]));
        }
        if found.is_some() {
            return Err(ApiError::Validation(vec![FieldError::new(
                "file",
                "send one file",
            )]));
        }
        found = Some(label(field.file_name()));
        let mut options = tokio::fs::OpenOptions::new();
        // `create_new`: the id is fresh, so an existing file here is not ours to overwrite.
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut out = options.open(path).await.map_err(io)?;
        while let Some(chunk) = field.chunk().await.map_err(upload_err)? {
            total += chunk.len() as u64;
            if total > limit {
                return Err(too_large(limit));
            }
            // The row cap, counted as the file streams and refused in the same breath as the byte
            // cap: a file this server will not run is not written to its end first. One line is
            // discounted for a header the sniff has not read yet, so a headed file carrying the
            // cap exactly is not refused for its header.
            lines += chunk.iter().filter(|&&b| b == b'\n').count() as u64;
            if lines.saturating_sub(1) > MAX_IMPORT_ROWS {
                return Err(too_many_rows());
            }
            head.extend_from_slice(
                &chunk[..chunk.len().min(HEAD_BYTES.saturating_sub(head.len()))],
            );
            out.write_all(&chunk).await.map_err(io)?;
        }
        out.flush().await.map_err(io)?;
    }
    let Some(filename) = found else {
        return Err(ApiError::Validation(vec![FieldError::new(
            "file", "required",
        )]));
    };
    // A head cut short ends mid-row; sniffing it would report half a cell as a sample.
    if total > head.len() as u64
        && let Some(end) = head.iter().rposition(|&b| b == b'\n')
    {
        head.truncate(end + 1);
    }
    Ok((filename, total, head))
}

/// Takes a CSV as `multipart/form-data` (one part, `file`), keeps it on disk under a name derived
/// from the import's id, records the import as `pending`, and answers with the delimiter, a header
/// guess, and a proposed mapping from each column to a contact field or custom field.
///
/// The proposal names a column only when the match is certain: the whole header, lowercased with
/// everything but letters and digits removed, equals a field's name the same way (`First Name` is
/// `first_name`; `Email Address` is nothing), or is the exporter's own `attr:<key>`, and no other
/// column or field also claims it. Anything else is `ignore`. Reads the first 100 rows and writes nothing to `contacts`.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/imports",
    operation_id = "create_import",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body(content = NewImport, content_type = "multipart/form-data"),
    responses(
        (status = 201, body = Upload),
        (status = 403, description = "insufficient-role: importing takes editor"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 413, description = "import-too-large: the file is over the size limit"),
        (status = 422, description = "not multipart/form-data, no `file` part, an unknown part, more than one file, empty-file: the file has no rows, too-many-columns, or too-many-rows"),
        (status = 429, description = "rate-limited: too many uploads for this workspace"),
    ),
)]
pub async fn create_import(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> ApiResult<Response> {
    // Before the body is read, so a stranger's gigabyte is never written and the answer is the
    // 404 or 403 it would be for any other route. The rejection is taken after for the same reason.
    authorize(&state.pool, &principal, workspace_id, Role::Editor).await?;
    // Also after `authorize`, so a stranger cannot spend the workspace's quota.
    let quota = state.import_limit.check(workspace_id);
    if let Some(retry_after_secs) = quota.over {
        return Ok((
            quota.headers,
            ApiError::TooManyRequests { retry_after_secs },
        )
            .into_response());
    }
    // THE REFUSAL AN OVERSIZE UPLOAD HAS TO GET, AND WHERE IT HAS TO GET IT. A browser sends the
    // whole body before it reads the answer, so a 413 raised while the body streams is one the
    // client may never read: the connection goes down under it and the UI gets a network error
    // where it expected a problem body. The declared length is on the headers, which are already
    // here, so the refusal is taken before a byte of the body is read. The multipart framing is
    // counted in, so a body inside this cannot hold a file over the cap.
    if declared_length(&headers).is_some_and(|n| n > MAX_IMPORT_BYTES + MULTIPART_OVERHEAD) {
        return Ok((quota.headers, too_large(MAX_IMPORT_BYTES)).into_response());
    }
    // Every answer from here carries the `RateLimit` headers, refusals included, as
    // `preview_segment`'s do: the quota was spent whatever the outcome, and the client whose file
    // was refused is exactly the one about to send another.
    Ok(
        match record_upload(&state, meta, &principal, workspace_id, multipart).await {
            Ok(upload) => (StatusCode::CREATED, quota.headers, Json(upload)).into_response(),
            Err(e) => (quota.headers, e).into_response(),
        },
    )
}

/// The body length the client declared, where it declared one. A chunked upload declares none and
/// is counted as it streams instead, by [`receive`].
///
/// **What answering early buys, and where it stops.** The refusal goes out before a byte of the
/// body is read, which is what gets a problem body to a client that is still uploading rather than
/// a dead connection. The server then closes with that body unread, and a close over unread data
/// is a TCP reset — `an_upload_declaring_more_than_the_cap_is_413_with_a_problem_body` shows the
/// answer surviving one, with 8 MB in flight, reading concurrently and not — **but that is Linux
/// over loopback, which is the only stack this repo can test**. Another TCP stack, or a middlebox
/// between, may drop a receive buffer on a reset and take the answer with it. The only cure that
/// does not depend on the client's stack is reading and discarding the whole body before
/// answering, which is the half-gigabyte of work this refusal exists to avoid.
fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// `create_import` past the role check, the quota and the declared length: take the file, sniff
/// it, and record the import.
async fn record_upload(
    state: &AppState,
    meta: RequestMeta,
    principal: &Principal,
    workspace_id: Uuid,
    multipart: Result<Multipart, MultipartRejection>,
) -> ApiResult<Upload> {
    let multipart = multipart.map_err(|_| bad_body("expected multipart/form-data"))?;

    let id = Uuid::now_v7();
    let path = upload_path(id);
    let mut file = TempFile { path, keep: false };
    let (filename, total_bytes, head) = receive(multipart, &file.path, MAX_IMPORT_BYTES).await?;
    let defs = definitions_for(&state.pool, workspace_id).await?;
    let sniffed = sniff(&String::from_utf8_lossy(&head), &defs);
    if sniffed.columns.len() > MAX_IMPORT_COLUMNS {
        return Err(ApiError::UnprocessableDetail {
            code: "too-many-columns",
            detail: format!("the file has more than {MAX_IMPORT_COLUMNS} columns"),
        });
    }
    if sniffed.columns.is_empty() {
        return Err(ApiError::UnprocessableDetail {
            code: "empty-file",
            detail: "the file has no rows".to_owned(),
        });
    }

    // The upload took as long as it took: the role is checked again before anything is recorded.
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, principal, workspace_id, Role::Editor).await?;
    let created_by = match principal {
        Principal::User { user_id, .. } => Some(*user_id),
        Principal::ApiKey { .. } => None,
    };
    sqlx::query!(
        "insert into imports (id, workspace_id, status, filename, delimiter, has_header, created_by) \
         values ($1, $2, 'pending', $3, $4, $5, $6)",
        id,
        workspace_id,
        filename,
        i16::from(sniffed.delimiter),
        sniffed.has_header,
        created_by,
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::IMPORT_UPLOADED,
            target_type: Some("import"),
            target_id: Some(id.to_string()),
            metadata: json!({"filename": filename, "bytes": total_bytes}),
        },
    )
    .await?;
    tx.commit().await?;
    file.keep = true;

    Ok(Upload {
        id,
        delimiter: char::from(sniffed.delimiter).to_string(),
        has_header: sniffed.has_header,
        columns: sniffed
            .columns
            .into_iter()
            .map(|c| UploadColumn {
                name: c.name,
                samples: c.samples,
                proposed: c.proposed,
            })
            .collect(),
        total_bytes,
    })
}

/// What an import did, count by count (spec 7.5): *"'Imported 940,000 of 1,000,000' with no
/// breakdown is not a report."*
///
/// **`imported`, `updated`, `skipped`, `collapsed` and `failed` are the five row-level counts**,
/// and they are mutually exclusive: every row of the file lands in exactly one of them, so on a
/// `complete` import they sum to `total_rows`. (A `failed` import died part-way: its counts are
/// what it got to, and `total_rows` may still be `null`.)
///
/// **`suppressed` and `memberships_declined` are not row-level.** They describe rows already
/// counted among the five — a suppressed address is still an imported or updated contact, and a
/// declined subscription belongs to one — so adding all seven counts those rows twice.
#[derive(Serialize, ToSchema)]
pub struct ImportReport {
    pub id: Uuid,
    /// `pending`, `running`, `complete` or `failed`.
    pub status: String,
    /// What the file was called when it was uploaded. A label; nothing opens it.
    pub filename: String,
    /// `null` while the `COPY` is still counting the file. Not `0`, which would report an empty
    /// file for an import that has not read its first row.
    pub total_rows: Option<i64>,
    /// Rows that became a new contact.
    pub imported: i64,
    /// Rows applied to a contact that already existed.
    pub updated: i64,
    /// Rows left alone because the contact already existed (`skip existing`).
    pub skipped: i64,
    /// Rows a later row of the same file names the same address as. Not `skipped`: the file
    /// de-duplicated itself, and the database never had these rows.
    pub collapsed: i64,
    /// Every rejected row. [`ImportReport::errors_recorded`] is how many of them were kept.
    pub failed: i64,
    /// Imported or updated contacts whose address or domain is suppressed. They exist and they
    /// are not mailable — said here rather than discovered at send time.
    pub suppressed: i64,
    /// Subscriptions this import asked for and did not get, because the address is already
    /// `unsubscribed` on that list. The contact itself was still imported or updated.
    pub memberships_declined: i64,
    /// How many rejected rows `GET /imports/{id}/errors` can show. An import keeps at most
    /// [`MAX_IMPORT_ERRORS`](super::staging::MAX_IMPORT_ERRORS) of them while `failed` counts
    /// every one, so **when this is less than `failed`, that list is the first `errors_recorded`
    /// rejected rows in file order and not all of them** — an operator who pages to the end of it
    /// has not seen everything wrong with the file, and has to be told so.
    pub errors_recorded: i64,
    /// Why the import as a whole failed, and `null` otherwise. Never one row's reason: those are
    /// in `GET /imports/{id}/errors`.
    pub error_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// One rejected row, as the operator has to read it: which row, which column, why, and the cell
/// that did it.
#[derive(Serialize, ToSchema)]
pub struct ImportErrorRow {
    /// **The line the row is on in the file, as a spreadsheet numbers them**, which is what an
    /// operator has to open the file at. The header row takes line 1 of a headed file and a blank
    /// line takes a number of its own, so the first data row of a headed file is line 2 and the
    /// second is line 3.
    ///
    /// It is therefore **not** an index into the data rows and does not run alongside
    /// `total_rows`, which counts data rows only: a headed file of 10 rows reports
    /// `total_rows = 10` and may hold an error on line 11.
    pub row_number: i64,
    /// The column to blame, where one column is to blame.
    pub column_name: Option<String>,
    pub reason: String,
    /// The offending cell as the file held it, cut short. Nothing else of the file is echoed.
    pub value: Option<String>,
}

/// The import's report. Every count is named separately, and the five row-level ones sum to
/// `total_rows` on a `complete` import — see [`ImportReport`] for which five and why the other
/// two are not among them.
#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/imports/{id}",
    operation_id = "get_import",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Import id"),
    ),
    responses(
        (status = 200, body = ImportReport),
        (status = 404, description = "not a member, or no such import in this workspace"),
    ),
)]
pub async fn get_import(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<ImportReport>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    report(&mut conn, workspace_id, id).await.map(Json)
}

/// One import's report, which is also what the run answers with once it has written the counts.
///
/// `fetch_one` scoped by `workspace_id`: another workspace's import is `RowNotFound`, which is
/// the 404 every other tenant-scoped read answers with.
async fn report(conn: &mut PgConnection, workspace_id: Uuid, id: Uuid) -> ApiResult<ImportReport> {
    sqlx::query_as!(
        ImportReport,
        r#"select i.id, i.status, i.filename, i.total_rows, i.imported, i.updated, i.skipped,
                  i.collapsed, i.failed, i.suppressed, i.memberships_declined,
                  (select count(*) from import_errors e
                    where e.workspace_id = i.workspace_id and e.import_id = i.id)
                      as "errors_recorded!",
                  i.error_reason, i.created_at, i.started_at, i.finished_at
             from imports i where i.workspace_id = $1 and i.id = $2"#,
        workspace_id,
        id,
    )
    .fetch_one(conn)
    .await
    .map_err(Into::into)
}

/// This workspace's imports, newest first, a page at a time. Each row is the same report
/// `GET /imports/{id}` answers with: a list screen shows a status and counts, and a second read
/// per row to get them would be the list all over again.
#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/imports",
    operation_id = "list_imports",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), PageParams),
    responses(
        (status = 200, description = "Newest first", body = Page<ImportReport>),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_imports(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<ImportReport>>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    let limit = page.limit();
    // Keyset on `id`, which is a UUIDv7, so `id desc` is newest first. `unique (workspace_id, id)`
    // is the index that answers it; no second index is needed for this list.
    let rows = sqlx::query_as!(
        ImportReport,
        r#"select i.id, i.status, i.filename, i.total_rows, i.imported, i.updated, i.skipped,
                  i.collapsed, i.failed, i.suppressed, i.memberships_declined,
                  (select count(*) from import_errors e
                    where e.workspace_id = i.workspace_id and e.import_id = i.id)
                      as "errors_recorded!",
                  i.error_reason, i.created_at, i.started_at, i.finished_at
             from imports i
            where i.workspace_id = $1 and ($2::uuid is null or i.id < $2)
            order by i.id desc limit $3"#,
        workspace_id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |i| i.id)))
}

/// The rows this import rejected, in file order, a page at a time.
///
/// **This is the first `errors_recorded` rejected rows, not necessarily all of them.** An import
/// keeps at most [`MAX_IMPORT_ERRORS`](super::staging::MAX_IMPORT_ERRORS) error rows while its
/// `failed` count counts every rejected row, so on a worse file the end of this list is the end
/// of what was kept and not the end of the damage. `GET /imports/{id}` reports both numbers, and
/// a client that shows this list shows them together.
#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/imports/{id}/errors",
    operation_id = "list_import_errors",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Import id"),
        PageParams,
    ),
    responses(
        (status = 200, description = "File order, oldest row first", body = Page<ImportErrorRow>),
        (status = 404, description = "not a member, or no such import in this workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_import_errors(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<ImportErrorRow>>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    // An import that is not this workspace's is a 404 and not an empty page: "no such import" and
    // "this import rejected nothing" are different facts, and only one of them is this client's
    // business. The listing below would answer both with `[]`.
    sqlx::query_scalar!(
        "select 1 from imports where workspace_id = $1 and id = $2",
        workspace_id,
        id,
    )
    .fetch_one(&mut *conn)
    .await?;
    let limit = page.limit();
    // Keyset on `row_number`, which `import_errors_by_import` is ordered by, and scoped on
    // `workspace_id` as well as `import_id` so the index is the one that answers it.
    //
    // shortcut: one error row per rejected row, so `row_number` is the whole key. Every writer
    // refuses a row whole (`stage_row` returns one `RowError`, the erasure sweeps report one row
    // each). If a stage ever files two errors for one row, this cursor drops the second: give it
    // a tiebreaker column then.
    let rows = sqlx::query_as!(
        ImportErrorRow,
        "select row_number, column_name, reason, value from import_errors \
         where workspace_id = $1 and import_id = $2 and ($3::bigint is null or row_number > $3) \
         order by row_number limit $4",
        workspace_id,
        id,
        page.after_row()?,
        limit + 1,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(Page::from_numbered_rows(rows, limit, |e| {
        e.row_number
    })))
}

/// The confirmed mapping, and the run.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StartImport {
    /// One target per CSV column, in the file's column order. One column must be `email`, and no
    /// two columns may name the same single-write target.
    pub mapping: Vec<ColumnTarget>,
    /// What to do with a row whose address is already a contact here. `update_existing` by
    /// default, which is spec 7.3's blank-cell rule: a blank leaves the stored value alone.
    #[serde(default)]
    pub dedup: Dedup,
    /// A list in this workspace to subscribe every imported contact to. The file's own `lists`
    /// column, where it has one, is added to this rather than replacing it.
    pub list_id: Option<Uuid>,
    /// Replaces the delimiter the upload sniffed: one of `,`, `;`, a tab or `|`. Left out, the
    /// stored one is used — the file is never sniffed a second time.
    pub delimiter: Option<String>,
    /// Replaces the header guess the upload made. Left out, the stored one is used.
    pub has_header: Option<bool>,
}

/// What a start says when the upload is not on this server any more. A constant because it is both
/// the problem's detail and the import's `error_reason`, and the two have to be the same sentence.
const FILE_GONE: &str = "the uploaded file is no longer on this server; upload it again";

/// Removes an import's uploaded file once that import is over, on every path that ends one.
///
/// A failure is logged and nothing else: the import has already committed, so there is nothing
/// left to refuse, and the operator cannot act on it anyway. But it is **logged**, because the
/// thing this call exists to prevent — half a gigabyte of someone's contacts left on disk for
/// ever — otherwise happens in silence. The path is [`upload_path`]'s, derived from the import id,
/// so it carries nothing secret. "Already gone" is not a failure: that is the `import-file-gone`
/// path, which ends an import whose file was the thing that was missing.
///
/// Answers whether THIS call removed the file, which is what [`sweep_orphaned_uploads`] counts: a
/// sweep that reported the files it meant to take rather than the ones it took would log work it
/// had not done.
async fn remove_upload(path: &Path) -> bool {
    match tokio::fs::remove_file(path).await {
        Ok(()) => true,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "could not remove an import's uploaded file",
                );
            }
            false
        }
    }
}

/// Ends a run that was refused: the row says `failed` and why, the `import_errors` rows the `COPY`
/// had already written are **kept** (that is the whole point — see the savepoint in
/// [`start_import`]), and the file goes.
///
/// `total_rows` is `None` where the `COPY` never counted the file. Null is "still unknown", which
/// a `0` would report as an empty file.
async fn record_refusal(
    mut tx: Transaction<'_, Postgres>,
    job: &ImportJob,
    reason: &str,
    total_rows: Option<i64>,
    failed: i64,
) -> ApiResult<()> {
    sqlx::query!(
        "update imports set status = 'failed', error_reason = $3, total_rows = $4, failed = $5, \
         finished_at = now() where workspace_id = $1 and id = $2",
        job.workspace_id,
        job.id,
        reason,
        total_rows,
        failed,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    remove_upload(&job.path).await;
    Ok(())
}

/// The detail a refusal carries, for `imports.error_reason`. `None` for anything else: an internal
/// error's cause is never shown to a client, and it must not be stored where admins read it
/// either. Such an error also leaves a transaction that may not be used.
fn refusal_detail(e: &ApiError) -> Option<&str> {
    match e {
        ApiError::ConflictDetail { detail, .. } | ApiError::UnprocessableDetail { detail, .. } => {
            Some(detail)
        }
        _ => None,
    }
}

/// What `imports.dedup` stores, which is serde's own name for the variant and so the wire name the
/// request used. A unit test holds the two together.
fn dedup_name(dedup: Dedup) -> &'static str {
    match dedup {
        Dedup::UpdateExisting => "update_existing",
        Dedup::SkipExisting => "skip_existing",
        Dedup::FailOnDuplicate => "fail_on_duplicate",
    }
}

/// A delimiter override: one of the four [`DELIMITERS`] the sniff itself chooses between. A `"`, a
/// newline or a letter would parse the file in a way the operator was never shown, so it is
/// refused rather than obeyed.
fn delimiter_byte(s: &str) -> ApiResult<u8> {
    match s.as_bytes() {
        [b] if DELIMITERS.contains(b) => Ok(*b),
        _ => Err(ApiError::Validation(vec![FieldError::new(
            "delimiter",
            "must be one of `,`, `;`, a tab or `|`",
        )])),
    }
}

/// What a mapping has to be before a single row is read.
///
/// Both refusals exist because the alternative is found out one row at a time, after the run has
/// written everything else. A mapping naming a custom field **deleted since the sniff** fails
/// every row of the file with the same message — the proposal was made against the fields as they
/// were at the upload, and the API takes a mapping directly, so a client-side guard is not one.
/// Two columns aimed at one field quietly keep whichever the staged row wrote last, and nothing
/// tells the operator which column was dropped. Tags and lists accumulate and are exempt.
fn check_mapping(mapping: &[ColumnTarget], defs: &[Definition]) -> ApiResult<()> {
    for (i, target) in mapping.iter().enumerate() {
        let name = match target {
            // `ignore` writes nothing, and tags and lists ACCUMULATE: a file with its tags in two
            // columns is a file with two tag columns, not a collision.
            ColumnTarget::Ignore
            | ColumnTarget::Field(ContactField::Tags | ContactField::Lists) => {
                continue;
            }
            ColumnTarget::Field(f) => f.header().to_owned(),
            ColumnTarget::Custom(key) => {
                let message = match defs.iter().find(|d| d.key == *key) {
                    None => Some("no such custom field"),
                    // The same sentence `validate_attributes` would have given, a million rows
                    // later: `definitions_for` returns archived fields so old values stay
                    // readable, and an archived field takes no new ones.
                    Some(d) if d.archived_at.is_some() => {
                        Some("this custom field is archived and takes no new values")
                    }
                    Some(_) => None,
                };
                if let Some(message) = message {
                    return Err(ApiError::Validation(vec![FieldError::new(
                        format!("mapping[{i}]"),
                        message,
                    )]));
                }
                format!("attr:{key}")
            }
        };
        if let Some(j) = mapping[i + 1..].iter().position(|t| t == target) {
            return Err(ApiError::UnprocessableDetail {
                code: "duplicate-mapping-target",
                detail: format!(
                    "columns {} and {} are both mapped to `{name}`; only tags and lists take more \
                     than one column",
                    i + 1,
                    i + 2 + j,
                ),
            });
        }
    }
    Ok(())
}

/// Confirms the mapping and runs the import: `COPY` the file into a staging table, upsert out of
/// it, and write what that came to onto the import's row.
///
/// **One transaction for the whole run.** The staging table is `create temporary table … on commit
/// drop`, so it exists only inside it, and that `COMMIT` is what makes the contacts durable.
///
/// **The delimiter and the header come from the stored values, never from a second sniff** — the
/// operator confirmed a mapping against those columns, and a sniff that disagreed at this point
/// would silently re-point every column. The request may carry `delimiter` and `has_header`
/// OVERRIDES, which replace them: a headerless file read as headed silently drops its first row,
/// and a misdetected delimiter misparses every row, and the operator who was shown the guess is
/// the one who can correct it. A request carrying neither uses what is stored.
///
/// **A refusal keeps the per-row report.** See the savepoint below.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/imports/{id}/start",
    operation_id = "start_import",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Import id"),
    ),
    request_body = StartImport,
    responses(
        (status = 200, description = "The import ran; every count is on the report", body = ImportReport),
        (status = 403, description = "insufficient-role: running an import takes editor"),
        (status = 404, description = "not a member, no such import in this workspace, or no such list"),
        (status = 409, description = "import-in-progress: this workspace is already running an \
            import; import-already-started: an import runs once; or import-duplicate-address, \
            which writes no contact and keeps the rejected rows"),
        (status = 422, description = "a mapping naming a custom field that no longer takes values, \
            two columns aimed at one target, mapping-needs-email, a delimiter that is not one of \
            the four, import-file-gone, or an unknown field"),
    ),
)]
pub async fn start_import(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<StartImport>,
) -> ApiResult<Json<ImportReport>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    // ONE IMPORT AT A TIME PER WORKSPACE (section 7.4), as an advisory lock keyed on the
    // workspace and NOT a status check. This run writes `running` inside this transaction and
    // commits once, at the end, so a concurrent start cannot see it: two requests would both read
    // `pending` and both pass. The lock is taken before the row below is read, because it has to
    // be held before this request can see another request's status, and it lives on this
    // transaction, so it covers the whole run and is released by the COMMIT or the rollback,
    // whichever comes.
    //
    // `try`, not a wait: a second start is answered now, and says what is holding the workspace,
    // rather than queueing behind a million rows while holding a pooled connection.
    //
    // The `running` row read beside it is the other half of the guard. A lock is held by a live
    // transaction and so says nothing about an import whose process died; such a row is swept to
    // `failed` at the next boot by [`sweep_running_imports`], and until that boot it holds the
    // workspace. It is also the one case where the refusal can name the import, because it is the
    // only case where the `running` row has been committed.
    //
    // `for update` below is neither half and does not stand in for them. It locks THIS import's
    // row, so two starts of the same import cannot both read `pending`; one import per WORKSPACE
    // is a different claim over rows that statement never sees.
    //
    // THE ORDERING HAS A CONSEQUENCE WORTH NAMING. Because this runs before the import is read, a
    // start naming an id that does not exist — or one belonging to another workspace — answers
    // `409 import-in-progress` rather than 404 while this workspace has a committed `running`
    // import. That is not a tenancy leak: the 409 is decided entirely by the caller's OWN
    // workspace, and says nothing about whether the id exists anywhere. Reading the import first
    // would give the 404 priority, and would cost the thing the comment above argues for — the
    // lock has to be held before this request can see another request's state.
    let guard = sqlx::query!(
        // The seed does not create a namespace. `pg_advisory_xact_lock(bigint)` has ONE 64-bit key
        // space for the whole cluster, and `suppressions::lock_address` is already in it. What the
        // seed does is keep this key off the plain `hashtextextended(workspace_id::text, 0)` that
        // another feature locking a workspace would reach for first, so the two cannot become the
        // same key by having had the same idea. A collision with an unrelated key is a 1-in-2^64
        // event; this side would answer a spurious 409 and never hang, because it is `try`.
        r#"select pg_try_advisory_xact_lock(hashtextextended($1::uuid::text, 7711)) as "held!",
                  (select id from imports
                    where workspace_id = $1 and status = 'running' limit 1) as "running?""#,
        workspace_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if !guard.held || guard.running.is_some() {
        return Err(ApiError::ConflictDetail {
            code: "import-in-progress",
            detail: match guard.running {
                Some(other) => format!(
                    "import {other} is already running in this workspace; one import runs at a \
                     time, and this one can be started when that one is over"
                ),
                // The holder's `running` is inside its own transaction, so there is no id to name.
                None => "another import is already running in this workspace; one import runs at \
                         a time, and this one can be started when that one is over"
                    .to_owned(),
            },
        });
    }
    let row = sqlx::query!(
        "select status, delimiter, has_header from imports \
         where workspace_id = $1 and id = $2 for update",
        workspace_id,
        id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if row.status != "pending" {
        return Err(ApiError::ConflictDetail {
            code: "import-already-started",
            detail: format!(
                "this import is already `{}`; an import runs once, and a second file is a second \
                 upload",
                row.status
            ),
        });
    }

    let defs = definitions_for(&mut *tx, workspace_id).await?;
    check_mapping(&input.mapping, &defs)?;
    let delimiter = match &input.delimiter {
        Some(d) => delimiter_byte(d)?,
        // `check (delimiter between 0 and 255)` on the column, so this cannot narrow.
        None => u8::try_from(row.delimiter).map_err(|e| ApiError::Internal(e.into()))?,
    };
    if let Some(list_id) = input.list_id {
        // 404, as every other id in a body that names another workspace's row. Without it the
        // composite foreign key refuses the update below and the operator reads `internal error`.
        lists::get(&mut tx, workspace_id, list_id).await?;
    }
    let job = ImportJob {
        id,
        workspace_id,
        path: upload_path(id),
        delimiter,
        has_header: input.has_header.unwrap_or(row.has_header),
        mapping: input.mapping,
        dedup: input.dedup,
        list_id: input.list_id,
        defs,
    };

    // What the run was asked to do, on the row, before it does any of it. The overrides are
    // written back because they are what parsed the file: a row still claiming the sniff's guess
    // would describe an import that did not happen.
    let mapping = serde_json::to_value(&job.mapping).map_err(|e| ApiError::Internal(e.into()))?;
    sqlx::query!(
        "update imports set status = 'running', started_at = now(), mapping = $3, dedup = $4, \
         list_id = $5, delimiter = $6, has_header = $7 where workspace_id = $1 and id = $2",
        workspace_id,
        id,
        mapping,
        dedup_name(job.dedup),
        job.list_id,
        i16::from(job.delimiter),
        job.has_header,
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::IMPORT_STARTED,
            target_type: Some("import"),
            target_id: Some(id.to_string()),
            // The import's own settings and nothing out of the file. A header, a cell and an
            // address are the file's contents, and this log is stored in plaintext.
            metadata: json!({
                "dedup": dedup_name(job.dedup),
                "columns": job.mapping.len(),
                "list_id": job.list_id,
            }),
        },
    )
    .await?;

    let file = match tokio::fs::File::open(&job.path).await {
        Ok(file) => file,
        // THE IMPORT IS OVER, not back where it started. No retry can bring the file back, so a
        // `?` here — which would roll `running` back to `pending` — leaves a row in `GET /imports`
        // that answers 422 for ever with nothing that can ever clear it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // `total_rows` stays null: the `COPY` never counted the file, and null is "still
            // unknown", which a `0` would report as an empty file.
            record_refusal(tx, &job, FILE_GONE, None, 0).await?;
            return Err(ApiError::UnprocessableDetail {
                code: "import-file-gone",
                detail: FILE_GONE.to_owned(),
            });
        }
        // AND ONLY THAT ERROR ENDS THE IMPORT. Every other one — a descriptor the rest of the
        // process has exhausted, a permission that changed under it — is a request that can be
        // made again, and `?` is the retry: it rolls `running` back to `pending`, where the
        // operator left it. Ending the import on one of those throws away a 512 MB upload, and no
        // retry brings it back.
        Err(e) => return Err(io(e)),
    };
    let staged = copy_into_staging(&mut tx, &state.secrets, &job, file).await?;

    // A SAVEPOINT, so that a refusal can take the upsert's writes back WITHOUT taking back the
    // `import_errors` rows the `COPY` wrote above.
    let mut run = tx.begin().await?;
    let counts = match upsert_from_staging(&mut run, &job, staged.failed).await {
        Ok(counts) => {
            run.commit().await?;
            counts
        }
        Err(e) => {
            // THE DECISION A CALLER OF `copy_into_staging` HAS TO MAKE, AND NOTHING ENFORCES.
            // `fail on duplicate` raises its 409 from a pre-flight `SELECT`, so the transaction is
            // still usable — and a caller that rolled all of it back would tell an operator that
            // `bob@example.com` is already a contact while never mentioning the twelve unreadable
            // rows in the same file. The per-row report is the thing they fix the file with; the
            // contacts are what they asked not to have, and the savepoint takes back only those.
            //
            // Anything that is not a refusal carries no detail that may be shown and leaves a
            // transaction that may not be used: it is re-raised with nothing recorded, and the
            // rollback puts the import back to `pending` for the operator to start again.
            let Some(reason) = refusal_detail(&e).map(str::to_owned) else {
                return Err(e);
            };
            run.rollback().await?;
            // `total_rows` and `failed` too, so the kept rows and the counts agree: a report
            // saying `failed = 0` beside a page of rejected rows is a report nobody believes.
            record_refusal(tx, &job, &reason, Some(staged.total_rows), staged.failed).await?;
            return Err(e);
        }
    };

    // THE EIGHT COLUMNS NOTHING ELSE IN `src/` WRITES. Seven come from `UpsertCounts`, which has
    // already apportioned them — the caller has nothing to add up. `total_rows` is the `COPY`'s
    // own count of the file and never a recount: a recount counts the rows that reached staging,
    // and would report a file with twelve unreadable rows as twelve rows shorter than it is.
    sqlx::query!(
        "update imports set status = 'complete', finished_at = now(), total_rows = $3, \
         imported = $4, updated = $5, skipped = $6, collapsed = $7, failed = $8, suppressed = $9, \
         memberships_declined = $10 where workspace_id = $1 and id = $2",
        workspace_id,
        id,
        staged.total_rows,
        counts.imported,
        counts.updated,
        counts.skipped,
        counts.collapsed,
        counts.failed,
        counts.suppressed,
        counts.memberships_declined,
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::IMPORT_FINISHED,
            target_type: Some("import"),
            target_id: Some(id.to_string()),
            // Numbers, which is all a count is. Still nothing out of the file.
            metadata: json!({
                "total_rows": staged.total_rows,
                "imported": counts.imported,
                "updated": counts.updated,
                "skipped": counts.skipped,
                "collapsed": counts.collapsed,
                "failed": counts.failed,
                "suppressed": counts.suppressed,
                "memberships_declined": counts.memberships_declined,
            }),
        },
    )
    .await?;
    let done = report(&mut tx, workspace_id, id).await?;
    tx.commit().await?;
    // The file has been imported. Nothing reads it again — a second start is a 409 — and leaving
    // it is half a gigabyte of someone's contacts on disk for ever.
    remove_upload(&job.path).await;
    Ok(Json(done))
}

/// What a swept import's `error_reason` says. A constant because the sweep writes it and the
/// operator reads it on the report, and the two are the same sentence.
const SWEPT: &str = "the server restarted while this import was running; its file was not fully \
                     imported, so upload it again and start it";

/// Fails every import left `running`, and answers how many. **Called once at boot, before the
/// first request.**
///
/// A row stuck in `running` for ever is worse than a clear failure: `GET /imports` reports it as
/// in progress, nothing will ever finish it, and the workspace's guard in [`start_import`] holds
/// against it until someone edits the database.
///
/// Instance-wide and not scoped to a workspace: a boot has no request and no tenant.
///
/// **What it can sweep today is narrower than it looks.** A run is one transaction that commits
/// `complete` or `failed`, so an instance killed mid-import rolls back to `pending` and leaves
/// nothing here. What this catches is a row committed `running` by anything else — a hand-run
/// `UPDATE`, a restore of a dump taken mid-transaction on another system, or the background runner
/// this route becomes when a million rows stop fitting in one request. It is cheap, and the state
/// it clears is one nothing else can clear.
///
/// **And on that day this predicate is not enough.** It has no age and no owner: every `running`
/// row goes, whoever wrote it and however recently. That is harmless while no live import can be
/// in that state, and wrong the moment one can — a second instance booting would fail the first
/// instance's import mid-run. Whatever makes `running` reachable has to bring a predicate with it:
/// an instance id on the row, or an age past which a run cannot still be alive.
pub async fn sweep_running_imports(pool: &PgPool) -> sqlx::Result<u64> {
    let swept = sqlx::query!(
        "update imports set status = 'failed', error_reason = $1, finished_at = now() \
         where status = 'running'",
        SWEPT,
    )
    .execute(pool)
    .await?;
    Ok(swept.rows_affected())
}

/// Removes the uploaded file of every import that is over, and answers how many it took. **Called
/// once at boot, after [`sweep_running_imports`], whose newly failed imports are part of its
/// work.**
///
/// Its own pass and not part of the sweep above: one is a transactional count of rows and the
/// other is a best-effort walk of a directory, they fail in different ways, and an upload is not
/// deleted on the strength of a row the same statement has just changed. [`remove_upload`] already
/// runs on every path that ends an import, so what is left here is what never reached one — a
/// process killed between the COMMIT and the unlink, or an unlink that failed.
///
/// **A `pending` import's file is kept**, because that file is exactly what its start will read:
/// an upload sitting unstarted is someone's mapping screen, not litter. So an upload that is never
/// started is still on disk until its import ends; the cure for that is an expiry on pending
/// imports, which is a policy decision and not a sweep's to take.
///
/// **A file with no import row is kept too.** This is the system temp directory, and a file this
/// database cannot account for is not ours to delete — the same reason the upload opens with
/// `create_new`.
///
/// shortcut: it attributes a file by its id alone, so two instances sharing a temp directory and
/// one database sweep each other's correctly, and the file of an import deleted from the database
/// is left behind. Give the uploads a configured directory of their own if either ever matters.
pub async fn sweep_orphaned_uploads(pool: &PgPool) -> sqlx::Result<u64> {
    let mut found = Vec::new();
    let dir = std::env::temp_dir();
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(error = %e, path = %dir.display(), "could not read the upload directory");
            return Ok(0);
        }
    };
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                if let Some(id) = entry
                    .file_name()
                    .to_string_lossy()
                    .strip_prefix("nm-import-")
                    .and_then(|rest| rest.strip_suffix(".csv"))
                    .and_then(|id| id.parse::<Uuid>().ok())
                {
                    found.push(id);
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, path = %dir.display(), "the upload directory ended early");
                break;
            }
        }
    }
    if found.is_empty() {
        return Ok(0);
    }
    let over = sqlx::query_scalar!(
        "select id from imports where id = any($1) and status <> 'pending'",
        &found,
    )
    .fetch_all(pool)
    .await?;
    let mut removed = 0;
    for id in &over {
        removed += u64::from(remove_upload(&upload_path(*id)).await);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        extract::FromRequest,
        http::{Request, header},
    };

    use super::*;

    async fn part(file_bytes: usize) -> Multipart {
        let body = format!(
            "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.csv\"\r\n\r\n{}\r\n--b--\r\n",
            "x".repeat(file_bytes)
        );
        let req = Request::post("/")
            .header(header::CONTENT_TYPE, "multipart/form-data; boundary=b")
            .body(Body::from(body))
            .unwrap();
        Multipart::from_request(req, &()).await.unwrap()
    }

    #[tokio::test]
    async fn a_file_over_the_limit_is_refused_as_413_and_one_at_it_is_kept() {
        let path = std::env::temp_dir().join(format!("nm-import-test-{}", Uuid::now_v7()));
        let over = receive(part(21).await, &path, 20).await;
        assert!(matches!(
            over,
            Err(ApiError::PayloadTooLarge {
                code: "import-too-large",
                ..
            })
        ));
        // `receive` itself leaves the partial file: removing it is the guard's job, tested below.
        assert!(path.exists());
        std::fs::remove_file(&path).unwrap();

        let (_, bytes, _) = receive(part(20).await, &path, 20).await.unwrap();
        assert_eq!(bytes, 20);
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn the_guard_removes_a_partial_file_when_dropped_and_keeps_a_recorded_one() {
        let dir = std::env::temp_dir();
        let (gone, kept) = (
            dir.join(format!("nm-import-test-{}", Uuid::now_v7())),
            dir.join(format!("nm-import-test-{}", Uuid::now_v7())),
        );
        {
            let _guard = TempFile {
                path: gone.clone(),
                keep: false,
            };
            let over = receive(part(21).await, &gone, 20).await;
            assert!(over.is_err() && gone.exists());
        }
        assert!(
            !gone.exists(),
            "dropping the guard removes what was written"
        );

        std::fs::write(&kept, b"x").unwrap();
        drop(TempFile {
            path: kept.clone(),
            keep: true,
        });
        assert!(kept.exists());
        std::fs::remove_file(&kept).unwrap();
    }

    /// `imports.dedup` holds what the request said, so the two names cannot drift apart.
    #[test]
    fn the_stored_dedup_is_the_name_the_request_used() {
        for dedup in [
            Dedup::UpdateExisting,
            Dedup::SkipExisting,
            Dedup::FailOnDuplicate,
        ] {
            assert_eq!(json!(dedup), json!(dedup_name(dedup)));
        }
    }

    #[test]
    fn a_filename_is_a_label_not_a_path() {
        assert_eq!(label(Some("../../etc/passwd.csv")), "passwd.csv");
        assert_eq!(label(Some(r"C:\Users\x\book.csv")), "book.csv");
        assert_eq!(label(Some("a\u{0}b\n.csv")), "ab.csv");
        assert_eq!(label(Some("../")), "upload.csv");
        assert_eq!(label(None), "upload.csv");
    }
}
