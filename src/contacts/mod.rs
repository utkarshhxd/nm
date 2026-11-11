// SPDX-License-Identifier: AGPL-3.0-only

//! Contacts: the people a workspace sends email to. A contact has no status column: an opt-out is
//! a suppression keyed by address (a later task), so it survives erasure and re-import.

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::{PgConnection, Postgres, QueryBuilder};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    custom_fields::{Definition, FieldKind, definitions_for},
    error::{ApiError, ApiResult, FieldError},
    lists::MembershipStatus,
    pagination::{
        Page,
        sorted::{ContactSort, Dir, SortedPageParams, page},
    },
    users::normalize_email,
    workspaces::validate_timezone,
};

pub mod routes;

#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow, ToSchema)]
pub struct Contact {
    pub id: Uuid,
    pub email: String,
    pub first_name: String,
    pub last_name: String,
    /// Custom field values by field key. Values of archived fields stay readable here.
    pub attributes: Value,
    pub timezone: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A contact to insert. Nothing here is trusted: `create` validates every field.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewContact {
    /// Normalised (trimmed, NFC, lowercased) before it is stored.
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    /// Custom field values by field key; see `validate_attributes` for what each kind accepts.
    pub attributes: Option<Value>,
    /// An IANA time zone such as `Europe/London`.
    pub timezone: Option<String>,
}

const MAX_TEXT: usize = 2000;

/// Trimmed, at most 100 characters, no control characters; empty is allowed.
pub(crate) fn validate_name(field: &'static str, raw: &str) -> Result<String, FieldError> {
    let name = raw.trim();
    if name.chars().count() <= 100 && !name.chars().any(char::is_control) {
        return Ok(name.to_owned());
    }
    Err(FieldError::new(
        field,
        "must be at most 100 characters with no control characters",
    ))
}

/// Checks `attrs` (a JSON object of values by field key) against the workspace's definitions and
/// returns it canonicalised: dates as `YYYY-MM-DD`, datetimes as UTC RFC 3339. Rejects an unknown
/// key, a value of the wrong type, and ANY value for an archived field. `definitions_for` returns
/// archived definitions so that existing values stay readable, so this is the one place that
/// stops an archived field taking fresh data. `null` is not a value: it is rejected here, and
/// `validate_attributes_patch` is where it means "remove".
pub fn validate_attributes(defs: &[Definition], attrs: &Value) -> Result<Value, Vec<FieldError>> {
    let Some(attrs) = attrs.as_object() else {
        return Err(vec![FieldError::new("attributes", "must be an object")]);
    };
    let mut out = Map::new();
    let mut errors = vec![];
    for (key, value) in attrs {
        let result = match defs.iter().find(|d| d.key == *key) {
            None => Err("no such custom field"),
            Some(d) if d.archived_at.is_some() => {
                Err("this custom field is archived and takes no new values")
            }
            Some(d) => canonical_value(d, value),
        };
        match result {
            Ok(v) => {
                out.insert(key.clone(), v);
            }
            Err(m) => errors.push(FieldError::new(format!("attributes.{key}"), m)),
        }
    }
    if errors.is_empty() {
        Ok(Value::Object(out))
    } else {
        Err(errors)
    }
}

/// `validate_attributes` for a merge patch (RFC 7386): returns the canonicalised values to set and
/// the keys to remove. A `null` removes the key. Removal is not new data, so it is allowed for an
/// archived field: without that, one person's value could only ever go by purging the field for
/// the whole workspace. A key that names no field at all is still an error.
pub fn validate_attributes_patch(
    defs: &[Definition],
    attrs: &Value,
) -> Result<(Value, Vec<String>), Vec<FieldError>> {
    let Some(map) = attrs.as_object() else {
        return Err(vec![FieldError::new("attributes", "must be an object")]);
    };
    let (nulls, sets): (Map<String, Value>, Map<String, Value>) =
        map.clone().into_iter().partition(|(_, v)| v.is_null());
    let mut errors: Vec<FieldError> = nulls
        .keys()
        .filter(|k| !defs.iter().any(|d| d.key == **k))
        .map(|k| FieldError::new(format!("attributes.{k}"), "no such custom field"))
        .collect();
    let sets = validate_attributes(defs, &Value::Object(sets));
    match (errors.is_empty(), sets) {
        (true, Ok(sets)) => Ok((sets, nulls.into_iter().map(|(k, _)| k).collect())),
        (_, sets) => {
            errors.extend(sets.err().into_iter().flatten());
            Err(errors)
        }
    }
}

fn canonical_value(d: &Definition, v: &Value) -> Result<Value, &'static str> {
    let in_options = |s: &str| d.options.iter().any(|o| o == s);
    match (d.kind, v) {
        (FieldKind::Text, Value::String(s))
            if s.chars().count() <= MAX_TEXT && !s.contains('\0') =>
        {
            Ok(v.clone())
        }
        (FieldKind::Number, Value::Number(_)) | (FieldKind::Boolean, Value::Bool(_)) => {
            Ok(v.clone())
        }
        (FieldKind::Date, Value::String(s)) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map(|d| Value::from(d.to_string()))
            .map_err(|_| "must be a date as YYYY-MM-DD"),
        (FieldKind::DateTime, Value::String(s)) => DateTime::parse_from_rfc3339(s)
            .map(|t| {
                let utc = t.with_timezone(&Utc);
                Value::from(utc.to_rfc3339_opts(SecondsFormat::AutoSi, true))
            })
            .map_err(|_| "must be an RFC 3339 date and time"),
        (FieldKind::Select, Value::String(s)) if in_options(s) => Ok(v.clone()),
        (FieldKind::MultiSelect, Value::Array(items)) => {
            let mut seen: Vec<&str> = vec![];
            for item in items {
                match item.as_str() {
                    Some(s) if in_options(s) && !seen.contains(&s) => seen.push(s),
                    _ => return Err("must be distinct values, each one of the field's options"),
                }
            }
            Ok(v.clone())
        }
        (FieldKind::Select, _) => Err("must be one of the field's options"),
        _ => Err("has the wrong type for this field"),
    }
}

