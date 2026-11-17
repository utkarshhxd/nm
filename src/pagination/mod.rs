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

    /// The decoded cursor for a list keyed by a row number rather than an id
    /// ([`encode_row_cursor`]); a malformed one is a 422. An id cursor is 16 bytes and this one
    /// is 8, so one minted for the other list kind is refused rather than silently restarting.
    pub fn after_row(&self) -> ApiResult<Option<i64>> {
        let Some(cursor) = &self.cursor else {
            return Ok(None);
        };
        URL_SAFE_NO_PAD
            .decode(cursor)
            .ok()
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(|bytes| Some(i64::from_be_bytes(bytes)))
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
    pub fn from_rows(rows: Vec<T>, limit: i64, id: impl Fn(&T) -> Uuid) -> Page<T> {
        Page::paged(rows, limit, |row| encode_cursor(id(row)))
    }

    /// [`Page::from_rows`] for a list keyed by a row number rather than an id: the errors of one
    /// import, which are ordered by the CSV row they came from.
    pub fn from_numbered_rows(rows: Vec<T>, limit: i64, row_number: impl Fn(&T) -> i64) -> Page<T> {
        Page::paged(rows, limit, |row| encode_row_cursor(row_number(row)))
    }

    fn paged(mut rows: Vec<T>, limit: i64, cursor: impl Fn(&T) -> String) -> Page<T> {
        let limit = usize::try_from(limit).unwrap_or(0);
        let mut next_cursor = None;
        if rows.len() > limit {
            rows.truncate(limit);
            next_cursor = rows.last().map(&cursor);
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

/// The cursor for a row-numbered list: the row number's eight big-endian bytes.
pub fn encode_row_cursor(row_number: i64) -> String {
    URL_SAFE_NO_PAD.encode(row_number.to_be_bytes())
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
    fn a_row_cursor_round_trips_and_the_two_cursor_kinds_do_not_cross() {
        let cursor = encode_row_cursor(9_000_000_000);
        assert_eq!(
            params(None, Some(&cursor)).after_row().unwrap(),
            Some(9_000_000_000)
        );
        assert_eq!(params(None, None).after_row().unwrap(), None);
        // An id cursor is 16 bytes, a row cursor 8: each is garbage to the other list's handler,
        // which is a 422 rather than a silent restart at the top of the list.
        let id = encode_cursor(Uuid::now_v7());
        for bad in ["%%%", "", "AAAA", &id] {
            assert!(matches!(
                params(None, Some(bad)).after_row(),
                Err(ApiError::Validation(_))
            ));
        }
        assert!(matches!(
            params(None, Some(&cursor)).after(),
            Err(ApiError::Validation(_))
        ));
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
