// SPDX-License-Identifier: AGPL-3.0-only

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;
use uuid::Uuid;

use std::collections::BTreeSet;

use super::{
    Authored, Doc, Kind, Stored, StoredError,
    block::{Block, Run},
    lint::{self, Finding, Rule},
    mjml::CompileError,
    render::{self, Contact, UrlCheck},
    stored_field_names,
};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    brand,
    custom_fields::{Definition, FieldKind, definitions_for},
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    pagination::{Page, PageParams},
    workspaces::roles::Role,
};

/// How many blocks one template may hold, `Columns` children included.
///
/// Depth is already bounded — `serde_json` refuses past 128 levels and `Columns` inside `Columns`
/// is refused outright — but WIDTH is not: a flat list of 200,000 `Divider`s deserialises fine and
/// then goes through the emitter, `mrml` and the classifier. The request body is capped by axum's
/// 2 MB default on top of this.
pub const MAX_BLOCKS: usize = 1_000;

/// How long a hand-written `mjml` or `html` source may be. `mrml` parses it and the classifier
/// walks it; both are save-time, but both are work an authenticated request can ask for.
pub const MAX_SOURCE: usize = 256 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/workspaces/{workspace_id}/templates",
            axum::routing::get(list_templates).post(create_template),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/templates/{id}",
            axum::routing::get(get_template)
                .patch(update_template)
                .delete(delete_template),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/templates/{id}/preview",
            axum::routing::post(preview_template),
        )
}

// ---------------------------------------------------------------------------------------------
// Wire types

/// A template as the API shows it. `compiled` is never returned: it is the renderer's own
/// storage, it is derived from the two columns below, and the editor has no use for an AST
/// (contract section 4).
#[derive(Debug, Serialize, ToSchema)]
pub struct TemplateView {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub kind: Kind,
    /// The block document, for a `blocks` template; null for a raw one.
    pub tree: Option<Doc>,
    /// The MJML or HTML the operator wrote; null for a `blocks` template.
    pub source: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What the list shows and the campaign picker reads (issue 50 item 1): no `tree`, no `source`.
#[derive(Debug, Serialize, ToSchema)]
pub struct TemplateSummary {
    pub id: Uuid,
    pub name: String,
    pub kind: Kind,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewTemplate {
    pub name: String,
    pub kind: Kind,
    /// Required when `kind` is `blocks`, and refused otherwise.
    pub tree: Option<Doc>,
    /// Required when `kind` is `mjml` or `html`, and refused otherwise. May not be empty.
    pub source: Option<String>,
}

/// A field left out is left alone. `kind` is not patchable: it decides which column holds the
/// document, so changing it is a different template, not an edit to this one.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateTemplate {
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = String, required = false)]
    pub name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Doc, required = false)]
    pub tree: Option<Option<Doc>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = String, required = false)]
    pub source: Option<Option<String>>,
}

/// What to preview. Send the document being edited to preview unsaved changes; send neither key
/// to preview what is stored.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplatePreviewRequest {
    pub tree: Option<Doc>,
    pub source: Option<String>,
}

/// Contract section 12, exactly. `html` is a whole document; a failed render or a failed check is
/// an ERROR and never an `Ok` with an empty `findings`, which the preview panel reads as a pass.
#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewResult {
    pub html: String,
    pub text: String,
    pub findings: Vec<Finding>,
}

/// Tells a key that is absent (`None`) from one present as `null` (`Some(None)`). The same helper
/// `relay`, `lists`, `contacts`, `forms`, `segments` and `sending_domains` each carry.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

// ---------------------------------------------------------------------------------------------
// The authored document, validated

/// What the operator wrote, owned, after the shape has been checked against the kind.
#[derive(Clone)]
enum Authoring {
    Blocks(Doc),
    Mjml(String),
    Html(String),
}

