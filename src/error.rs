// SPDX-License-Identifier: AGPL-3.0-only

use axum::{
    Json,
    extract::{
        FromRequest, FromRequestParts, Path, Query, Request,
        path::ErrorKind,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{HeaderValue, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

/// One invalid input field in a 422 `validation` problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

impl FieldError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> FieldError {
        FieldError {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// Every API failure. Renders as an RFC 9457 `application/problem+json` body
/// with `type` = `urn:nm:problem:<code>`.
#[derive(Debug)]
pub enum ApiError {
    /// 422 `validation`; the body carries `errors: [{field, message}]`.
    Validation(Vec<FieldError>),
    /// 401 with the given problem code.
    Unauthorized(&'static str),
    /// 401 with a problem code and a fixed, user-facing detail (the default detail is the code).
    UnauthorizedDetail {
        code: &'static str,
        detail: &'static str,
    },
    /// 403 with the given problem code.
    Forbidden(&'static str),
    /// 404 `not-found`. Also the answer for another workspace's resources.
    NotFound,
    /// 409 with the given problem code.
    Conflict(&'static str),
    /// 422 with the given problem code, for a request that is well-formed but cannot be acted on
    /// and is not one field's fault (an expired selection token).
    Unprocessable(&'static str),
    /// 429 `rate-limited` plus a `Retry-After` header.
    TooManyRequests { retry_after_secs: u64 },
    /// 415 `unsupported-media-type`: a JSON endpoint got another content type.
    UnsupportedMediaType,
    /// 503 with the given problem code: the instance cannot serve this request right now.
    ServiceUnavailable(&'static str),
    /// 500 `internal`. The detail is always `internal error`; the cause is only logged.
    Internal(anyhow::Error),
}

pub type ApiResult<T> = Result<T, ApiError>;

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> ApiError {
        ApiError::Internal(e)
    }
}

impl From<sqlx::Error> for ApiError {
    /// `RowNotFound` is a 404: a `fetch_one` scoped by `workspace_id` on another
    /// workspace's id must answer not-found. Use `fetch_optional` to decide otherwise.
    fn from(e: sqlx::Error) -> ApiError {
        match e {
            sqlx::Error::RowNotFound => ApiError::NotFound,
            e => ApiError::Internal(e.into()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut errors = None;
        let mut retry_after = None;
        let (status, code, detail) = match &self {
            ApiError::Validation(list) => {
                errors = Some(list);
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "validation",
                    "request validation failed".to_owned(),
                )
            }
            ApiError::Unauthorized(code) => (StatusCode::UNAUTHORIZED, *code, humanise(code)),
            ApiError::UnauthorizedDetail { code, detail } => {
                (StatusCode::UNAUTHORIZED, *code, (*detail).to_owned())
            }
            ApiError::Forbidden(code) => (StatusCode::FORBIDDEN, *code, humanise(code)),
            ApiError::NotFound => (
                StatusCode::NOT_FOUND,
                "not-found",
                "resource not found".to_owned(),
            ),
            ApiError::Conflict(code) => (StatusCode::CONFLICT, *code, humanise(code)),
            ApiError::Unprocessable(code) => {
                (StatusCode::UNPROCESSABLE_ENTITY, *code, humanise(code))
            }
            ApiError::TooManyRequests { retry_after_secs } => {
                retry_after = Some(*retry_after_secs);
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate-limited",
                    "too many requests".to_owned(),
                )
            }
            ApiError::UnsupportedMediaType => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported-media-type",
                "expected a JSON request body".to_owned(),
            ),
            ApiError::ServiceUnavailable(code) => {
                (StatusCode::SERVICE_UNAVAILABLE, *code, humanise(code))
            }
            ApiError::Internal(cause) => {
                tracing::error!(error = ?cause, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "internal error".to_owned(),
                )
            }
        };
        let mut body = json!({
            "type": format!("urn:nm:problem:{code}"),
            "title": status.canonical_reason().unwrap_or("Error"),
            "status": status.as_u16(),
            "detail": detail,
        });
        if let Some(list) = errors {
            body["errors"] = Value::from(
                list.iter()
                    .map(|e| json!({"field": e.field, "message": e.message}))
                    .collect::<Vec<_>>(),
            );
        }
        let mut res = (status, Json(body)).into_response();
        res.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        if let Some(secs) = retry_after {
            res.headers_mut().insert(header::RETRY_AFTER, secs.into());
        }
        res
    }
}

fn humanise(code: &str) -> String {
    code.replace('-', " ")
}

/// Maps axum's JSON body rejections to problems in one place: a wrong content
/// type is 415, anything else wrong with the body is a 422 `validation`.
impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> ApiError {
        match rejection {
            JsonRejection::MissingJsonContentType(_) => ApiError::UnsupportedMediaType,
            // Serde's message is not echoed: it can quote request content.
            _ => ApiError::Validation(vec![FieldError::new("body", "invalid JSON body")]),
        }
    }
}

/// `axum::Json` as a request extractor that rejects with `ApiError`. Handlers
/// take this instead of `Json<T>` so body rejections come back as problems.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        Ok(ApiJson(Json::<T>::from_request(req, state).await?.0))
    }
}

/// A malformed query string is a 422 `validation` problem. Serde's message is not
/// echoed (it can quote request content) and carries no field name, so it is `query`.
impl From<QueryRejection> for ApiError {
    fn from(_: QueryRejection) -> ApiError {
        ApiError::Validation(vec![FieldError::new("query", "invalid query string")])
    }
}

/// A malformed path parameter is a 422 `validation` problem naming the route
/// template's parameter. A missing parameter is a routing bug, so it is a 500.
impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> ApiError {
        let field = match &rejection {
            PathRejection::FailedToDeserializePathParams(e) => match e.kind() {
                ErrorKind::ParseErrorAtKey { key, .. }
                | ErrorKind::InvalidUtf8InPathParam { key }
                | ErrorKind::DeserializeError { key, .. } => key.clone(),
                _ => "path".to_owned(),
            },
            PathRejection::MissingPathParams(_) => {
                return ApiError::Internal(anyhow::anyhow!(rejection));
            }
            _ => "path".to_owned(),
        };
        ApiError::Validation(vec![FieldError::new(field, "invalid path parameter")])
    }
}

/// `axum::extract::Query` that rejects with `ApiError`.
pub struct ApiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Ok(ApiQuery(
            Query::<T>::from_request_parts(parts, state).await?.0,
        ))
    }
}

/// `axum::extract::Path` that rejects with `ApiError`.
pub struct ApiPath<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiPath<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Ok(ApiPath(
            Path::<T>::from_request_parts(parts, state).await?.0,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn internal_error_never_leaks_cause() {
        let res = ApiError::Internal(anyhow::anyhow!("password=hunter2")).into_response();
        assert_eq!(res.status(), 500);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains("hunter2"));
        assert!(text.contains(r#""detail":"internal error""#));
    }

    #[tokio::test]
    async fn validation_lists_field_errors_and_429_sets_retry_after() {
        let res = ApiError::Validation(vec![FieldError::new("email", "required")]).into_response();
        assert_eq!(res.status(), 422);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["errors"][0]["field"], "email");
        assert_eq!(v["type"], "urn:nm:problem:validation");

        let res = ApiError::TooManyRequests {
            retry_after_secs: 7,
        }
        .into_response();
        assert_eq!(res.status(), 429);
        assert_eq!(res.headers()["retry-after"], "7");
    }

    async fn problem(err: ApiError) -> (StatusCode, Value) {
        let res = err.into_response();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn coded_errors_map_status_type_and_detail() {
        for (err, status, code, detail) in [
            (
                ApiError::Unauthorized("invalid-credentials"),
                401,
                "invalid-credentials",
                "invalid credentials",
            ),
            (
                ApiError::Forbidden("csrf-failed"),
                403,
                "csrf-failed",
                "csrf failed",
            ),
            (
                ApiError::Conflict("email-taken"),
                409,
                "email-taken",
                "email taken",
            ),
        ] {
            let (s, v) = problem(err).await;
            assert_eq!(s.as_u16(), status);
            assert_eq!(v["type"], format!("urn:nm:problem:{code}"));
            assert_eq!(v["status"], status);
            assert_eq!(v["detail"], detail);
        }
    }

    #[tokio::test]
    async fn row_not_found_is_404_but_other_sqlx_errors_are_500() {
        let (s, v) = problem(sqlx::Error::RowNotFound.into()).await;
        assert_eq!(s, 404);
        assert_eq!(v["type"], "urn:nm:problem:not-found");
        let (s, _) = problem(sqlx::Error::PoolClosed.into()).await;
        assert_eq!(s, 500);
    }
}
