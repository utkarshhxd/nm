// SPDX-License-Identifier: AGPL-3.0-only

//! Keyset pagination for a list the client can sort. M0's `PageParams` (a bare id cursor, always
//! `id desc`) stays as it is for every other endpoint; this is the composite form: the cursor
//! carries the last row's sort value and id, and the next page starts strictly after that pair.
//! Pages never skip rows by counting them: that cannot meet the latency target at ten million rows.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{DEFAULT_LIMIT, MAX_LIMIT, Page};
use crate::error::{ApiError, ApiResult, FieldError};

/// What a contact list is ordered by. Only keys with an index (`contacts_keyset_created`,
/// `contacts_keyset_email`) are offered: a sort by name would sort the whole workspace per page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContactSort {
    /// Creation order, by the time-ordered contact id.
    Created,
    Email,
}

impl ContactSort {
    pub fn default_dir(self) -> Dir {
        match self {
            ContactSort::Created => Dir::Desc,
            ContactSort::Email => Dir::Asc,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    Asc,
    Desc,
}

/// `?limit=&cursor=&sort=&dir=&q=` for the contact list.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SortedPageParams {
    /// Page size, 1 to 200 (default 50).
    pub limit: Option<u32>,
    /// Opaque cursor from the previous page's `next_cursor`. Only valid with the same `sort`.
    pub cursor: Option<String>,
    /// Default `created`.
    pub sort: Option<ContactSort>,
    /// Default `desc` for `created`, `asc` for `email`.
    pub dir: Option<Dir>,
    /// Case-insensitive substring of the address or name, at least 3 characters.
    pub q: Option<String>,
}

impl SortedPageParams {
    pub fn limit(&self) -> i64 {
        i64::from(self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT))
    }

    pub fn sort(&self) -> ContactSort {
        self.sort.unwrap_or(ContactSort::Created)
    }

    pub fn dir(&self) -> Dir {
        self.dir.unwrap_or_else(|| self.sort().default_dir())
    }

    /// The decoded `(sort_value, id)`; a malformed cursor is a 422. `created` sorts by id alone,
    /// so its value is empty and an address never is: a cursor from the other sort fails here
    /// instead of silently restarting the list.
    pub fn after(&self) -> ApiResult<Option<(String, Uuid)>> {
        let Some(cursor) = &self.cursor else {
            return Ok(None);
        };
        let invalid = || ApiError::Validation(vec![FieldError::new("cursor", "invalid cursor")]);
        let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| invalid())?;
        let (id, value) = bytes.split_first_chunk::<16>().ok_or_else(invalid)?;
        let value = String::from_utf8(value.to_vec()).map_err(|_| invalid())?;
        if value.is_empty() != (self.sort() == ContactSort::Created) {
            return Err(invalid());
        }
        Ok(Some((value, Uuid::from_bytes(*id))))
    }
}

/// The cursor for the row after which the next page starts: its id's 16 bytes, then its sort
/// value as UTF-8 (empty for `created`).
pub fn encode_cursor(sort_value: &str, id: Uuid) -> String {
    let mut bytes = id.as_bytes().to_vec();
    bytes.extend_from_slice(sort_value.as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `rows` is the `limit + 1` fetch: an extra row means another page exists. `key` gives a row's
/// `(sort_value, id)`.
pub fn page<T>(mut rows: Vec<T>, limit: i64, key: impl Fn(&T) -> (String, Uuid)) -> Page<T> {
    let limit = usize::try_from(limit).unwrap_or(0);
    let mut next_cursor = None;
    if rows.len() > limit {
        rows.truncate(limit);
        next_cursor = rows.last().map(|last| {
            let (value, id) = key(last);
            encode_cursor(&value, id)
        });
    }
    Page {
        data: rows,
        next_cursor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;

    fn params(query: &str) -> SortedPageParams {
        let uri = format!("/?{query}").parse().unwrap();
        Query::<SortedPageParams>::try_from_uri(&uri).unwrap().0
    }

    #[test]
    fn cursor_round_trips_value_and_id() {
        let id = Uuid::now_v7();
        let cursor = encode_cursor("zoe@example.com", id);
        let p = params(&format!("sort=email&cursor={cursor}"));
        assert_eq!(p.after().unwrap(), Some(("zoe@example.com".to_owned(), id)));
        let cursor = encode_cursor("", id);
        let p = params(&format!("cursor={cursor}"));
        assert_eq!(p.after().unwrap(), Some((String::new(), id)));
        assert_eq!(params("").after().unwrap(), None);
    }

    #[test]
    fn garbage_cursor_is_validation_error() {
        let id = Uuid::now_v7();
        // A cursor minted under the other sort is garbage too.
        let wrong_sort = [
            format!("sort=email&cursor={}", encode_cursor("", id)),
            format!("cursor={}", encode_cursor("a@b.co", id)),
        ];
        let bad = ["%25%25%25", "", "AAAA", "_w"].map(|c| format!("cursor={c}"));
        for q in bad.iter().chain(&wrong_sort) {
            match params(q).after() {
                Err(ApiError::Validation(e)) => assert_eq!(e[0].field, "cursor", "{q}"),
                other => panic!("{q}: {other:?}"),
            }
        }
    }

    #[test]
    fn limit_defaults_to_50_and_clamps_to_200() {
        assert_eq!(params("").limit(), 50);
        assert_eq!(params("limit=0").limit(), 1);
        assert_eq!(params("limit=500").limit(), 200);
    }

    #[test]
    fn unknown_sort_key_is_validation_error() {
        let uri = "/?sort=name".parse().unwrap();
        let rejection = Query::<SortedPageParams>::try_from_uri(&uri).unwrap_err();
        assert!(matches!(ApiError::from(rejection), ApiError::Validation(_)));
    }

    #[test]
    fn direction_defaults_follow_the_sort() {
        assert_eq!(params("").dir(), Dir::Desc);
        assert_eq!(params("sort=email").dir(), Dir::Asc);
        assert_eq!(params("sort=email&dir=desc").dir(), Dir::Desc);
    }

    #[test]
    fn next_cursor_only_when_more_rows() {
        let rows: Vec<(String, Uuid)> = (0..4).map(|i| (i.to_string(), Uuid::now_v7())).collect();
        let p = page(rows.clone(), 3, |r| r.clone());
        assert_eq!(p.data.len(), 3);
        assert_eq!(p.next_cursor, Some(encode_cursor("2", rows[2].1)));
        assert_eq!(page(rows[..3].to_vec(), 3, |r| r.clone()).next_cursor, None);
    }
}