impl Authoring {
    fn authored(&self) -> Authored<'_> {
        match self {
            Authoring::Blocks(d) => Authored::Blocks(&d.blocks),
            Authoring::Mjml(s) => Authored::Mjml(s),
            Authoring::Html(s) => Authored::Html(s),
        }
    }

    /// `(tree, source)` for the columns. `templates_tree_xor_source` enforces the same pairing in
    /// the database; this is what gets written, not a second opinion about it.
    fn columns(&self) -> (Option<Value>, Option<&str>) {
        match self {
            Authoring::Blocks(d) => (serde_json::to_value(d).ok(), None),
            Authoring::Mjml(s) | Authoring::Html(s) => (None, Some(s.as_str())),
        }
    }

    /// The field name a refusal about the document hangs off.
    fn field(&self) -> &'static str {
        match self {
            Authoring::Blocks(_) => "tree",
            _ => "source",
        }
    }
}

fn block_count(blocks: &[Block]) -> usize {
    blocks
        .iter()
        .map(|b| match b {
            Block::Columns { children } => 1 + children.iter().map(Vec::len).sum::<usize>(),
            _ => 1,
        })
        .sum()
}

/// Every URL field in the tree, as `tree.blocks[2].children[0][1].src`, with its value.
fn url_fields(blocks: &[Block], prefix: &str, out: &mut Vec<(String, String)>) {
    for (i, block) in blocks.iter().enumerate() {
        let at = format!("{prefix}[{i}]");
        match block {
            Block::Image { src, .. } => out.push((format!("{at}.src"), src.clone())),
            Block::Button { href, .. } => out.push((format!("{at}.href"), href.clone())),
            Block::Text { runs } | Block::Heading { runs, .. } => {
                for (r, run) in runs.iter().enumerate() {
                    if let Run::Link { href, .. } = run {
                        out.push((format!("{at}.runs[{r}].href"), href.clone()));
                    }
                }
            }
            Block::Columns { children } => {
                for (c, column) in children.iter().enumerate() {
                    url_fields(column, &format!("{at}.children[{c}]"), out);
                }
            }
            Block::Divider {} | Block::Spacer { .. } => {}
        }
    }
}

/// Refuse a URL whose scheme is outside the frozen allowlist, at SAVE.
///
/// `render::static_url` would otherwise rewrite it to `#` when the email is built, so the
/// operator's link would be silently dead rather than refused. Reads `check_static_url`, which is
/// the renderer's own parser and allowlist: there is one URL implementation in this product and
/// the handler does not grow a second one. A relative or malformed URL is NOT refused here — it
/// names no scheme to disallow, M2c's unsubscribe links are written relative, and the link checker
/// reports the malformed ones as findings.
fn check_urls(blocks: &[Block], errors: &mut Vec<FieldError>) {
    let mut fields = Vec::new();
    url_fields(blocks, "tree.blocks", &mut fields);
    for (field, raw) in fields {
        if let UrlCheck::Scheme(scheme) = render::check_static_url(&raw) {
            errors.push(FieldError::new(
                field,
                format!(
                    "the scheme {scheme}: is not allowed; a link must be http, https, mailto or tel"
                ),
            ));
        }
    }
}

