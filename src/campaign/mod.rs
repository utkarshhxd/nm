// SPDX-License-Identifier: AGPL-3.0-only

//! Campaigns (spec M2c): the row, what the API shows of it, and [`prepare`], the one validation a
//! campaign passes before it may be scheduled, tested or launched.
//!
//! `campaigns.frozen`, the rendition a launch fixes, is NOT part of [`Campaign`]: it can be
//! megabytes and the screen polls the row every two seconds. Read it with [`load_frozen`], and
//! only through [`Frozen::from_column`].

use std::collections::BTreeSet;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgExecutor, PgPool, types::Json};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    brand,
    custom_fields::{Definition, definitions_for},
    error::{ApiError, ApiResult},
    segments,
    sending_domains::{self, not_passing},
    template::{
        Authored, Doc, FreezeError, Kind, Stored, StoredError, SystemFooter,
        mjml::CompileError,
        tag::{self, Node, Registry},
        text,
    },
};

pub mod launch;
pub mod queue;
pub mod routes;
pub mod schedule;
pub mod send;

/// How many content variants a campaign may carry (spec §4.9).
pub const MAX_VARIANTS: usize = 4;

/// Where a campaign is in the machine of spec §4.1. Stored as text under a `check` constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum CampaignState {
    Draft,
    Scheduled,
    Sending,
    Paused,
    Sent,
    Cancelled,
    Failed,
}
pub use CampaignState as State;

impl State {
    /// The word on the wire and in the column.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Draft => "draft",
            State::Scheduled => "scheduled",
            State::Sending => "sending",
            State::Paused => "paused",
            State::Sent => "sent",
            State::Cancelled => "cancelled",
            State::Failed => "failed",
        }
    }
}

/// How `schedule_local` is read: one instant for everyone, or that wall-clock time in each
/// recipient's own zone (spec §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum ScheduleMode {
    Instant,
    RecipientLocal,
}
pub use ScheduleMode as Mode;

/// One content variant. A single variant is an ordinary send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CampaignVariant {
    /// Merge-tag grammar, as plain text; compiled at save and at freeze.
    pub subject: String,
    pub template_id: Uuid,
    /// Overrides the campaign's `from_name` for this variant.
    pub from_name: Option<String>,
}
pub use CampaignVariant as Variant;

/// The row, less `frozen` (see the module docs).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Campaign {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub state: State,
    pub list_ids: Vec<Uuid>,
    pub segment_id: Option<Uuid>,
    pub sending_domain_id: Option<Uuid>,
    pub from_local_part: Option<String>,
    pub from_name: Option<String>,
    pub reply_to: Option<String>,
    pub variants: Json<Vec<Variant>>,
    pub ab_test_percent: Option<i16>,
    pub winner: Option<i16>,
    pub schedule_mode: Mode,
    pub schedule_zone: Option<String>,
    pub schedule_local: Option<NaiveDateTime>,
    pub send_at: Option<DateTime<Utc>>,
    pub launch_job_id: Option<Uuid>,
    pub phase: i16,
    pub resolve_cursor: Option<Uuid>,
    pub resolved: bool,
    pub frozen_at: Option<DateTime<Utc>>,
    pub total: i64,
    pub sent: i64,
    pub failed: i64,
    pub unknown: i64,
    pub skipped: i64,
    pub failure_reason: Option<String>,
    pub pause_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub launched_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Every [`Campaign`] column, spelled once as a literal so a query can `concat!` it.
macro_rules! columns {
    () => {
        "id, workspace_id, name, state, list_ids, segment_id, sending_domain_id, from_local_part, \
         from_name, reply_to, variants, ab_test_percent, winner, schedule_mode, schedule_zone, \
         schedule_local, send_at, launch_job_id, phase, resolve_cursor, resolved, frozen_at, \
         total, sent, failed, unknown, skipped, failure_reason, pause_reason, created_at, \
         updated_at, launched_at, finished_at"
    };
}

