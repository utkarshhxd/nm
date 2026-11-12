// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Json, Router, extract::State, http::StatusCode};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{
    Contact, ContactFilter, NewContact, create, get, list,
    selection::{self, Counts, Selection},
    validate_attributes_patch, validate_name,
};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    custom_fields::definitions_for,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    lists::{self, MembershipStatus},
    pagination::{Page, sorted::SortedPageParams},
    suppressions::{Block, blocking_suppression, erase_address},
    tags,
    workspaces::{roles::Role, validate_timezone},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts",
            axum::routing::get(list_contacts).post(create_contact),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/{id}",
            axum::routing::get(get_contact)
                .patch(update_contact)
                .delete(delete_contact),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/{id}/eligibility",
            axum::routing::get(contact_eligibility),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/{id}/erase",
            axum::routing::post(erase_contact),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/selection",
            axum::routing::post(create_selection),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/bulk/tag",
            axum::routing::post(bulk_tag_contacts),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/bulk/untag",
            axum::routing::post(bulk_untag_contacts),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/bulk/subscribe",
            axum::routing::post(bulk_subscribe_contacts),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/bulk/unsubscribe",
            axum::routing::post(bulk_unsubscribe_contacts),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/bulk/delete",
            axum::routing::post(bulk_delete_contacts),
        )
}

/// A JSON Merge Patch (RFC 7386) of a contact: a field left out is left alone, and an explicit
/// `null` removes it. The address cannot change, so `email` is an unknown field here and is
/// rejected, not ignored.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateContact {
    /// `null` clears it to an empty string.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub first_name: Option<Option<String>>,
    /// `null` clears it to an empty string.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub last_name: Option<Option<String>>,
    /// Merged into the stored attributes key by key: a key left out keeps its value. A key set to
    /// `null` is removed, also for an archived field; any other value for an archived field is
    /// refused. `attributes` itself cannot be `null`.
    #[serde(default, deserialize_with = "present")]
    pub attributes: Option<Value>,
    /// An IANA time zone such as `Europe/London`. `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub timezone: Option<Option<String>>,
}

/// With `#[serde(default)]`, tells a key that is absent (`None`) from one that is present, even as
/// `null` (`Some(..)`). A plain `Option<T>` maps both to `None`, so a `null` would be silently
/// ignored.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/contacts",
    operation_id = "list_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), SortedPageParams),
    responses(
        (status = 200, description = "Newest first unless `sort` says otherwise", body = Page<Contact>),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a malformed cursor, an unknown `sort`, a `q` under 3 characters, or a `list_status` without a `list_id`"),
    ),
)]
pub async fn list_contacts(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiQuery(params): ApiQuery<SortedPageParams>,
) -> ApiResult<Json<Page<Contact>>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    if params.list_status.is_some() && params.list_id.is_none() {
        return Err(ApiError::Validation(vec![FieldError::new(
            "list_status",
            "needs a list_id to narrow",
        )]));
    }
    let filter = ContactFilter {
        list_id: params.list_id,
        list_status: params.list_status,
        tag_id: params.tag_id,
        ..ContactFilter::with_search(params.q.as_deref())?
    };
    Ok(Json(list(&mut conn, workspace_id, &filter, &params).await?))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}",
    operation_id = "get_contact",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
    ),
    responses(
        (status = 200, body = Contact),
        (status = 404, description = "not a member, or no such contact in this workspace"),
    ),
)]
pub async fn get_contact(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<Contact>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    Ok(Json(get(&mut conn, workspace_id, id).await?))
}

/// Whether a contact may be mailed, and if a standing block stops it, which one.
#[derive(Serialize, ToSchema)]
pub struct Eligibility {
    /// `false` exactly when `blocked_by` is set.
    pub mailable: bool,
    /// The block that stops this address, whether it is this workspace's or the whole instance's.
    /// It says nothing about any other address. A list membership that is `pending` or
    /// `unsubscribed` is a separate reason a contact is not mailed on that list, and is not here.
    pub blocked_by: Option<Block>,
}