/// The authored document for `kind`, or the refusals that name what is wrong with it.
fn authoring(kind: Kind, tree: Option<Doc>, source: Option<String>) -> ApiResult<Authoring> {
    let mut errors = Vec::new();
    let authored = match kind {
        Kind::Blocks => {
            if source.is_some() {
                errors.push(FieldError::new(
                    "source",
                    "belongs to an mjml or html template; a blocks template keeps its document in \
                     tree",
                ));
            }
            match tree {
                None => {
                    errors.push(FieldError::new("tree", "is required for a blocks template"));
                    None
                }
                Some(doc) => {
                    let count = block_count(&doc.blocks);
                    if count > MAX_BLOCKS {
                        errors.push(FieldError::new(
                            "tree",
                            format!("has {count} blocks; at most {MAX_BLOCKS} are allowed"),
                        ));
                    }
                    check_urls(&doc.blocks, &mut errors);
                    Some(Authoring::Blocks(doc))
                }
            }
        }
        Kind::Mjml | Kind::Html => {
            if tree.is_some() {
                errors.push(FieldError::new(
                    "tree",
                    "belongs to a blocks template; an mjml or html template keeps its document in \
                     source",
                ));
            }
            match source {
                // RULING: an empty raw template is NOT savable. `templates.source is not null`
                // admits the empty string, which compiles to an email with nothing in it and
                // which the link checker cannot check at all (`LintError::Nothing`), so it would
                // be a template that can never be previewed.
                Some(s) if s.trim().is_empty() => {
                    errors.push(FieldError::new("source", "cannot be empty"));
                    None
                }
                Some(s) if s.len() > MAX_SOURCE => {
                    errors.push(FieldError::new(
                        "source",
                        format!("is {} bytes; at most {MAX_SOURCE} are allowed", s.len()),
                    ));
                    None
                }
                Some(s) if kind == Kind::Mjml => Some(Authoring::Mjml(s)),
                Some(s) => Some(Authoring::Html(s)),
                None => {
                    errors.push(FieldError::new(
                        "source",
                        "is required for an mjml or html template",
                    ));
                    None
                }
            }
        }
    };
    match (authored, errors.is_empty()) {
        (Some(a), true) => Ok(a),
        _ => Err(ApiError::Validation(errors)),
    }
}

/// A compile refusal as a `422` on the column the operator wrote into. `CompileError`'s `Display`
/// is already a sentence an operator can act on, including the tag text for a dangerous position.
fn compile_error(e: CompileError, field: &'static str) -> ApiError {
    ApiError::Validation(vec![FieldError::new(field, e.to_string())])
}

/// A stored rendition that may not be used, and that `preview` does not recompile.
///
/// Everything but a render failure is `409 template-stale`: the request was fine and the stored
/// document is not, and the cure is always the same — open the template and save it, which
/// compiles it again. `Stale` and `MarkerFormat` never get here (issue 49): they are recompiled in
/// memory from the authored document, never by guessing at the stored one — an unrecognised marker
/// format is a shape this build does not speak, and reading it is how a conditional's boundary
/// gets read as a field and the wrong range of a message is dropped.
fn stale(e: StoredError) -> ApiError {
    match e {
        // Classification passed at save, so these mean the stored document is not the one that
        // was classified: a bug here, not something an operator did.
        StoredError::Render(_)
        | StoredError::MarkedText
        | StoredError::NoUnsubscribeToken
        | StoredError::FooterLost(_) => ApiError::Internal(anyhow::anyhow!("{e}")),
        e => ApiError::ConflictDetail {
            code: "template-stale",
            detail: e.to_string(),
        },
    }
}

/// The contact a preview renders for. Not a real row: a preview must not hand one subscriber's
/// data to everyone who can open the editor, and an operator wants to see every tag filled rather
/// than whichever ones the first contact in the table happens to have.
///
/// Archived fields are included, because a contact keeps its values until the field is purged and
/// a template compiled before the archive still renders them.
pub(crate) fn sample(defs: &[Definition]) -> Value {
    let mut map = serde_json::Map::new();
    for d in defs {
        map.insert(
            d.key.clone(),
            match d.kind {
                FieldKind::Text => json!(d.label),
                FieldKind::Number => json!(42),
                FieldKind::Date => json!("2026-01-31"),
                FieldKind::DateTime => json!("2026-01-31T09:00:00Z"),
                FieldKind::Boolean => json!(true),
                FieldKind::Select | FieldKind::MultiSelect => {
                    d.options.first().map_or(Value::Null, |o| json!(o))
                }
            },
        );
    }
    Value::Object(map)
}

