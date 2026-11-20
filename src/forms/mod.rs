// SPDX-License-Identifier: AGPL-3.0-only

//! Signup forms: what an operator says a hosted signup page collects, and which lists it
//! subscribes to. The page itself and the submission are the public side (Task 9); everything
//! here is the authenticated model behind it.
//!
//! Every value an operator puts here ends up on a page served to strangers, so this module is the
//! last place a typo is still cheap. It validates on the way in rather than on the way out.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, types::Json as Jsonb};
use url::Url;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    custom_fields::{self, Definition},
    error::{ApiResult, FieldError},
};

pub mod routes;

/// What one field of a form collects.
///
/// This is deliberately **not** `contacts::import::ContactField`, which the CSV importer uses.
/// That enum carries `Lists`, and a public form field targeting it would let whoever fills the
/// form in name the lists they join — the form's lists are the operator's decision, not the
/// submitter's. It also has no custom-field case, which spec §7.3 requires.
///
/// There is therefore no `Lists` variant here and there must never be one. Serde refuses an
/// unknown tag, so a hand-written body naming `"lists"` fails to deserialise and the request is
/// refused before any handler sees it; `tests/forms.rs` pins that through the API, because no
/// Rust-level test can exercise a variant that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum FormTarget {
    Email,
    FirstName,
    LastName,
    Timezone,
    /// A custom field, by its `custom_fields.key`. Checked against the workspace's live
    /// definitions at write time: a form collecting into a field that is not there collects into
    /// nowhere, and the operator would find out only once the submissions had happened.
    Custom(String),
}

/// One input on the page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FormField {
    pub target: FormTarget,
    pub required: bool,
    /// What the page prints beside the input.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Form {
    pub id: Uuid,
    pub workspace_id: Uuid,
    /// Unique in the workspace. It is what names the form in its public URL.
    pub slug: String,
    /// The operator's own name for it; never shown to a submitter.
    pub name: String,
    /// The lists one submission subscribes to, ascending. The Rust-side projection of the
    /// `form_lists` rows, so it is a set: the order a client sent is not kept.
    pub list_ids: Vec<Uuid>,
    pub fields: Vec<FormField>,
    /// Where a successful submission is to be sent. Absolute `http`/`https` only; see
    /// [`validate_redirect_url`].
    pub redirect_url: Option<String>,
    pub headline: String,
    pub button_label: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A `forms` row with its lists joined on. The column order is the struct's: `query_as!` maps by
/// position.
struct Row {
    id: Uuid,
    workspace_id: Uuid,
    slug: String,
    name: String,
    fields: Jsonb<Vec<FormField>>,
    redirect_url: Option<String>,
    headline: String,
    button_label: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    list_ids: Vec<Uuid>,
}