/// Computed on the server because the answer can depend on an instance-wide block that a
/// workspace's own suppression list does not contain and its admins cannot read. It discloses only
/// the verdict for this one contact.
#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}/eligibility",
    operation_id = "contact_eligibility",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
    ),
    responses(
        (status = 200, body = Eligibility),
        (status = 404, description = "not a member, or no such contact in this workspace"),
    ),
)]
pub async fn contact_eligibility(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<Eligibility>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    let contact = get(&mut conn, workspace_id, id).await?;
    let blocked_by =
        blocking_suppression(&mut *conn, &state.secrets, workspace_id, &contact.email).await?;
    Ok(Json(Eligibility {
        mailable: blocked_by.is_none(),
        blocked_by,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts",
    operation_id = "create_contact",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewContact,
    responses(
        (status = 201, body = Contact),
        (status = 403, description = "insufficient-role: creating a contact takes editor"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 409, description = "email-taken; or address-erased: this address was erased and can never be added again"),
        (status = 422, description = "invalid address, name, time zone or attribute"),
    ),
)]
pub async fn create_contact(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewContact>,
) -> ApiResult<(StatusCode, Json<Contact>)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let id = create(&mut tx, &state.secrets, workspace_id, input).await?;
    // The address is personal data an erasure must be able to remove, and the audit log is not
    // erased: record the id only.
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::CONTACT_CREATED,
            target_type: Some("contact"),
            target_id: Some(id.to_string()),
            metadata: json!({}),
        },
    )
    .await?;
    let contact = get(&mut tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(contact)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}",
    operation_id = "update_contact",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
    ),
    request_body = UpdateContact,
    responses(
        (status = 200, body = Contact),
        (status = 403, description = "insufficient-role: editing a contact takes editor"),
        (status = 404, description = "not a member, or no such contact in this workspace"),
        (status = 422, description = "invalid name, time zone or attribute, or an unknown field such as `email`"),
    ),
)]
pub async fn update_contact(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<UpdateContact>,
) -> ApiResult<Json<Contact>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;

    // `Some(None)` is an explicit `null`; `None` is a key that was not sent.
    let name = |field: &'static str, v: &Option<Option<String>>| {
        v.as_ref()
            .map(|n| validate_name(field, n.as_deref().unwrap_or_default()))
            .transpose()
    };
    let first = name("first_name", &input.first_name);
    let last = name("last_name", &input.last_name);
    let tz = match &input.timezone {
        Some(Some(t)) => validate_timezone(t).map(Some),
        _ => Ok(None),
    };
    let attrs = match &input.attributes {
        Some(a) => {
            validate_attributes_patch(&definitions_for(&mut *tx, workspace_id).await?, a).map(Some)
        }
        None => Ok(None),
    };
    let (Ok(first), Ok(last), Ok(tz), Ok(attrs)) = (&first, &last, &tz, &attrs) else {
        let mut errors: Vec<FieldError> = [first.err(), last.err(), tz.err()]
            .into_iter()
            .flatten()
            .collect();
        errors.extend(attrs.err().into_iter().flatten());
        return Err(ApiError::Validation(errors));
    };
    let (sets, removed) = match attrs {
        Some((sets, removed)) => (Some(sets), removed.as_slice()),
        None => (None, [].as_slice()),
    };

    // A name left out is kept; `null` clears it to "". `timezone` needs its own flag, because
    // `null` there means NULL and so cannot also mean "unchanged". The attributes first lose the
    // removed keys, then merge the rest with `||`, which touches only the keys it names.
    let updated = sqlx::query!(
        "update contacts set first_name = coalesce($3, first_name), \
         last_name = coalesce($4, last_name), \
         timezone = case when $5 then $6 else timezone end, \
         attributes = (attributes - $7::text[]) || coalesce($8, '{}'::jsonb), updated_at = now() \
         where workspace_id = $1 and id = $2",
        workspace_id,
        id,
        first.as_deref(),
        last.as_deref(),
        input.timezone.is_some(),
        tz.as_deref(),
        removed,
        sets,
    )
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    // Which fields changed or were cleared, never their values: a name or an attribute can be
    // personal data, and the audit log outlives an erasure.
    let mut fields: Vec<String> = [
        ("first_name", input.first_name.is_some()),
        ("last_name", input.last_name.is_some()),
        ("timezone", input.timezone.is_some()),
    ]
    .into_iter()
    .filter(|(_, sent)| *sent)
    .map(|(name, _)| name.to_owned())
    .collect();
    let touched = sets
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|m| m.keys())
        .chain(removed);
    fields.extend(touched.map(|k| format!("attributes.{k}")));
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::CONTACT_UPDATED,
            target_type: Some("contact"),
            target_id: Some(id.to_string()),
            metadata: json!({ "fields": fields }),
        },
    )
    .await?;
    let contact = get(&mut tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok(Json(contact))
}