/// The campaign, or `404` if it is not in this workspace. Every route reads it through here, so
/// another workspace's id is a 404 before anything else is looked at.
pub async fn load(db: impl PgExecutor<'_>, ws: Uuid, id: Uuid) -> ApiResult<Campaign> {
    Ok(sqlx::query_as::<_, Campaign>(concat!(
        "select ",
        columns!(),
        " from campaigns where workspace_id = $1 and id = $2"
    ))
    .bind(ws)
    .bind(id)
    .fetch_one(db)
    .await?)
}

/// `campaigns.frozen`, read through [`Frozen::from_column`]. `None` for a campaign that has not
/// been launched.
pub async fn load_frozen(
    db: impl PgExecutor<'_>,
    ws: Uuid,
    id: Uuid,
) -> ApiResult<Option<Result<Frozen, StoredError>>> {
    let column: Option<Value> =
        sqlx::query_scalar("select frozen from campaigns where workspace_id = $1 and id = $2")
            .bind(ws)
            .bind(id)
            .fetch_one(db)
            .await?;
    Ok(column.as_ref().map(Frozen::from_column))
}

// ---------------------------------------------------------------------------------------------
// What the API shows

/// Contract section 2. `queued` is what is left: in flight included.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
pub struct CampaignCounts {
    pub total: i64,
    pub queued: i64,
    pub sent: i64,
    pub failed: i64,
    pub unknown: i64,
    pub skipped: i64,
}

impl CampaignCounts {
    pub fn new(total: i64, sent: i64, failed: i64, unknown: i64, skipped: i64) -> CampaignCounts {
        CampaignCounts {
            total,
            queued: (total - sent - failed - unknown - skipped).max(0),
            sent,
            failed,
            unknown,
            skipped,
        }
    }
}

/// The schedule as typed, and the instant the server resolved it to.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CampaignSchedule {
    pub mode: Mode,
    /// IANA name.
    pub zone: String,
    /// `YYYY-MM-DDTHH:MM`, no zone; null means send now.
    pub local: Option<String>,
    /// RFC 5545 resolution of `local` in `zone`; for `recipient_local`, the earliest zone's.
    pub send_at: Option<DateTime<Utc>>,
}

/// `<input type="datetime-local">`'s own format.
pub const LOCAL_FORMAT: &str = "%Y-%m-%dT%H:%M";

#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignView {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub state: State,
    pub list_ids: Vec<Uuid>,
    pub segment_id: Option<Uuid>,
    pub sending_domain_id: Option<Uuid>,
    pub from_local_part: Option<String>,
    pub from_name: Option<String>,
    pub reply_to: Option<String>,
    /// 1..4 once saved with content; empty on a fresh draft.
    pub variants: Vec<Variant>,
    /// 10..100, only with two or more variants; 100 is a pure split with no winner phase.
    pub ab_test_percent: Option<i16>,
    /// The variant index an admin picked.
    pub winner: Option<i16>,
    /// The A/B test is over and the remainder waits for `…/winner`.
    pub awaiting_winner: bool,
    pub schedule: Option<CampaignSchedule>,
    pub counts: CampaignCounts,
    pub failure_reason: Option<String>,
    pub pause_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub launched_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignSummary {
    pub id: Uuid,
    pub name: String,
    pub state: State,
    pub send_at: Option<DateTime<Utc>>,
    pub counts: CampaignCounts,
    pub updated_at: DateTime<Utc>,
}

/// How many contacts the audience is, or that there are too many to count in two seconds.
#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignEstimate {
    pub count: Option<i64>,
    pub exact: bool,
}

/// One variant as one sample recipient would receive it, footer included.
#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignPreview {
    pub subject: String,
    pub html: String,
    pub text: String,
}

impl Campaign {
    pub fn counts(&self) -> CampaignCounts {
        CampaignCounts::new(
            self.total,
            self.sent,
            self.failed,
            self.unknown,
            self.skipped,
        )
    }

    /// The A/B test's cohort is fully resolved and nothing of it is left to send, so the remainder
    /// is waiting for an admin.
    pub fn awaiting_winner(&self) -> bool {
        matches!(self.state, State::Sending | State::Paused)
            && self.phase == 1
            && self.resolved
            && self.ab_test_percent.is_some_and(|p| p < 100)
            && self.counts().queued == 0
    }