/// Inserts a contact and returns its id. The caller passes the transaction (so the audit row
/// commits with it) and has already authorised the actor. `409 email-taken` if the normalised
/// address exists in this workspace; `422` per invalid field.
pub async fn create(db: &mut PgConnection, workspace_id: Uuid, new: NewContact) -> ApiResult<Uuid> {
    let email = normalize_email(&new.email);
    let first = validate_name("first_name", new.first_name.as_deref().unwrap_or(""));
    let last = validate_name("last_name", new.last_name.as_deref().unwrap_or(""));
    let tz = new.timezone.as_deref().map(validate_timezone).transpose();
    let defs = definitions_for(&mut *db, workspace_id).await?;
    let empty = Value::Object(Map::new());
    let attrs = validate_attributes(&defs, new.attributes.as_ref().unwrap_or(&empty));
    let (Ok(email), Ok(first), Ok(last), Ok(tz), Ok(attrs)) = (&email, &first, &last, &tz, &attrs)
    else {
        let mut errors: Vec<FieldError> = [email.err(), first.err(), last.err(), tz.err()]
            .into_iter()
            .flatten()
            .collect();
        errors.extend(attrs.err().into_iter().flatten());
        return Err(ApiError::Validation(errors));
    };
    let id = Uuid::now_v7();
    sqlx::query!(
        "insert into contacts (id, workspace_id, email, first_name, last_name, attributes, timezone) \
         values ($1, $2, $3, $4, $5, $6, $7)",
        id,
        workspace_id,
        email,
        first,
        last,
        attrs,
        tz.as_deref(),
    )
    .execute(&mut *db)
    .await
    // The only unique constraint a fresh `id` can hit is `(workspace_id, email)`.
    .map_err(|e| match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("email-taken"),
        _ => e.into(),
    })?;
    Ok(id)
}

