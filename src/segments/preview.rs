// SPDX-License-Identifier: AGPL-3.0-only

//! Counting an unsaved rule document: exact when it is cheap, honest when it is not (spec M1b
//! section 5). `spec.md:457` budgets p95 < 2 s at 10M contacts and allows an estimate while
//! typing, so the count runs under a 1500 ms `statement_timeout` and falls back to the planner's
//! own row estimate when that fires.
//!
//! Three things here are measured facts rather than style, and each is a way a plausible
//! implementation is wrong:
//!
//! 1. **`SET LOCAL` outside a transaction is a `WARNING`, not an error**, and leaves
//!    `statement_timeout` at `0`. A count that forgets to open a transaction therefore has no
//!    timeout at all and hangs for as long as the query takes. `tests/segments.rs` asserts the
//!    preview *timed out*, not merely that it was inexact, because the second assertion passes
//!    vacuously in exactly that case.
//! 2. **A timed-out transaction is aborted** and refuses every later statement, so the estimate
//!    opens a fresh one.
//! 3. **`EXPLAIN (FORMAT JSON)` of a `count(*)` reports `Plan Rows: 1`** at every node, because
//!    the aggregate really does produce one row. The estimate therefore explains the
//!    *row-producing* select, and reads the **top** node: a scan node's figure is per worker, so
//!    a parallel plan's leaf would under-report by the number of workers.
//!
//! The compiler emits no `workspace_id` restriction of its own (see `compile::push_rules`), so
//! both queries below write one. Without it a preview would count every tenant's contacts.

use std::{collections::HashMap, time::Instant};

use serde::Serialize;
use serde_json::Value;
use sqlx::{AssertSqlSafe, PgPool, Postgres, QueryBuilder};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{
    compile::push_rules,
    rules::{Field, Group, RuleNode},
};
use crate::error::{ApiError, ApiResult};

/// Postgres' `query_canceled`, which is what `statement_timeout` raises.
const QUERY_CANCELED: &str = "57014";

/// `set local statement_timeout = <milliseconds>`, which is section 5's budget
/// (`Config::preview_timeout_ms` defaults to 1500 ms).
///
/// **The injection audit `AssertSqlSafe` asks for:** the only interpolated value is a `u64` from
/// `Config`, so the statement can differ from this literal only in its digits. Nothing a request
/// carries reaches it. `SET` takes no placeholders, which is why it is formatted at all.
fn timeout_statement(timeout_ms: u64) -> AssertSqlSafe<String> {
    AssertSqlSafe(format!("set local statement_timeout = {timeout_ms}"))
}

/// How many contacts a rule document matches.
#[derive(Debug, Serialize, ToSchema)]
pub struct Preview {
    /// The matching contacts, or `null` when no honest number can be given — the document filters
    /// on a custom attribute, which has no statistics (section 6). The rule builder shows "too
    /// many to count exactly" for `null`.
    pub count: Option<i64>,
    /// `false` when `count` is the planner's estimate, or when there is no count at all.
    pub exact: bool,
    pub took_ms: u64,
}

/// Counts the contacts `group` matches in `workspace_id`.
///
/// A document with any `attr:` rule gets **no number**. Section 6 ships no attribute index, so
/// Postgres has no statistics for `->>` and falls back to a default selectivity — measured 66×
/// low against a true count of 66,666. A planner figure there would be worse than none.
pub async fn preview(
    pool: &PgPool,
    workspace_id: Uuid,
    group: &Group,
    timeout_ms: u64,
) -> ApiResult<Preview> {
    let started = Instant::now();
    if filters_on_an_attribute(group) {
        return Ok(answer(None, false, started));
    }
    match count(pool, workspace_id, group, timeout_ms).await {
        Ok(n) => Ok(answer(Some(n), true, started)),
        Err(e) if timed_out(&e) => {
            let estimate = estimate(pool, workspace_id, group, timeout_ms).await?;
            Ok(answer(Some(estimate), false, started))
        }
        Err(e) => Err(e.into()),
    }
}

/// One segment's count in a batch. `Preview`'s `took_ms` has no meaning per row: the batch is one
/// statement and every row in it took the same time.
#[derive(Debug, Serialize, ToSchema)]
pub struct SegmentCount {
    pub id: Uuid,
    /// `null` where section 5 allows no honest number: the document filters on a custom attribute,
    /// or the batch ran out of time. The list shows "too many to count exactly" for both.
    pub count: Option<i64>,
    /// Always `true` alongside a number here: a batch is counted exactly or not at all.
    pub exact: bool,
}