/// What a compiled document still names that has since been archived.
///
/// These are the references a re-save is allowed to keep (`tag::Registry::allowing`), so the
/// operator has to be able to SEE them: a save that silently accepted a tag the rules say is dead
/// would leave the field meaning nothing to every new contact, with nothing in the panel to say
/// so. Advisory, like every finding — the email is not wrong for anyone who already has a value.
fn archived_findings(stored: &Stored, defs: &[Definition]) -> Vec<Finding> {
    stored
        .field_names()
        .filter(|key| {
            defs.iter()
                .any(|d| d.key == *key && d.archived_at.is_some())
        })
        .map(|key| {
            Finding::new(
                Rule::ArchivedField,
                format!(
                    "a merge tag names the custom field \"{key}\", which has been archived. Every \
                     contact keeps the value it already had, so this still renders — but no new \
                     contact can be given one. Remove the tag, or restore the field."
                ),
                None,
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Rows

struct Row {
    id: Uuid,
    workspace_id: Uuid,
    name: String,
    kind: Kind,
    tree: Option<Value>,
    source: Option<String>,
    compiled: Option<Value>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl Row {
    /// The row's own document, back through the same validation an incoming one gets. A row that
    /// does not survive it was not written by this handler.
    fn authoring(&self) -> ApiResult<Authoring> {
        let tree = self
            .tree
            .as_ref()
            .map(|v| serde_json::from_value::<Doc>(v.clone()))
            .transpose()
            .map_err(|e| {
                ApiError::Internal(anyhow::anyhow!("stored tree is not a document: {e}"))
            })?;
        authoring(self.kind, tree, self.source.clone())
    }

    fn view(self) -> ApiResult<TemplateView> {
        let tree = self
            .tree
            .map(serde_json::from_value::<Doc>)
            .transpose()
            .map_err(|e| {
                ApiError::Internal(anyhow::anyhow!("stored tree is not a document: {e}"))
            })?;
        Ok(TemplateView {
            id: self.id,
            workspace_id: self.workspace_id,
            name: self.name,
            kind: self.kind,
            tree,
            source: self.source,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

async fn load(db: impl sqlx::PgExecutor<'_>, workspace_id: Uuid, id: Uuid) -> ApiResult<Row> {
    Ok(sqlx::query_as!(
        Row,
        r#"select id, workspace_id, name, kind as "kind: Kind", tree, source, compiled,
                  created_at, updated_at
           from templates where workspace_id = $1 and id = $2"#,
        workspace_id,
        id,
    )
    .fetch_one(db)
    .await?)
}

/// Names the template. The document is not in the metadata: it is kept in the row, it is large,
/// and the audit log is read by admins and kept for ever.
async fn record(
    db: &mut sqlx::PgConnection,
    meta: &RequestMeta,
    principal: &Principal,
    action: &'static str,
    (workspace_id, id): (Uuid, Uuid),
    metadata: Value,
) -> ApiResult<()> {
    audit::record(
        db,
        meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action,
            target_type: Some("template"),
            target_id: Some(id.to_string()),
            metadata,
        },
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Handlers

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/templates",
    operation_id = "list_templates",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), PageParams),
    responses(
        (status = 200, description = "Newest first", body = Page<TemplateSummary>),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_templates(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<TemplateSummary>>> {
    authorize(&state.pool, &principal, workspace_id, Role::Viewer).await?;
    let limit = page.limit();
    // Issue 50 item 1: the summary columns only. The document is read by `get_template`.
    let rows = sqlx::query_as!(
        TemplateSummary,
        r#"select id, name, kind as "kind: Kind", created_at, updated_at
           from templates
           where workspace_id = $1 and ($2::uuid is null or id < $2)
           order by id desc limit $3"#,
        workspace_id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |t| t.id)))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/templates/{id}",
    operation_id = "get_template",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id"),
    ),
    responses(
        (status = 200, body = TemplateView),
        (status = 404, description = "not a member, or no such template in this workspace"),
    ),
)]
pub async fn get_template(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<TemplateView>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    Ok(Json(load(&mut *conn, workspace_id, id).await?.view()?))
}

/// Compiling a template is the save: both renditions are produced in one call against one
/// registry, and stored with the AST in one statement.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/templates",
    operation_id = "create_template",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewTemplate,
    responses(
        (status = 201, body = TemplateView),
        (status = 403, description = "insufficient-role: writing a template takes editor"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "an invalid name or document, a merge tag naming a field \
            that does not exist or has been archived, a tag in a position no encoding makes safe, \
            a link whose scheme is not allowed, or an unknown field"),
    ),
)]
pub async fn create_template(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewTemplate>,
) -> ApiResult<(StatusCode, Json<TemplateView>)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let name =
        crate::lists::validate_name(&input.name).map_err(|e| ApiError::Validation(vec![e]))?;
    let authoring = authoring(input.kind, input.tree, input.source)?;
    let defs = definitions_for(&mut *tx, workspace_id).await?;
    let kit = brand::load(&mut *tx, workspace_id).await?;
    // No stored version, so nothing is grandfathered: a brand-new template naming an archived
    // field is the case the rule is about and it is refused.
    let compiled = super::compile(authoring.authored(), &kit.render(), &defs, &BTreeSet::new())
        .map_err(|e| compile_error(e, authoring.field()))?;
    let (tree, source) = authoring.columns();
    let id = Uuid::now_v7();
    // ONE locking statement (ADR 0005): the insert takes `for key share` on the workspace row
    // through `templates_workspace_id_fkey` and asks for nothing else. The two reads above are
    // plain selects and lock no row.
    sqlx::query!(
        "insert into templates (id, workspace_id, name, kind, tree, source, compiled) \
         values ($1, $2, $3, $4, $5, $6, $7)",
        id,
        workspace_id,
        name,
        input.kind as Kind,
        tree,
        source,
        serde_json::to_value(&compiled).map_err(|e| anyhow::anyhow!(e))?,
    )
    .execute(&mut *tx)
    .await?;
    record(
        &mut tx,
        &meta,
        &principal,
        actions::TEMPLATE_CREATED,
        (workspace_id, id),
        json!({ "name": name, "kind": input.kind }),
    )
    .await?;
    let created = load(&mut *tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(created.view()?)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{workspace_id}/templates/{id}",
    operation_id = "update_template",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id"),
    ),
    request_body = UpdateTemplate,
    responses(
        (status = 200, body = TemplateView),
        (status = 403, description = "insufficient-role: writing a template takes editor"),
        (status = 404, description = "not a member, or no such template in this workspace"),
        (status = 422, description = "an invalid name or document, an explicit null, a merge tag \
            naming a field that does not exist or has been archived, or an unknown field"),
    ),
)]
pub async fn update_template(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<UpdateTemplate>,
) -> ApiResult<Json<TemplateView>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let mut errors = Vec::new();
    for (field, null) in [
        ("name", matches!(input.name, Some(None))),
        ("tree", matches!(input.tree, Some(None))),
        ("source", matches!(input.source, Some(None))),
    ] {
        if null {
            errors.push(FieldError::new(field, "cannot be null"));
        }
    }
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    // Read before write, but only to find the kind and the document a patch leaves alone. A plain
    // select: no `for update`, so this stays one locking statement (ADR 0005's tripwire).
    let row = load(&mut *tx, workspace_id, id).await?;
    let name = match input.name.flatten() {
        Some(n) => crate::lists::validate_name(&n).map_err(|e| ApiError::Validation(vec![e]))?,
        None => row.name.clone(),
    };
    let (tree, source) = match (input.tree.flatten(), input.source.flatten()) {
        // Nothing about the document was sent: recompile what is stored, so a save always leaves
        // the renditions current with the brand kit as it is now.
        (None, None) => (
            row.tree
                .as_ref()
                .map(|v| serde_json::from_value::<Doc>(v.clone()))
                .transpose()
                .map_err(|e| ApiError::Internal(anyhow::anyhow!("stored tree: {e}")))?,
            row.source.clone(),
        ),
        (tree, source) => (tree, source),
    };
    let authoring = authoring(row.kind, tree, source)?;
    let defs = definitions_for(&mut *tx, workspace_id).await?;
    let kit = brand::load(&mut *tx, workspace_id).await?;
    // What the STORED version already named may still be named although it is archived. Without
    // this a workspace that archives a field cannot re-save any template that mentions it — and
    // `409 template-stale` tells the operator to do exactly that, so they are locked out of their
    // own template by two refusals that each point at the other. A reference that is NEW in the
    // submitted document is still refused; `preview` reports the surviving ones as findings.
    let keep = row
        .compiled
        .as_ref()
        .map(stored_field_names)
        .unwrap_or_default();
    let compiled = super::compile(authoring.authored(), &kit.render(), &defs, &keep)
        .map_err(|e| compile_error(e, authoring.field()))?;
    let (tree, source) = authoring.columns();
    // ONE locking statement, and it writes no foreign key column — `workspace_id` is in the
    // `where`, never the `set` — so it takes nothing at all on `workspaces` (ADR 0005).
    sqlx::query!(
        "update templates set name = $3, tree = $4, source = $5, compiled = $6, updated_at = now() \
         where workspace_id = $1 and id = $2",
        workspace_id,
        id,
        name,
        tree,
        source,
        serde_json::to_value(&compiled).map_err(|e| anyhow::anyhow!(e))?,
    )
    .execute(&mut *tx)
    .await?;
    record(
        &mut tx,
        &meta,
        &principal,
        actions::TEMPLATE_UPDATED,
        (workspace_id, id),
        json!({ "name": name, "kind": row.kind }),
    )
    .await?;
    let updated = load(&mut *tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok(Json(updated.view()?))
}

/// A hard delete.
#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{workspace_id}/templates/{id}",
    operation_id = "delete_template",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 403, description = "insufficient-role: writing a template takes editor"),
        (status = 404, description = "not a member, or no such template in this workspace"),
    ),
)]
pub async fn delete_template(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let gone = sqlx::query!(
        "delete from templates where workspace_id = $1 and id = $2 returning name",
        workspace_id,
        id,
    )
    .fetch_one(&mut *tx)
    .await?;
    record(
        &mut tx,
        &meta,
        &principal,
        actions::TEMPLATE_DELETED,
        (workspace_id, id),
        json!({ "name": gone.name }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The HTML part, the plain-text part and the advisory findings, for a sample contact.
///
/// With `tree` or `source` in the body this previews UNSAVED work: the document is compiled fresh
/// against the brand kit and the field definitions as they are now, and nothing is written. With
/// an empty body it previews what is stored, recompiled in memory — and still not written — when a
/// brand kit or renderer change has made it stale. The system footer is not shown here: it is part
/// of a campaign's send rendition, which the campaign preview shows.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/templates/{id}/preview",
    operation_id = "preview_template",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id"),
    ),
    request_body = TemplatePreviewRequest,
    responses(
        (status = 200, description = "`html` is a whole document; `findings` is empty only when \
            the checks ran and found nothing.", body = PreviewResult),
        (status = 404, description = "not a member, or no such template in this workspace"),
        (status = 409, description = "template-stale: the stored renditions were built against \
            custom fields that have since changed type or been purged, or cannot be read. Save the \
            template again. A brand kit or renderer change is not a 409: the template is \
            recompiled for the preview and nothing is written."),
        (status = 422, description = "the document does not compile, or the checks could not run"),
        (status = 429, description = "rate-limited: too many template previews for this \
            workspace. Its own budget, not the segment preview's."),
    ),
)]
pub async fn preview_template(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<TemplatePreviewRequest>,
) -> ApiResult<Response> {
    authorize(&state.pool, &principal, workspace_id, Role::Viewer).await?;
    let quota = state.template_preview_limit.check(workspace_id);
    if let Some(retry_after_secs) = quota.over {
        return Ok((
            quota.headers,
            ApiError::TooManyRequests { retry_after_secs },
        )
            .into_response());
    }
    // Every answer carries the `RateLimit` headers, refusals included: an editor being told `422`
    // on a debounce is exactly the client that needs to know how little of its window is left.
    Ok(match preview(&state, workspace_id, id, input).await {
        Ok(result) => (quota.headers, Json(result)).into_response(),
        Err(e) => (quota.headers, e).into_response(),
    })
}

