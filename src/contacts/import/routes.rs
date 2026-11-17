// SPDX-License-Identifier: AGPL-3.0-only

//! `POST /imports`: the upload, and the server's reading of its head. Nothing here writes to
//! `contacts`; the run is a later request.
//!
//! And the two reads an operator is left with afterwards: `GET /imports/{id}`, the report, and
//! `GET /imports/{id}/errors`, the rows it rejected.

use std::path::{Path, PathBuf};

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Multipart,
        multipart::{MultipartError, MultipartRejection},
    },
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{ColumnTarget, sniff::sniff, upload_path};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    custom_fields::definitions_for,
    error::{ApiError, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    pagination::{Page, PageParams},
    workspaces::roles::Role,
};

use axum::extract::State;

/// The largest upload accepted, in bytes. The cap on rows is Task 8's and is 1M; at the few hundred
/// bytes a contact row takes with a dozen attributes that is well under this. A named limit and not
/// configuration, as `MAX_RULES` and `CHUNK_BYTES` are: it is a bound on what one request may make
/// the disk hold, and an operator who raises it should have to read this comment.
pub const MAX_IMPORT_BYTES: u64 = 512 * 1024 * 1024;

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
            axum::routing::post(create_import).layer(DefaultBodyLimit::max(
                (MAX_IMPORT_BYTES + MULTIPART_OVERHEAD) as usize,
            )),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/imports/{id}",
            axum::routing::get(get_import),
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
    let (mut found, mut total, mut head) = (None, 0u64, Vec::new());
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
        (status = 422, description = "not multipart/form-data, no `file` part, an unknown part, more than one file, empty-file: the file has no rows, or too-many-columns"),
    ),
)]
pub async fn create_import(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    multipart: Result<Multipart, MultipartRejection>,
) -> ApiResult<(StatusCode, Json<Upload>)> {
    // Before the body is read, so a stranger's gigabyte is never written and the answer is the
    // 404 or 403 it would be for any other route. The rejection is taken after for the same reason.
    authorize(&state.pool, &principal, workspace_id, Role::Editor).await?;
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
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let created_by = match &principal {
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

    Ok((
        StatusCode::CREATED,
        Json(Upload {
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
        }),
    ))
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
    /// The row's place in the file, counting data rows only — a header row is not one.
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
    // `fetch_one` scoped by `workspace_id`: another workspace's import is `RowNotFound`, which is
    // the 404 every other tenant-scoped read answers with.
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
    .fetch_one(&mut *conn)
    .await
    .map(Json)
    .map_err(Into::into)
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

    #[test]
    fn a_filename_is_a_label_not_a_path() {
        assert_eq!(label(Some("../../etc/passwd.csv")), "passwd.csv");
        assert_eq!(label(Some(r"C:\Users\x\book.csv")), "book.csv");
        assert_eq!(label(Some("a\u{0}b\n.csv")), "ab.csv");
        assert_eq!(label(Some("../")), "upload.csv");
        assert_eq!(label(None), "upload.csv");
    }
}
