// SPDX-License-Identifier: AGPL-3.0-only

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult, FieldError};

pub mod sorted;

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;

/// `?limit=&cursor=` for list endpoints. Queries order by `id DESC`, filter on
/// `id < after()` and fetch `limit() + 1` rows for `Page::from_rows`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PageParams {
    /// Page size, 1 to 200 (default 50).
    pub limit: Option<u32>,
    /// Opaque cursor from the previous page's `next_cursor`.
    pub cursor: Option<String>,
}

impl PageParams {
    pub fn limit(&self) -> i64 {
        i64::from(self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT))
    }

    /// The decoded cursor; a malformed one is a 422.
    pub fn after(&self) -> ApiResult<Option<Uuid>> {
        let Some(cursor) = &self.cursor else {
            return Ok(None);
        };
        URL_SAFE_NO_PAD
            .decode(cursor)
            .ok()
            .and_then(|bytes| Uuid::from_slice(&bytes).ok())
            .map(Some)
            .ok_or_else(|| ApiError::Validation(vec![FieldError::new("cursor", "invalid cursor")]))
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Page<T> {
    pub data: Vec<T>,
    pub next_cursor: Option<String>,
}

impl<T> Page<T> {
    /// `rows` is the `limit + 1` fetch: an extra row means another page exists.
    pub fn from_rows(mut rows: Vec<T>, limit: i64, id: impl Fn(&T) -> Uuid) -> Page<T> {
        let limit = usize::try_from(limit).unwrap_or(0);
        let mut next_cursor = None;
        if rows.len() > limit {
            rows.truncate(limit);
            next_cursor = rows.last().map(|last| encode_cursor(id(last)));
        }
        Page {
            data: rows,
            next_cursor,
        }
    }
}

pub fn encode_cursor(id: Uuid) -> String {
    URL_SAFE_NO_PAD.encode(id.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ApiError;
    use uuid::Uuid;

    fn params(limit: Option<u32>, cursor: Option<&str>) -> PageParams {
        PageParams {
            limit,
            cursor: cursor.map(str::to_owned),
        }
    }

    #[test]
    fn limit_default_and_clamp() {
        assert_eq!(params(None, None).limit(), 50);
        assert_eq!(params(Some(0), None).limit(), 1);
        assert_eq!(params(Some(500), None).limit(), 200);
    }

    #[test]
    fn cursor_round_trip_and_garbage_is_validation() {
        let id = Uuid::now_v7();
        assert_eq!(
            params(None, Some(&encode_cursor(id))).after().unwrap(),
            Some(id)
        );
        assert_eq!(params(None, None).after().unwrap(), None);
        for bad in ["%%%", "", "AAAA"] {
            assert!(matches!(
                params(None, Some(bad)).after(),
                Err(ApiError::Validation(_))
            ));
        }
    }

    #[test]
    fn next_cursor_only_when_more_rows() {
        let rows: Vec<Uuid> = (0..51).map(|_| Uuid::now_v7()).collect();
        let page = Page::from_rows(rows.clone(), 50, |id| *id);
        assert_eq!(page.data.len(), 50);
        assert_eq!(page.next_cursor, Some(encode_cursor(rows[49])));

        let page = Page::from_rows(rows[..50].to_vec(), 50, |id| *id);
        assert_eq!(page.data.len(), 50);
        assert_eq!(page.next_cursor, None);
    }
}