async fn preview(
    state: &AppState,
    workspace_id: Uuid,
    id: Uuid,
    input: TemplatePreviewRequest,
) -> ApiResult<PreviewResult> {
    let mut conn = state.pool.acquire().await?;
    let row = load(&mut *conn, workspace_id, id).await?;
    let defs = definitions_for(&mut *conn, workspace_id).await?;
    let kit = brand::load(&mut *conn, workspace_id).await?;
    drop(conn);
    let brand = kit.render();

    let unsaved = input.tree.is_some() || input.source.is_some();
    let authoring = if unsaved {
        authoring(row.kind, input.tree, input.source)?
    } else {
        row.authoring()?
    };
    // The same allowance `update_template` makes, for the same reason: the editor previews the
    // document it is about to save, so a preview that refused what the save accepts would send
    // the operator looking for a problem that is not there.
    let keep = row
        .compiled
        .as_ref()
        .map(stored_field_names)
        .unwrap_or_default();
    // `mrml` runs here, so on the blocking pool (spec §4.5 ruling 3): every compile outside a
    // save, the issue-49 recompile included.
    let fresh = || {
        let (a, brand, defs, keep) = (authoring.clone(), brand.clone(), defs.clone(), keep.clone());
        async move {
            let field = a.field();
            tokio::task::spawn_blocking(move || super::compile(a.authored(), &brand, &defs, &keep))
                .await
                .map_err(|e| {
                    ApiError::Internal(anyhow::anyhow!("the template compile failed: {e}"))
                })?
                .map_err(|e| compile_error(e, field))
        }
    };
    // The fingerprint of the document and the brand kit as they are NOW.
    let key = super::fingerprint(authoring.authored(), &brand);
    let stored = if unsaved {
        fresh().await?
    } else {
        let compiled = row
            .compiled
            .as_ref()
            .ok_or_else(|| ApiError::ConflictDetail {
                code: "template-stale",
                detail: "this template has never been compiled. Open it and save it again."
                    .to_owned(),
            })?;
        // Issue 49: a rendition made stale by a kit edit or a renderer change is recompiled here,
        // in memory, and NEVER written back — this handler writes no row. Field drift still
        // refuses: it changes what a tag means, and `ready` answers it before `Stale`.
        match Stored::from_column(compiled) {
            Ok(s) => match s.ready(&key, &defs).map(|_| ()) {
                Ok(()) => s,
                Err(StoredError::Stale) => fresh().await?,
                Err(e) => return Err(stale(e)),
            },
            Err(StoredError::MarkerFormat { .. }) => fresh().await?,
            Err(e) => return Err(stale(e)),
        }
    };
    // The same gate a send goes through: the key above, plus the field definitions as they are
    // now.
    let rendered = stored
        .ready(&key, &defs)
        .map_err(stale)?
        .render(&Contact {
            email: "sam.rivera@example.com",
            first_name: "Sam",
            last_name: "Rivera",
            timezone: Some("UTC"),
            attributes: &sample(&defs),
            // A template preview has no footer, so nothing reads this.
            unsubscribe_token: "preview",
            tracking_token: None,
        })
        .map_err(stale)?;

    // `LintError` is NOT an empty findings list: both its variants mean the check could not run,
    // and contract section 12 is explicit that the panel reads an empty list as a pass. A checker
    // that failed is an error here, never a clean result.
    //
    // shortcut: a raw `html` template gets neither of R6's checks — there is no `mj-style` to
    // misuse and no block tree to read URL fields from, and handing HTML to `lint` as if it were
    // MJML fails in `mrml::parse` on every preview (R6 disclosed the same limit for raw templates
    // generally). It says so in a finding of its own rather than answering with a bare empty list,
    // which the panel would read as "checked, nothing wrong". Upgrade it by giving `lint` the
    // compiled document's URL attributes if raw HTML templates turn out to need link findings.
    let mut findings = match &authoring {
        Authoring::Blocks(doc) => lint::lint("", Some(&doc.blocks)),
        // The MARKED source, because that is the string `mrml` parsed at save: the raw source can
        // hold a tag fallback (`{{ x | "a" }}` inside an attribute) that compile lifts out and
        // `mrml` cannot parse in place, which would refuse every preview of a valid template.
        Authoring::Mjml(_) => lint::lint(&render::marked(&stored.nodes).0, None).map(|mut f| {
            f.push(Finding::new(
                Rule::NotChecked,
                "a raw MJML template's links are not checked: the link check reads the block \
                 tree, and this template has none. The style checks above did run. Nothing \
                 here is a statement about its links."
                    .to_owned(),
                None,
            ));
            f
        }),
        Authoring::Html(_) => Ok(vec![Finding::new(
            Rule::NotChecked,
            "a raw HTML template is not link-checked or style-checked: both checks read MJML or \
             the block tree, and this template has neither. Nothing below is a statement about \
             its links."
                .to_owned(),
            None,
        )]),
    }
    .map_err(uncheckable)?;
    findings.extend(archived_findings(&stored, &defs));

    Ok(PreviewResult {
        html: rendered.html,
        text: rendered.text,
        findings,
    })
}