/// A hard delete. It writes no suppression: the address may be added again. Erasing a person for
/// GDPR is a different, admin-only action.
#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}",
    operation_id = "delete_contact",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
    ),
    responses(
        (status = 204, description = "Deleted. Nothing is suppressed"),
        (status = 403, description = "insufficient-role: deleting a contact takes editor"),
        (status = 404, description = "not a member, or no such contact in this workspace"),
    ),
)]
pub async fn delete_contact(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let deleted = sqlx::query!(
        "delete from contacts where workspace_id = $1 and id = $2",
        workspace_id,
        id,
    )
    .execute(&mut *tx)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::CONTACT_DELETED,
            target_type: Some("contact"),
            target_id: Some(id.to_string()),
            metadata: json!({}),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// GDPR erasure: deletes the contact, its list memberships and its tags, and leaves a block that
/// keeps the address from being mailed or added again. Admin only, and not reversible: no route
/// lifts it. A mistake is fixed with `DELETE`, which blocks nothing.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}/erase",
    operation_id = "erase_contact",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
    ),
    responses(
        (status = 204, description = "Erased. The address can never be added again"),
        (status = 403, description = "insufficient-role: erasing a contact takes admin"),
        (status = 404, description = "not a member, or no such contact in this workspace"),
    ),
)]
pub async fn erase_contact(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;
    // The address is read here only to hash it; the tombstone takes the lock before the delete,
    // so a concurrent create of the same address either sees the tombstone or is deleted by us.
    let email = get(&mut tx, workspace_id, id).await?.email;
    erase_address(&mut tx, &state.secrets, workspace_id, &email).await?;
    let deleted = sqlx::query!(
        "delete from contacts where workspace_id = $1 and id = $2",
        workspace_id,
        id,
    )
    .execute(&mut *tx)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    // The audit log outlives the contact and admins read it: the id and the action, never the
    // address, or the erasure would not have erased anything.
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::CONTACT_ERASED,
            target_type: Some("contact"),
            target_id: Some(id.to_string()),
            metadata: json!({}),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The signed token for a selection; see `selection`.
#[derive(Serialize, ToSchema)]
pub struct SelectionToken {
    /// Valid for one hour. Pass it to a `bulk/*` route.
    pub selection_token: String,
    /// How many contacts the filter matches, as of the token's `as_of`. A live count over the
    /// whole match: deliberately exempt from the 150 ms p95 target, like
    /// `GET /lists/{id}/mailable-count`, because counting ten million rows is slow. The UI asks
    /// for it when the user asks to select everything, and shows a skeleton until it lands. It is
    /// never kept as a counter on a table: hand-maintained counters drift.
    pub selected: u64,
}

/// What a bulk route acts on: exactly one of a filter selection or an explicit list of contacts.
///
/// The three bulk bodies spell their fields out and deny unknown ones, on purpose: `excluded_ids`
/// defaults to empty, so a misspelt `excluded_id` that was quietly ignored would run the action
/// on exactly the contacts the user meant to spare. `serde(flatten)` cannot be combined with
/// `deny_unknown_fields`, so do not factor these back into one flattened struct, and give any
/// new optional field here a reason to exist.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BulkTarget {
    /// From `POST /contacts/selection`: every contact its filter matched, as of then, less
    /// `excluded_ids`. Send this or `ids`, not both and not neither.
    pub selection_token: Option<String>,
    /// These contacts and no others, e.g. the ticked rows of a table: at most 10 000. Ids that are
    /// not in this workspace select nothing. Cannot be combined with `excluded_ids`.
    pub ids: Option<Vec<Uuid>>,
    /// Contacts to leave out of a `selection_token`. At most 10 000.
    #[serde(default)]
    pub excluded_ids: Vec<Uuid>,
}

/// `BulkTarget` and a tag.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BulkTag {
    /// See `BulkTarget`.
    pub selection_token: Option<String>,
    /// See `BulkTarget`.
    pub ids: Option<Vec<Uuid>>,
    /// See `BulkTarget`.
    #[serde(default)]
    pub excluded_ids: Vec<Uuid>,
    pub tag_id: Uuid,
}

