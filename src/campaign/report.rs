// SPDX-License-Identifier: AGPL-3.0-only

//! The campaign report (spec M2d §4.4, §4.6, §5.2): what was sent, what came of it, and which
//! links were clicked.
//!
//! Three plain reads, each with `workspace_id` in its predicate, and no transaction: the figures
//! are a running tally, so a click landing between two reads costs nothing a refresh does not fix.
//! The caller has already passed the campaign through [`super::load`], which is the 404.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use utoipa::ToSchema;

use super::{Campaign, Frozen};

#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignReport {
    pub sent: i64,
    pub failed: i64,
    pub bounced: i64,
    /// Messages opened at least once. Approximate: a mail client may preload the pixel, and one
    /// that blocks images never loads it.
    pub unique_opens: i64,
    /// Messages with at least one click.
    pub unique_clicks: i64,
    /// Contacts who unsubscribed through this campaign's message, counted once however many lists.
    pub unsubscribed: i64,
    /// Every link in every variant's frozen table, ordered by `(variant, link)`; empty before a
    /// launch.
    pub links: Vec<LinkReport>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LinkReport {
    /// 0-based.
    pub variant: i16,
    /// 0-based position in the variant's link table.
    pub link: i32,
    pub url: String,
    /// Clicks, repeats included.
    pub clicks: i64,
    /// Messages that clicked this link.
    pub unique_clicks: i64,
}

pub async fn report(db: &PgPool, c: &Campaign) -> sqlx::Result<CampaignReport> {
    let (unique_opens, unique_clicks): (i64, i64) = sqlx::query_as(
        "select count(*) filter (where opened_at is not null),
                count(*) filter (where clicked_at is not null)
         from messages where workspace_id = $1 and campaign_id = $2",
    )
    .bind(c.workspace_id)
    .bind(c.id)
    .fetch_one(db)
    .await?;

    let clicked: Vec<(i16, i32, i64, i64)> = sqlx::query_as(
        "select variant, link, sum(clicks)::bigint, count(*)
         from message_clicks where workspace_id = $1 and campaign_id = $2
         group by variant, link",
    )
    .bind(c.workspace_id)
    .bind(c.id)
    .fetch_all(db)
    .await?;
    let clicked: HashMap<(i16, i32), (i64, i64)> = clicked
        .into_iter()
        .map(|(v, l, n, u)| ((v, l), (n, u)))
        .collect();

    let unsubscribed: i64 = sqlx::query_scalar(
        "select count(distinct contact_id) from consent_log
         where workspace_id = $1 and source_ref = $2 and action = 'unsubscribed'",
    )
    .bind(c.workspace_id)
    .bind(c.id.to_string())
    .fetch_one(db)
    .await?;

    let frozen: Option<Value> =
        sqlx::query_scalar("select frozen from campaigns where workspace_id = $1 and id = $2")
            .bind(c.workspace_id)
            .bind(c.id)
            .fetch_one(db)
            .await?;
    let frozen = match frozen.as_ref().map(Frozen::from_column) {
        Some(Ok(f)) => Some(f),
        Some(Err(e)) => {
            tracing::error!(campaign = %c.id, error = %e, "a campaign report could not read the frozen rendition");
            None
        }
        None => None,
    };
    let links = frozen
        .iter()
        .flat_map(|f| f.variants.iter().enumerate())
        .flat_map(|(v, fv)| {
            fv.rendition
                .links()
                .iter()
                .enumerate()
                .map(move |(l, u)| (v as i16, l as i32, u))
        })
        .map(|(variant, link, url)| {
            let (clicks, unique_clicks) = clicked.get(&(variant, link)).copied().unwrap_or((0, 0));
            LinkReport {
                variant,
                link,
                url: url.clone(),
                clicks,
                unique_clicks,
            }
        })
        .collect();

    Ok(CampaignReport {
        sent: c.sent,
        failed: c.failed,
        bounced: c.bounced,
        unique_opens,
        unique_clicks,
        unsubscribed,
        links,
    })
}