/// A check that could not run is a refusal, never a clean result (contract section 12).
fn uncheckable(e: lint::LintError) -> ApiError {
    ApiError::UnprocessableDetail {
        code: "template-uncheckable",
        detail: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_check_is_a_422_template_uncheckable_never_an_empty_list() {
        // The API can no longer reach `LintError` through a template that saved (lint reads the
        // marked source `mrml` already parsed), so the mapping is pinned here, against sources
        // that genuinely fail `lint`.
        for (source, blocks) in [("<mjml><mj-body>", None), ("  ", None)] {
            let e = uncheckable(lint::lint(source, blocks).unwrap_err());
            let ApiError::UnprocessableDetail { code, detail } = &e else {
                panic!("not a 422 detail: {e:?}");
            };
            assert_eq!(*code, "template-uncheckable");
            assert!(detail.contains("could not be checked") || detail.contains("nothing to check"));
            assert_eq!(e.into_response().status(), StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    #[test]
    fn a_columns_block_counts_its_children() {
        let blocks = vec![
            Block::Divider {},
            Block::Columns {
                children: vec![
                    vec![Block::Divider {}, Block::Divider {}],
                    vec![Block::Divider {}],
                ],
            },
        ];
        assert_eq!(block_count(&blocks), 5);
    }

    #[test]
    fn every_url_field_is_found_including_inside_a_column() {
        let blocks = vec![
            Block::Image {
                src: "a".into(),
                alt: String::new(),
            },
            Block::Columns {
                children: vec![vec![
                    Block::Button {
                        label: "b".into(),
                        href: "c".into(),
                    },
                    Block::Text {
                        runs: vec![Run::Link {
                            href: "d".into(),
                            text: "t".into(),
                            marks: vec![],
                        }],
                    },
                ]],
            },
        ];
        let mut out = Vec::new();
        url_fields(&blocks, "tree.blocks", &mut out);
        let found: Vec<&str> = out.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(found, ["a", "c", "d"]);
        assert_eq!(out[2].0, "tree.blocks[1].children[0][1].runs[0].href");
    }
}