    pub fn view(self) -> CampaignView {
        let counts = self.counts();
        let awaiting_winner = self.awaiting_winner();
        CampaignView {
            schedule: self.schedule_zone.map(|zone| CampaignSchedule {
                mode: self.schedule_mode,
                zone,
                local: self
                    .schedule_local
                    .map(|l| l.format(LOCAL_FORMAT).to_string()),
                send_at: self.send_at,
            }),
            id: self.id,
            workspace_id: self.workspace_id,
            name: self.name,
            state: self.state,
            list_ids: self.list_ids,
            segment_id: self.segment_id,
            sending_domain_id: self.sending_domain_id,
            from_local_part: self.from_local_part,
            from_name: self.from_name,
            reply_to: self.reply_to,
            variants: self.variants.0,
            ab_test_percent: self.ab_test_percent,
            winner: self.winner,
            awaiting_winner,
            counts,
            failure_reason: self.failure_reason,
            pause_reason: self.pause_reason,
            created_at: self.created_at,
            updated_at: self.updated_at,
            launched_at: self.launched_at,
            finished_at: self.finished_at,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The frozen rendition

/// What a launch fixes: the From, and each variant compiled for sending. Stored whole in
/// `campaigns.frozen`; the send path substitutes into it and never compiles.
///
/// No `Deserialize`: [`Frozen::from_column`] is the only way back in, because it re-runs the
/// checks a column's contents have not passed.
#[derive(Debug, Serialize)]
pub struct Frozen {
    /// `{from_local_part}@{domain}`, the header From and the envelope From.
    pub from: String,
    pub reply_to: Option<String>,
    pub sending_domain_id: Uuid,
    pub variants: Vec<FrozenVariant>,
}

#[derive(Debug, Serialize)]
pub struct FrozenVariant {
    /// The subject's merge-tag AST; render it with `text::render_subject`.
    pub subject: Vec<Node>,
    /// The variant's own name, else the campaign's, else the sending domain's.
    pub from_name: Option<String>,
    pub rendition: Stored,
}

#[derive(Deserialize)]
struct RawFrozen {
    from: String,
    reply_to: Option<String>,
    sending_domain_id: Uuid,
    variants: Vec<RawVariant>,
}

#[derive(Deserialize)]
struct RawVariant {
    subject: Vec<Node>,
    from_name: Option<String>,
    rendition: Value,
}

impl Frozen {
    /// Read `campaigns.frozen`. The rendition goes through [`Stored::from_column`], and the
    /// subject's AST is depth-checked like every other stored AST before anything walks it.
    pub fn from_column(v: &Value) -> Result<Frozen, StoredError> {
        let raw: RawFrozen =
            serde_json::from_value(v.clone()).map_err(|e| StoredError::Malformed(e.to_string()))?;
        let variants = raw
            .variants
            .into_iter()
            .map(|v| {
                tag::check_depth(&v.subject).map_err(|_| StoredError::TooDeep)?;
                Ok(FrozenVariant {
                    subject: v.subject,
                    from_name: v.from_name,
                    rendition: Stored::from_column(&v.rendition)?,
                })
            })
            .collect::<Result<_, StoredError>>()?;
        Ok(Frozen {
            from: raw.from,
            reply_to: raw.reply_to,
            sending_domain_id: raw.sending_domain_id,
            variants,
        })
    }
}

/// Whether every variant of a frozen rendition still passes `ready` against the fields as they are
/// now, on its OWN key (spec §4.7 ruling 10). The one predicate preview, resume and the send loop
/// share, so what is previewed is what is sent. `Err` names the first variant that does not.
pub fn frozen_usable(f: &Frozen, defs: &[Definition]) -> Result<(), Refusal> {
    for (i, v) in f.variants.iter().enumerate() {
        match v.rendition.ready(v.rendition.fingerprint(), defs) {
            Ok(_) => {}
            Err(StoredError::FieldsChanged(drift)) => {
                return Err(Refusal::Drift {
                    variant: i,
                    fields: drift.into_iter().map(|d| d.key).collect(),
                });
            }
            Err(e) => {
                return Err(Refusal::Template {
                    variant: i,
                    error: e.to_string(),
                });
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// prepare

/// Why a campaign may not be scheduled, tested, previewed or launched. Each names what to fix.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("the brand kit has no postal address; set it in the footer before sending")]
    PostalAddress,
    #[error("{0}")]
    DomainUnverified(String),
    #[error("{field} needs {what}")]
    Missing { field: String, what: &'static str },
    #[error("variants[{variant}]: {error}")]
    Template { variant: usize, error: String },
    /// The variant's subject does not compile.
    #[error("variants[{variant}].subject: {error}")]
    Subject { variant: usize, error: String },
    #[error(
        "variants[{variant}]'s template was saved against custom fields that have since changed \
         type or been purged ({}). Open it and save it again.",
        .fields.join(", ")
    )]
    Drift { variant: usize, fields: Vec<String> },
    /// Not the operator's fault (a database error): answer 500, and a launch job retries.
    #[error("{0}")]
    Internal(String),
}

fn missing(field: &str, what: &'static str) -> Refusal {
    Refusal::Missing {
        field: field.to_owned(),
        what,
    }
}

fn internal(e: impl std::fmt::Display) -> Refusal {
    Refusal::Internal(e.to_string())
}

#[derive(sqlx::FromRow)]
struct TemplateRow {
    kind: Kind,
    tree: Option<Value>,
    source: Option<String>,
    compiled: Option<Value>,
}

/// A template's authored document, owned, so it can cross into `spawn_blocking`.
enum Owned {
    Blocks(Doc),
    Mjml(String),
    Html(String),
}

impl Owned {
    fn authored(&self) -> Authored<'_> {
        match self {
            Owned::Blocks(d) => Authored::Blocks(&d.blocks),
            Owned::Mjml(s) => Authored::Mjml(s),
            Owned::Html(s) => Authored::Html(s),
        }
    }
}

/// The one validation (spec §4.2 ruling 2, §4.4, §4.5): the audience, the domain and its gate,
/// the From, the postal address, and every variant compiled for sending.
///
/// Runs `mrml`, so call it OUTSIDE any transaction; the compiles are on the blocking pool. Nothing
/// here writes. The schedule handler and the test send call it to refuse early; the launch calls
/// it to produce what it stores.
pub async fn prepare(db: &PgPool, public_url: &url::Url, c: &Campaign) -> Result<Frozen, Refusal> {
    let ws = c.workspace_id;
    let kit = brand::load(db, ws).await.map_err(internal)?;
    let address = kit
        .footer
        .get("address")
        .and_then(Value::as_str)
        .unwrap_or("");
    if address.trim().is_empty() {
        return Err(Refusal::PostalAddress);
    }
    let address = address.to_owned();

    // Audience.
    if c.list_ids.is_empty() {
        return Err(missing("list_ids", "at least one list"));
    }
    let wanted: BTreeSet<Uuid> = c.list_ids.iter().copied().collect();
    let found: i64 =
        sqlx::query_scalar("select count(*) from lists where workspace_id = $1 and id = any($2)")
            .bind(ws)
            .bind(&c.list_ids)
            .fetch_one(db)
            .await
            .map_err(internal)?;
    if usize::try_from(found).ok() != Some(wanted.len()) {
        return Err(missing("list_ids", "every list it names to still exist"));
    }
    if let Some(id) = c.segment_id {
        let mut conn = db.acquire().await.map_err(internal)?;
        match segments::load_rules(&mut conn, ws, id).await {
            Ok(_) => {}
            Err(ApiError::Internal(e)) => return Err(internal(e)),
            Err(_) => {
                return Err(missing(
                    "segment_id",
                    "a segment that still exists and can be read",
                ));
            }
        }
    }

    // Sender.
    let domain_id = c
        .sending_domain_id
        .ok_or_else(|| missing("sending_domain_id", "a sending domain"))?;
    let domain = sending_domains::by_id(db, ws, domain_id)
        .await
        .map_err(|e| internal(format!("{e:?}")))?
        .ok_or_else(|| missing("sending_domain_id", "a sending domain that still exists"))?;
    let unverified = not_passing(&domain);
    if !unverified.is_empty() && domain.send_unverified_acknowledged_at.is_none() {
        return Err(Refusal::DomainUnverified(format!(
            "{} has not passed its {} DNS check{}, and no admin has acknowledged sending from it \
             anyway",
            domain.domain,
            unverified.join(", "),
            if unverified.len() == 1 { "" } else { "s" },
        )));
    }
    let local = c
        .from_local_part
        .as_deref()
        .ok_or_else(|| missing("from_local_part", "a from address"))?;

    // Content.
    let variants = &c.variants.0;
    if variants.is_empty() {
        return Err(missing("variants", "at least one variant"));
    }
    if variants.len() > 1 && c.ab_test_percent.is_none() {
        return Err(missing(
            "ab_test_percent",
            "a percentage when there is more than one variant",
        ));
    }
    let defs = definitions_for(db, ws).await.map_err(internal)?;
    let registry = Registry::from_definitions(&defs);
    let mut subjects = Vec::with_capacity(variants.len());
    let mut templates = Vec::with_capacity(variants.len());
    for (i, v) in variants.iter().enumerate() {
        subjects.push(text::compile_subject(&v.subject, &registry).map_err(|e| {
            Refusal::Subject {
                variant: i,
                error: CompileError::Tag(e).to_string(),
            }
        })?);
        let row: Option<TemplateRow> = sqlx::query_as(
            "select kind, tree, source, compiled from templates \
             where workspace_id = $1 and id = $2",
        )
        .bind(ws)
        .bind(v.template_id)
        .fetch_optional(db)
        .await
        .map_err(internal)?;
        let TemplateRow {
            kind,
            tree,
            source,
            compiled,
        } = row.ok_or_else(|| Refusal::Template {
            variant: i,
            error: "its template no longer exists".to_owned(),
        })?;
        let unreadable = |what: &str| Refusal::Template {
            variant: i,
            error: format!("its template's {what} could not be read; open it and save it again"),
        };
        let owned = match (kind, tree, source) {
            (Kind::Blocks, Some(tree), None) => Owned::Blocks(
                serde_json::from_value(tree).map_err(|_| unreadable("block document"))?,
            ),
            (Kind::Mjml, None, Some(s)) => Owned::Mjml(s),
            (Kind::Html, None, Some(s)) => Owned::Html(s),
            _ => return Err(unreadable("document")),
        };
        templates.push((owned, compiled));
    }

    // The compiles: `mrml` and the classifier, on the blocking pool, holding no connection.
    let (brand_kit, public_url) = (kit.render(), public_url.clone());
    let renditions = tokio::task::spawn_blocking(move || {
        let footer = SystemFooter {
            public_url: &public_url,
            postal_address: &address,
        };
        templates
            .iter()
            .enumerate()
            .map(|(i, (owned, compiled))| {
                crate::template::freeze(
                    compiled.as_ref(),
                    owned.authored(),
                    &brand_kit,
                    &defs,
                    &footer,
                )
                .map_err(|e| match e {
                    FreezeError::Drift(d) => Refusal::Drift {
                        variant: i,
                        fields: d.into_iter().map(|d| d.key).collect(),
                    },
                    FreezeError::Compile(e) => Refusal::Template {
                        variant: i,
                        error: e.to_string(),
                    },
                })
            })
            .collect::<Result<Vec<_>, _>>()
    })
    .await
    .map_err(internal)??;

    Ok(Frozen {
        from: format!("{local}@{}", domain.domain),
        reply_to: c.reply_to.clone(),
        sending_domain_id: domain.id,
        variants: variants
            .iter()
            .zip(subjects)
            .zip(renditions)
            .map(|((v, subject), rendition)| FrozenVariant {
                subject,
                from_name: v
                    .from_name
                    .clone()
                    .or_else(|| c.from_name.clone())
                    .or_else(|| domain.from_name.clone()),
                rendition,
            })
            .collect(),
    })
}