/// How many contacts each of `segments` matches, in **one** statement on **one** connection.
///
/// The list view counts every segment on the page. A preview request per row spends a workspace's
/// whole quota on a single page load — 30 a minute against 25 rows — which is what makes section
/// 19's lateral-join note urgent ahead of its latency argument.
///
/// Section 5 still decides what a count may say, and this path is strictly more conservative than
/// `preview`: a document filtering on a custom attribute gets no number without touching the
/// database, and a batch that times out gets no numbers at all. The planner estimate `preview`
/// falls back to needs an `EXPLAIN` per segment, which is the round trip per row this exists to
/// avoid, and section 5's own measurement says a figure nobody should act on is worse than none.
pub async fn counts(
    pool: &PgPool,
    workspace_id: Uuid,
    segments: &[(Uuid, Group)],
    timeout_ms: u64,
) -> ApiResult<Vec<SegmentCount>> {
    let countable: Vec<&(Uuid, Group)> = segments
        .iter()
        .filter(|(_, group)| !filters_on_an_attribute(group))
        .collect();
    let mut counted: HashMap<Uuid, i64> = HashMap::new();
    if !countable.is_empty() {
        match count_many(pool, workspace_id, &countable, timeout_ms).await {
            Ok(rows) => counted = rows.into_iter().collect(),
            // Every row in the batch loses its number together: they were one statement.
            Err(e) if timed_out(&e) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(segments
        .iter()
        .map(|(id, _)| SegmentCount {
            id: *id,
            count: counted.get(id).copied(),
            exact: counted.contains_key(id),
        })
        .collect())
}

/// Every countable segment's count as one `union all`, under the same timeout a preview gets.
async fn count_many(
    pool: &PgPool,
    workspace_id: Uuid,
    countable: &[&(Uuid, Group)],
    timeout_ms: u64,
) -> sqlx::Result<Vec<(Uuid, i64)>> {
    let mut tx = pool.begin().await?;
    sqlx::query(timeout_statement(timeout_ms))
        .execute(&mut *tx)
        .await?;
    let mut q = QueryBuilder::<Postgres>::new("");
    for (i, (id, group)) in countable.iter().enumerate() {
        if i > 0 {
            q.push(" union all ");
        }
        // The cast names the column's type for the union; the id itself is bound, like every
        // other value the compiler emits.
        q.push("select ");
        q.push_bind(*id);
        q.push("::uuid, (select count(*) from contacts where workspace_id = ");
        q.push_bind(workspace_id);
        q.push(" and ");
        push_rules(&mut q, group);
        q.push(")");
    }
    let rows = q
        .build_query_as::<(Uuid, i64)>()
        .fetch_all(&mut *tx)
        .await?;
    tx.rollback().await?;
    Ok(rows)
}

fn answer(count: Option<i64>, exact: bool, started: Instant) -> Preview {
    Preview {
        count,
        exact,
        took_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

/// True if any rule anywhere in the tree names a custom attribute.
fn filters_on_an_attribute(group: &Group) -> bool {
    group.rules.iter().any(|node| match node {
        RuleNode::Rule(r) => matches!(r.field, Field::Attr { .. }),
        RuleNode::Group(g) => filters_on_an_attribute(g),
    })
}

/// The exact count, under the timeout. The transaction is what makes `SET LOCAL` mean anything;
/// it is rolled back either way, because nothing here writes.
async fn count(
    pool: &PgPool,
    workspace_id: Uuid,
    group: &Group,
    timeout_ms: u64,
) -> sqlx::Result<i64> {
    let mut tx = pool.begin().await?;
    sqlx::query(timeout_statement(timeout_ms))
        .execute(&mut *tx)
        .await?;
    let mut q =
        QueryBuilder::<Postgres>::new("select count(*) from contacts where workspace_id = ");
    q.push_bind(workspace_id).push(" and ");
    push_rules(&mut q, group);
    let n = q.build_query_scalar::<i64>().fetch_one(&mut *tx).await?;
    tx.rollback().await?;
    Ok(n)
}

/// The planner's estimate, in a **fresh** transaction: the one the count timed out in is aborted.
async fn estimate(
    pool: &PgPool,
    workspace_id: Uuid,
    group: &Group,
    timeout_ms: u64,
) -> ApiResult<i64> {
    let mut tx = pool.begin().await?;
    // Planning is not what was slow, but an unbounded statement on a pooled connection is the
    // thing this endpoint exists to avoid, so the fallback is capped too.
    sqlx::query(timeout_statement(timeout_ms))
        .execute(&mut *tx)
        .await?;
    let mut q = QueryBuilder::<Postgres>::new(
        // `select 1`, not `count(*)`: an aggregate's plan estimates one row at every node.
        "explain (format json) select 1 from contacts where workspace_id = ",
    );
    q.push_bind(workspace_id).push(" and ");
    push_rules(&mut q, group);
    let plan = q.build_query_scalar::<Value>().fetch_one(&mut *tx).await?;
    tx.rollback().await?;
    // The top node of the first (only) plan. Nested `Plans` are per worker and per scan.
    plan[0]["Plan"]["Plan Rows"].as_i64().ok_or_else(|| {
        ApiError::Internal(anyhow::anyhow!("no `Plan Rows` in the plan for a preview"))
    })
}

fn timed_out(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| code == QUERY_CANCELED)
}