/// The contact, or `404` if it is not in this workspace.
pub async fn get(db: &mut PgConnection, workspace_id: Uuid, id: Uuid) -> ApiResult<Contact> {
    sqlx::query_as!(
        Contact,
        "select id, email, first_name, last_name, attributes, timezone, created_at, updated_at \
         from contacts where workspace_id = $1 and id = $2",
        workspace_id,
        id,
    )
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

/// Which contacts a list shows. `tag_id` is defined now but not yet applied: tags do not exist
/// until a later task, which wires it into `list` (and embeds this type in a signed selection
/// token). So its being unread here is not an oversight. The serialised form is part of that
/// token's payload: renaming a field would invalidate tokens already issued.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactFilter {
    /// Case-insensitive substring of the address or name.
    pub q: Option<String>,
    /// Contacts with a membership row on this list, whatever its status: a `pending` or
    /// `unsubscribed` member is still a member here. A list of another workspace matches nothing.
    pub list_id: Option<Uuid>,
    /// Narrows `list_id` to the memberships with this status. Meaningless without `list_id`: the
    /// handler refuses it, and `list` ignores it.
    pub list_status: Option<MembershipStatus>,
    pub tag_id: Option<Uuid>,
}

/// Shorter terms cannot use the trigram index and would scan the whole table.
const MIN_SEARCH_CHARS: usize = 3;

impl ContactFilter {
    /// A search of 1 or 2 characters is a 422; a blank one is no search.
    pub fn with_search(q: Option<&str>) -> ApiResult<ContactFilter> {
        let q = q.map(str::trim).filter(|q| !q.is_empty());
        if q.is_some_and(|q| q.chars().count() < MIN_SEARCH_CHARS) {
            return Err(ApiError::Validation(vec![FieldError::new(
                "q",
                "must be at least 3 characters",
            )]));
        }
        Ok(ContactFilter {
            q: q.map(str::to_owned),
            ..ContactFilter::default()
        })
    }
}

/// One page of contacts in `params`' order, after `params`' cursor. The keyset predicate compares
/// `(sort_value, id)` as a row, in the column order of `contacts_keyset_email`, so the index
/// serves both the filter and the order. Today the `id` half of that pair is unobservable: `email`
/// is unique per workspace and `created` sorts by id alone. It is there for the first sort key
/// that is not unique, and keeping it is what keeps the column order Postgres needs to use
/// `contacts_keyset_email`. Do not remove it as dead weight.
pub async fn list(
    db: &mut PgConnection,
    workspace_id: Uuid,
    filter: &ContactFilter,
    params: &SortedPageParams,
) -> ApiResult<Page<Contact>> {
    let (sort, limit) = (params.sort(), params.limit());
    let (order, op) = match params.dir() {
        Dir::Asc => ("asc", ">"),
        Dir::Desc => ("desc", "<"),
    };
    let mut q = QueryBuilder::<Postgres>::new(
        "select id, email, first_name, last_name, attributes, timezone, created_at, updated_at \
         from contacts where workspace_id = ",
    );
    q.push_bind(workspace_id);
    if let Some(term) = &filter.q {
        // Must stay the index expression, spelled the same, for `contacts_search` to apply.
        q.push(" and (email || ' ' || first_name || ' ' || last_name) ilike '%' || ")
            .push_bind(escape_like(term))
            .push(" || '%'");
    }
    if let Some(list_id) = filter.list_id {
        // Correlated on `workspace_id` as well as the contact, so a list of another workspace
        // matches nothing.
        q.push(
            " and exists (select 1 from list_memberships m \
             where m.workspace_id = contacts.workspace_id and m.contact_id = contacts.id \
             and m.list_id = ",
        )
        .push_bind(list_id);
        if let Some(status) = filter.list_status {
            q.push(" and m.status = ").push_bind(status);
        }
        q.push(")");
    }
    if let Some((value, id)) = params.after()? {
        match sort {
            ContactSort::Email => {
                q.push(format!(" and (email, id) {op} ("))
                    .push_bind(value)
                    .push(", ")
                    .push_bind(id)
                    .push(")");
            }
            ContactSort::Created => {
                q.push(format!(" and id {op} ")).push_bind(id);
            }
        }
    }
    q.push(match sort {
        ContactSort::Email => format!(" order by email {order}, id {order}"),
        // Creation order only because contact ids are `Uuid::now_v7()`: a contact imported with a
        // v4 id would land at a random position.
        ContactSort::Created => format!(" order by id {order}"),
    });
    q.push(" limit ").push_bind(limit + 1);
    let rows = q.build_query_as::<Contact>().fetch_all(db).await?;
    Ok(page(rows, limit, |c| {
        let value = match sort {
            ContactSort::Email => c.email.clone(),
            ContactSort::Created => String::new(),
        };
        (value, c.id)
    }))
}