impl From<Row> for Form {
    fn from(r: Row) -> Form {
        Form {
            id: r.id,
            workspace_id: r.workspace_id,
            slug: r.slug,
            name: r.name,
            list_ids: r.list_ids,
            fields: r.fields.0,
            redirect_url: r.redirect_url,
            headline: r.headline,
            button_label: r.button_label,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// The form, or `404` if it is not in this workspace.
pub async fn get(db: &mut PgConnection, workspace_id: Uuid, id: Uuid) -> ApiResult<Form> {
    sqlx::query_as!(
        Row,
        r#"select f.id, f.workspace_id, f.slug, f.name,
                  f.fields as "fields: Jsonb<Vec<FormField>>",
                  f.redirect_url, f.headline, f.button_label, f.created_at, f.updated_at,
                  coalesce(array_agg(fl.list_id order by fl.list_id)
                           filter (where fl.list_id is not null), '{}') as "list_ids!: Vec<Uuid>"
           from forms f
           left join form_lists fl on fl.workspace_id = f.workspace_id and fl.form_id = f.id
           where f.workspace_id = $1 and f.id = $2
           group by f.id"#,
        workspace_id,
        id,
    )
    .fetch_one(db)
    .await
    .map(Form::from)
    .map_err(Into::into)
}

/// A form's public URL is `/f/{slug}`, so the slug is a URL path segment and nothing else:
/// `^[a-z0-9][a-z0-9-]{1,62}$`, written out. No trimming — a slug that needed trimming was not a
/// slug.
pub fn validate_slug(raw: &str) -> Result<String, FieldError> {
    let b = raw.as_bytes();
    if (2..=63).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    {
        return Ok(raw.to_owned());
    }
    Err(FieldError::new(
        "slug",
        "must be 2-63 characters: lowercase letters, digits and hyphens, not starting with a hyphen",
    ))
}

/// At most this many characters in a redirect URL. A URL longer than this is not an operator
/// typing one.
const MAX_URL: usize = 2000;

/// An absolute `http` or `https` URL with a host, and nothing else.
///
/// This is checked **here, on the way in**, not where Task 9 renders it. `public::page::esc`
/// escapes `& < > " '` — it is markup escaping, not a URL sanitiser, and `javascript:alert(1)`
/// goes through it untouched (recorded in Task 4b's review). The moment such a URL is stored,
/// every present and future reader of the column is one mistake away from executing it, so it is
/// never stored. Returns the parser's own serialisation: what is kept is what a URL parser
/// accepted, not a string that merely looked like one.
pub fn validate_redirect_url(raw: &str) -> Result<String, FieldError> {
    let err = || {
        FieldError::new(
            "redirect_url",
            "must be an absolute http or https URL, for example https://example.com/thanks",
        )
    };
    if raw.len() > MAX_URL {
        return Err(err());
    }
    let url = Url::parse(raw).map_err(|_| err())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(err());
    }
    if url.host_str().is_none_or(|h| h.is_empty()) {
        return Err(err());
    }
    Ok(url.to_string())
}

/// A form may name at most this many lists. One submission writes a membership, a consent row and
/// possibly a confirmation job per list, from an unauthenticated public endpoint: the bound is
/// what stops a form being an amplifier.
pub const MAX_LISTS: usize = 25;

/// And at most this many inputs on the page.
pub const MAX_FIELDS: usize = 25;

/// Deduplicated and sorted, so `list_ids` means a set and the stored order never depends on how a
/// client happened to spell the request. Refuses an empty list: a form that subscribes to nothing
/// collects addresses for nothing.
///
/// Whether each id belongs to this workspace is **not** checked here. `form_lists` carries a
/// composite foreign key onto `lists (workspace_id, id)`, so the database refuses a foreign id
/// with SQLSTATE `23503` and cannot tell "another workspace's list" from "no such list at all".
/// A lookup here would be able to tell them apart, and would therefore be able to leak the
/// difference. See `routes::unknown_list`.
pub fn validate_list_ids(raw: &[Uuid]) -> Result<Vec<Uuid>, FieldError> {
    if raw.is_empty() {
        return Err(FieldError::new("list_ids", "name at least one list"));
    }
    if raw.len() > MAX_LISTS {
        return Err(FieldError::new(
            "list_ids",
            "a form may name at most 25 lists",
        ));
    }
    let mut ids = raw.to_vec();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// The page's inputs, checked against the workspace's live custom field definitions.
///
/// Every form collects an address — a submission with no address subscribes nobody — and no
/// target appears twice, which would put two inputs on the page writing to one place.
///
/// An unknown custom field and **another workspace's** custom field are the same answer, by
/// construction: `defs` holds this workspace's definitions only, so neither key is in it and both
/// take the same branch. Do not add a global lookup to make the message friendlier.
pub fn validate_fields(
    raw: &[FormField],
    defs: &[Definition],
) -> Result<Vec<FormField>, FieldError> {
    let err = |m: &str| Err(FieldError::new("fields", m));
    if raw.len() > MAX_FIELDS {
        return err("a form may have at most 25 fields");
    }
    if raw.iter().filter(|f| f.target == FormTarget::Email).count() != 1 {
        return err("a form collects the email address exactly once");
    }
    let mut out: Vec<FormField> = Vec::with_capacity(raw.len());
    for field in raw {
        if out.iter().any(|seen| seen.target == field.target) {
            return err("each field targets something different");
        }
        if let FormTarget::Custom(key) = &field.target {
            // Bounds what the message below echoes, as well as refusing a key no custom field
            // could ever have.
            let key = custom_fields::validate_key(key)
                .map_err(|e| FieldError::new("fields", e.message))?;
            match defs.iter().find(|d| d.key == key) {
                None => return err(&format!("unknown custom field `{key}`")),
                Some(d) if d.archived_at.is_some() => {
                    return err(&format!("custom field `{key}` is archived"));
                }
                Some(_) => {}
            }
        }
        let label = field.label.trim();
        if !(1..=100).contains(&label.chars().count()) || label.chars().any(char::is_control) {
            return err("each label must be 1-100 characters with no control characters");
        }
        out.push(FormField {
            target: field.target.clone(),
            required: field.required,
            label: label.to_owned(),
        });
    }
    Ok(out)
}

/// Trimmed, 1..=`max` characters, no control characters. `lists::validate_name` would do this but
/// names its error `name`, and these errors have to name their own field.
pub(crate) fn validate_text(
    field: &'static str,
    raw: &str,
    max: usize,
) -> Result<String, FieldError> {
    let text = raw.trim();
    if (1..=max).contains(&text.chars().count()) && !text.chars().any(char::is_control) {
        return Ok(text.to_owned());
    }
    Err(FieldError::new(
        field,
        format!("must be 1-{max} characters with no control characters"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        for ok in ["ab", "0-9", "a-b-c", &"a".repeat(63)] {
            assert_eq!(validate_slug(ok).unwrap(), ok);
        }
        for bad in [
            "",
            "a",
            "-a",
            "A b",
            "a_b",
            "a/b",
            " ab",
            "ab ",
            "é!",
            &"a".repeat(64),
        ] {
            assert!(validate_slug(bad).is_err(), "{bad:?}");
        }
    }

    /// The scheme check is the whole point: markup escaping downstream does not touch a scheme.
    #[test]
    fn redirect_urls() {
        assert_eq!(
            validate_redirect_url("https://example.test/thanks").unwrap(),
            "https://example.test/thanks"
        );
        for bad in [
            "javascript:alert(1)",
            "JAVASCRIPT:alert(1)",
            "  javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox(1)",
            "file:///etc/passwd",
            "//example.test/thanks",
            "/thanks",
            "",
            "http://",
        ] {
            assert!(validate_redirect_url(bad).is_err(), "{bad:?}");
        }
        assert!(validate_redirect_url(&format!("https://e.test/{}", "a".repeat(MAX_URL))).is_err());
    }

    fn email() -> FormField {
        FormField {
            target: FormTarget::Email,
            required: true,
            label: "Email".to_owned(),
        }
    }

    #[test]
    fn fields_collect_an_address_once_and_nothing_twice() {
        assert_eq!(validate_fields(&[email()], &[]).unwrap(), vec![email()]);
        assert!(validate_fields(&[], &[]).is_err());
        assert!(validate_fields(&[email(), email()], &[]).is_err());
        let mut named = email();
        named.target = FormTarget::FirstName;
        assert!(validate_fields(&[named.clone()], &[]).is_err());
        assert!(validate_fields(&[email(), named.clone(), named], &[]).is_err());
    }

    #[test]
    fn a_custom_target_must_name_a_live_definition() {
        let def = |key: &str, archived: bool| Definition {
            id: Uuid::now_v7(),
            key: key.to_owned(),
            label: key.to_owned(),
            kind: custom_fields::FieldKind::Text,
            options: vec![],
            archived_at: archived.then(Utc::now),
        };
        let custom = |key: &str| FormField {
            target: FormTarget::Custom(key.to_owned()),
            required: false,
            label: "x".to_owned(),
        };
        let defs = [def("live", false), def("gone", true)];
        assert!(validate_fields(&[email(), custom("live")], &defs).is_ok());
        assert!(validate_fields(&[email(), custom("gone")], &defs).is_err());
        assert!(validate_fields(&[email(), custom("absent")], &defs).is_err());
        // The same key refused the same way whether the workspace has no definitions at all or
        // some other workspace has one: `defs` is this workspace's, and nothing else is consulted.
        assert_eq!(
            validate_fields(&[email(), custom("absent")], &defs).unwrap_err(),
            validate_fields(&[email(), custom("absent")], &[]).unwrap_err()
        );
    }

    /// `FormTarget` has no `Lists`, so this is the only level at which "a body naming lists is
    /// refused" can be stated in Rust at all: the wire format. The API-level proof is in
    /// `tests/forms.rs`.
    #[test]
    fn the_wire_format_has_no_lists_target() {
        assert_eq!(
            serde_json::to_value(FormTarget::Email).unwrap(),
            serde_json::json!({"kind": "email"})
        );
        let err = serde_json::from_value::<FormTarget>(serde_json::json!({"kind": "lists"}))
            .expect_err("`lists` is not a form target");
        assert!(err.to_string().contains("lists"), "{err}");
        assert!(
            serde_json::from_value::<FormTarget>(serde_json::json!({"kind": "first_name"})).is_ok()
        );
    }
}