impl BulkTag {
    fn split(self) -> (BulkTarget, Uuid) {
        let target = BulkTarget {
            selection_token: self.selection_token,
            ids: self.ids,
            excluded_ids: self.excluded_ids,
        };
        (target, self.tag_id)
    }
}

/// `BulkTarget` and a list.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BulkList {
    /// See `BulkTarget`.
    pub selection_token: Option<String>,
    /// See `BulkTarget`.
    pub ids: Option<Vec<Uuid>>,
    /// See `BulkTarget`.
    #[serde(default)]
    pub excluded_ids: Vec<Uuid>,
    pub list_id: Uuid,
}

impl BulkList {
    fn split(self) -> (BulkTarget, Uuid) {
        let target = BulkTarget {
            selection_token: self.selection_token,
            ids: self.ids,
            excluded_ids: self.excluded_ids,
        };
        (target, self.list_id)
    }
}

/// What a bulk action did, as two numbers taken from the same statement. `affected` is how many
/// contacts it changed; `selected` is how many the selection matched (as of its `as_of`, less the
/// exclusions). `affected` is lower whenever some were already in the wanted state, so the UI can
/// say "N of M".
#[derive(Serialize, ToSchema)]
pub struct BulkResult {
    pub affected: u64,
    pub selected: u64,
    /// How many the selection stood for when it was made. `selected` above it means the filter
    /// matched contacts that did not match when the person looked — they existed then, and were
    /// tagged, subscribed or renamed into the view afterwards. `bulk/delete` refuses outright in
    /// that case; the reversible actions proceed and the UI says so.
    pub issued: u64,
    /// Only for `subscribe`: the status the new memberships were written with. `pending` means
    /// the contacts are on a double-opt-in list and not mailable until they confirm, and nothing
    /// in this milestone confirms them: `affected` is not a count of people who can be mailed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<MembershipStatus>,
}

/// Freezes "the contacts this filter matches" at this moment. The token holds the filter and a
/// time bound, not the contacts: contacts created afterwards are never part of it.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/selection",
    operation_id = "create_contact_selection",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = ContactFilter,
    responses(
        (status = 200, body = SelectionToken),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a `q` under 3 characters, or a `list_status` without a `list_id`"),
    ),
)]
pub async fn create_selection(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(filter): ApiJson<ContactFilter>,
) -> ApiResult<Json<SelectionToken>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Editor).await?;
    if filter.list_status.is_some() && filter.list_id.is_none() {
        return Err(ApiError::Validation(vec![FieldError::new(
            "list_status",
            "needs a list_id to narrow",
        )]));
    }
    let filter = ContactFilter {
        list_id: filter.list_id,
        list_status: filter.list_status,
        tag_id: filter.tag_id,
        ..ContactFilter::with_search(filter.q.as_deref())?
    };
    let mut selection = Selection::now(workspace_id, filter);
    let selected = selection::count(&mut conn, &selection).await?;
    // Signed into the token: this is the number the person is about to be shown, and the only
    // thing a bulk action can later compare its own resolved count against.
    selection.issued = selected;
    Ok(Json(SelectionToken {
        selection_token: selection::issue(&state.secrets, &selection),
        selected,
    }))
}

/// Opens the transaction, authorises, and resolves the token: in that order, so a non-member
/// learns nothing about whether a token was good. A token for another workspace is a `404`.
async fn begin_bulk(
    state: &AppState,
    principal: &Principal,
    workspace_id: Uuid,
    target: &BulkTarget,
) -> ApiResult<(Transaction<'static, Postgres>, Selection)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, principal, workspace_id, Role::Editor).await?;
    selection::check_excluded(&target.excluded_ids)?;
    let selection = match (&target.selection_token, &target.ids) {
        (Some(token), None) => selection::parse(&state.secrets, token)?,
        (None, Some(ids)) => {
            selection::check_ids(ids)?;
            if !target.excluded_ids.is_empty() {
                return Err(ApiError::Validation(vec![FieldError::new(
                    "excluded_ids",
                    "only applies to a selection_token",
                )]));
            }
            Selection::of_ids(workspace_id, ids.clone())
        }
        _ => {
            return Err(ApiError::Validation(vec![FieldError::new(
                "selection_token",
                "send exactly one of selection_token and ids",
            )]));
        }
    };
    if selection.workspace_id != workspace_id {
        return Err(ApiError::NotFound);
    }
    Ok((tx, selection))
}