/// Makes `%`, `_` and `\` in a search term match themselves: unescaped, `___` would match every
/// row and no index could narrow it.
fn escape_like(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn search_needs_three_characters() {
        for short in ["a", "ab", " ab "] {
            assert!(ContactFilter::with_search(Some(short)).is_err(), "{short}");
        }
        assert_eq!(ContactFilter::with_search(Some("  ")).unwrap().q, None);
        assert_eq!(ContactFilter::with_search(None).unwrap().q, None);
        assert_eq!(
            ContactFilter::with_search(Some(" abc "))
                .unwrap()
                .q
                .as_deref(),
            Some("abc")
        );
        // Characters, not bytes: two multi-byte characters are still too short.
        assert!(ContactFilter::with_search(Some("éé")).is_err());
    }

    #[test]
    fn like_wildcards_are_escaped() {
        assert_eq!(escape_like(r"a%b_c\d"), r"a\%b\_c\\d");
    }

    #[test]
    fn filter_serialises_all_four_fields() {
        let v = serde_json::to_value(ContactFilter::default()).unwrap();
        assert_eq!(
            v,
            json!({"q": null, "list_id": null, "list_status": null, "tag_id": null})
        );
        let f: ContactFilter = serde_json::from_value(v).unwrap();
        assert_eq!(f, ContactFilter::default());
    }

    fn def(key: &str, kind: FieldKind, options: &[&str], archived: bool) -> Definition {
        Definition {
            id: Uuid::nil(),
            key: key.into(),
            label: key.into(),
            kind,
            options: options.iter().map(|s| (*s).to_owned()).collect(),
            archived_at: archived.then(Utc::now),
        }
    }

    #[test]
    fn archived_field_takes_no_value() {
        let defs = [def("plan", FieldKind::Text, &[], true)];
        let e = validate_attributes(&defs, &json!({"plan": "pro"})).unwrap_err();
        assert_eq!(e[0].field, "attributes.plan");
        assert!(validate_attributes(&defs, &json!({})).is_ok());
    }

    #[test]
    fn null_removes_even_for_an_archived_field() {
        let defs = [def("plan", FieldKind::Text, &[], true)];
        let (sets, removed) = validate_attributes_patch(&defs, &json!({"plan": null})).unwrap();
        assert_eq!((sets, removed), (json!({}), vec!["plan".to_owned()]));
        assert!(validate_attributes_patch(&defs, &json!({"plan": "x"})).is_err());
        assert!(validate_attributes_patch(&defs, &json!({"nope": null})).is_err());
    }

    #[test]
    fn values_are_canonicalised() {
        let defs = [
            def("d", FieldKind::Date, &[], false),
            def("t", FieldKind::DateTime, &[], false),
        ];
        let v = validate_attributes(
            &defs,
            &json!({"d": "2024-1-5", "t": "2024-03-01T12:00:00+02:00"}),
        )
        .unwrap();
        assert_eq!(v, json!({"d": "2024-01-05", "t": "2024-03-01T10:00:00Z"}));
    }
}
