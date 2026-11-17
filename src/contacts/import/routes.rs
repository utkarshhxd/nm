// SPDX-License-Identifier: AGPL-3.0-only

//! `POST /imports`: the upload, and the server's reading of its head. Nothing here writes to
//! `contacts`; the run is a later request.

use std::path::{Path, PathBuf};

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Multipart,
        multipart::{MultipartError, MultipartRejection},
    },
    http::StatusCode,
};
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
    error::{ApiError, ApiPath, ApiResult, FieldError},
    http::request_meta::RequestMeta,
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
    Router::new().route(
        "/api/v1/workspaces/{workspace_id}/imports",
        axum::routing::post(create_import).layer(DefaultBodyLimit::max(
            (MAX_IMPORT_BYTES + MULTIPART_OVERHEAD) as usize,
        )),
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