/// The one audit row for a bulk action: what was asked (the filter and its time bound) and what
/// it did (the count), never the contacts. Per-contact rows would outgrow the contacts table.
async fn finish_bulk(
    mut tx: Transaction<'static, Postgres>,
    meta: &RequestMeta,
    principal: &Principal,
    action: &'static str,
    (selection, target): (&Selection, &BulkTarget),
    extra: Value,
    (counts, status): (Counts, Option<MembershipStatus>),
) -> ApiResult<Json<BulkResult>> {
    // The search text can be an address or a name, and the audit log outlives an erasure: record
    // that a search was part of the selection, never what it said. Not a hash either: a hash of
    // an address falls to a dictionary.
    let mut filter = json!(selection.filter);
    // An explicit id list is recorded as such and as a count, never as the ids: one row, not one
    // per contact, and ids are personal data the log would outlive.
    let by = if selection.ids.is_some() {
        "ids"
    } else {
        "filter"
    };
    if !filter["q"].is_null() {
        filter["q"] = json!("<redacted>");
    }
    audit::record(
        &mut *tx,
        meta,
        AuditEvent {
            workspace_id: Some(selection.workspace_id),
            actor: principal.actor(),
            action,
            target_type: None,
            target_id: None,
            metadata: json!({
                "by": by,
                "filter": filter,
                "as_of": selection.as_of,
                "ids": selection.ids.as_ref().map(Vec::len),
                // A count, not the ids, by design (one row, not one per contact): replayed calls with
                // different exclusions cannot be told apart afterwards. Do not turn this into a list.
                "excluded": target.excluded_ids.len(),
                "issued": selection.issued,
                "selected": counts.selected,
                "affected": counts.affected,
                "with": extra,
            }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(BulkResult {
        affected: counts.affected,
        selected: counts.selected,
        issued: selection.issued,
        status,
    }))
}

/// Tags every contact in the selection.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/bulk/tag",
    operation_id = "bulk_tag_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = BulkTag,
    responses(
        (status = 200, description = "How many were newly tagged", body = BulkResult),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 404, description = "not a member, no such tag, or a token for another workspace"),
        (status = 422, description = "selection-expired, selection-invalid, too many excluded_ids or ids, or not exactly one of selection_token and ids"),
    ),
)]
pub async fn bulk_tag_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkTag>,
) -> ApiResult<Json<BulkResult>> {
    let (target, tag_id) = input.split();
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &target).await?;
    // Checked up front: with no contact to insert for, the foreign key would never object.
    tags::get(&mut tx, workspace_id, tag_id).await?;
    let n = selection::tag(&mut tx, &sel, &target.excluded_ids, tag_id).await?;
    let extra = json!({ "tag_id": tag_id });
    let action = actions::CONTACTS_BULK_TAGGED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &target),
        extra,
        (n, None),
    )
    .await
}

/// Takes a tag off every contact in the selection.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/bulk/untag",
    operation_id = "bulk_untag_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = BulkTag,
    responses(
        (status = 200, description = "How many had the tag", body = BulkResult),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 404, description = "not a member, no such tag, or a token for another workspace"),
        (status = 422, description = "selection-expired, selection-invalid, too many excluded_ids or ids, or not exactly one of selection_token and ids"),
    ),
)]
pub async fn bulk_untag_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkTag>,
) -> ApiResult<Json<BulkResult>> {
    let (target, tag_id) = input.split();
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &target).await?;
    tags::get(&mut tx, workspace_id, tag_id).await?;
    let n = selection::untag(&mut tx, &sel, &target.excluded_ids, tag_id).await?;
    let extra = json!({ "tag_id": tag_id });
    let action = actions::CONTACTS_BULK_UNTAGGED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &target),
        extra,
        (n, None),
    )
    .await
}

/// Adds every contact in the selection who is not on a list yet: `pending` on a double-opt-in
/// list, `subscribed` otherwise. An existing membership is left exactly as it is: `pending` is not
/// promoted, and `unsubscribed` is never flipped back. A suppression is not lifted either.
///
/// This deliberately differs from `POST /lists/{id}/members`, which lets an unsubscribed contact
/// back in. One admin re-adding one named person is a considered act they answer for; a bulk
/// action over thousands involves no per-person judgement, and reviving opt-outs en masse is what
/// gets a sender blocklisted. Do not "fix" the asymmetry.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/bulk/subscribe",
    operation_id = "bulk_subscribe_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = BulkList,
    responses(
        (status = 200, description = "How many memberships were added; contacts already on the list, whatever their status, are not counted", body = BulkResult),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 404, description = "not a member, no such list, or a token for another workspace"),
        (status = 422, description = "selection-expired, selection-invalid, too many excluded_ids or ids, or not exactly one of selection_token and ids"),
    ),
)]
pub async fn bulk_subscribe_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkList>,
) -> ApiResult<Json<BulkResult>> {
    let (target, list_id) = input.split();
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &target).await?;
    let list = lists::get(&mut tx, workspace_id, list_id).await?;
    let status = if list.double_optin {
        MembershipStatus::Pending
    } else {
        MembershipStatus::Subscribed
    };
    let excluded = &target.excluded_ids;
    let n = selection::subscribe(&mut tx, &sel, excluded, list_id, status).await?;
    let extra = json!({ "list_id": list_id });
    let action = actions::CONTACTS_BULK_SUBSCRIBED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &target),
        extra,
        (n, Some(status)),
    )
    .await
}

/// Unsubscribes every member of the selection from a list.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/bulk/unsubscribe",
    operation_id = "bulk_unsubscribe_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = BulkList,
    responses(
        (status = 200, description = "How many were unsubscribed now; those already unsubscribed or not on the list are not counted", body = BulkResult),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 404, description = "not a member, no such list, or a token for another workspace"),
        (status = 422, description = "selection-expired, selection-invalid, too many excluded_ids or ids, or not exactly one of selection_token and ids"),
    ),
)]
pub async fn bulk_unsubscribe_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkList>,
) -> ApiResult<Json<BulkResult>> {
    let (target, list_id) = input.split();
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &target).await?;
    lists::get(&mut tx, workspace_id, list_id).await?;
    let excluded = &target.excluded_ids;
    let n = selection::unsubscribe(&mut tx, &sel, excluded, list_id).await?;
    let extra = json!({ "list_id": list_id });
    let action = actions::CONTACTS_BULK_UNSUBSCRIBED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &target),
        extra,
        (n, None),
    )
    .await
}

/// A hard delete of every contact in the selection. Like `DELETE /contacts/{id}` it writes no
/// suppression: the addresses may be added again. It is not erasure.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/contacts/bulk/delete",
    operation_id = "bulk_delete_contacts",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = BulkTarget,
    responses(
        (status = 200, description = "How many were deleted", body = BulkResult),
        (status = 403, description = "insufficient-role: bulk actions take editor"),
        (status = 404, description = "not a member, or a token for another workspace"),
        (status = 409, description = "selection-widened: the filter now matches more contacts than the selection was made for; nothing was deleted"),
        (status = 422, description = "selection-expired, selection-invalid, too many excluded_ids or ids, or not exactly one of selection_token and ids"),
    ),
)]
pub async fn bulk_delete_contacts(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<BulkTarget>,
) -> ApiResult<Json<BulkResult>> {
    let (mut tx, sel) = begin_bulk(&state, &principal, workspace_id, &input).await?;
    let n = selection::delete(&mut tx, &sel, &input.excluded_ids).await?;
    // Inside the transaction and after the statement, so the number compared is the one the
    // delete itself resolved rather than a second count that could disagree with it. Returning
    // here drops `tx` and the delete rolls back.
    if sel.widened(n.selected, input.excluded_ids.len()) {
        // `selected` counts the match less the exclusions, so the match itself is at most
        // `selected + excluded`; that is the number worth naming, and naming it as an upper
        // bound is what makes it honest.
        let now = n.selected.saturating_add(input.excluded_ids.len() as u64);
        return Err(ApiError::ConflictDetail {
            code: "selection-widened",
            detail: format!(
                "This view matched {} contacts when you selected it and matches up to {} now, so \
                 nothing was deleted. Contacts that already existed were changed to match it in \
                 between. Select again to see what you would be deleting.",
                sel.issued, now,
            ),
        });
    }
    let action = actions::CONTACTS_BULK_DELETED;
    finish_bulk(
        tx,
        &meta,
        &principal,
        action,
        (&sel, &input),
        json!({}),
        (n, None),
    )
    .await
}
