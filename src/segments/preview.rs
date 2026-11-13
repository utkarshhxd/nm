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
    rules::{Field, Group, RuleError, RuleNode},
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

/// What one row of a batch count is, and why.
///
/// A nullable number stood for three different facts here — no estimate is honest, the batch ran
/// out of time, the stored document no longer parses — and the list told all three of them with
/// section 5's sentence for the first. One state per fact, so the screen cannot say the wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CountState {
    /// Counted. `count` is the number of contacts that match.
    Exact,
    /// The planner's row estimate, after the count ran out of its budget. `count` is that figure.
    Estimated,
    /// No honest number: the document filters on a custom attribute, where section 6 ships no
    /// index and the planner's default selectivity measured 66x low. `count` is null, and this is
    /// the only state the list says "too many to count exactly" for.
    NoEstimate,
    /// The stored document no longer parses, so it cannot be compiled or counted. `count` is null
    /// and the segment needs re-saving. One such row no longer costs the other 24 their counts.
    Unreadable,
    /// We could not work out a number for this row **this time**: the batch count ran out of its
    /// budget and the fallback estimate failed too. `count` is null and a retry may well work.
    ///
    /// It is its own state because saying either of the two above would be a lie about a
    /// different thing — `no_estimate` says no honest number exists for this document, and
    /// `unreadable` says the document is broken. Neither is true of a planner that stumbled.
    Failed,
}

/// One segment's count in a batch. `Preview`'s `took_ms` has no meaning per row: the batch is one
/// statement and every row in it took the same time.
#[derive(Debug, Serialize, ToSchema)]
pub struct SegmentCount {
    pub id: Uuid,
    pub state: CountState,
    /// The number, for `exact` and `estimated`. Null for the other two states, which is what
    /// `state` is there to explain.
    pub count: Option<i64>,
}

impl SegmentCount {
    fn new(id: Uuid, state: CountState, count: Option<i64>) -> SegmentCount {
        SegmentCount { id, state, count }
    }
}

/// How many contacts each of `segments` matches, in **one** statement on **one** connection.
///
/// The list view counts every segment on the page. A preview request per row spends a workspace's
/// whole quota on a single page load — 30 a minute against 25 rows — which is what makes section
/// 19's lateral-join note urgent ahead of its latency argument.
///
/// Section 5 decides what a count may say, per row:
///
/// - A document filtering on a custom attribute is `no_estimate` without touching the database.
/// - A stored document that no longer parses is `unreadable`, and only that row.
/// - The rest are counted together. **If that statement runs out of time, each of them falls back
///   to the planner's estimate in a fresh transaction**, the way `preview` does — the timed-out
///   transaction is aborted and unusable. `EXPLAIN` is plan-only, so the round trip per row this
///   route exists to avoid is paid only on a page that was already too slow to count.
pub async fn counts(
    pool: &PgPool,
    workspace_id: Uuid,
    segments: &[(Uuid, Result<Group, RuleError>)],
    timeout_ms: u64,
) -> ApiResult<Vec<SegmentCount>> {
    let mut rows: HashMap<Uuid, SegmentCount> = HashMap::new();
    let mut countable: Vec<(Uuid, &Group)> = Vec::new();
    for (id, parsed) in segments {
        match parsed {
            Err(_) => {
                rows.insert(*id, SegmentCount::new(*id, CountState::Unreadable, None));
            }
            Ok(group) if filters_on_an_attribute(group) => {
                rows.insert(*id, SegmentCount::new(*id, CountState::NoEstimate, None));
            }
            Ok(group) => countable.push((*id, group)),
        }
    }

    if !countable.is_empty() {
        match count_many(pool, workspace_id, &countable, timeout_ms).await {
            Ok(counted) => {
                for (id, n) in counted {
                    rows.insert(id, SegmentCount::new(id, CountState::Exact, Some(n)));
                }
            }
            // One statement, so every countable row ran out of time together — and every one of
            // them gets its own estimate rather than the whole page losing its numbers.
            Err(e) if timed_out(&e) => {
                for (id, group) in &countable {
                    // Per row, never `?`. One segment whose `EXPLAIN` fails — it timed out too,
                    // the planner stumbled — used to turn the whole page's counts into a 500,
                    // which is the shape the parse path above had removed from it and this path
                    // kept. The row says `failed` and the other 24 keep their numbers.
                    let row = match estimate(pool, workspace_id, group, timeout_ms).await {
                        Ok(n) => SegmentCount::new(*id, CountState::Estimated, Some(n)),
                        Err(failure) => {
                            tracing::warn!(
                                segment_id = %id,
                                error = ?failure,
                                "the fallback estimate failed for one segment",
                            );
                            SegmentCount::new(*id, CountState::Failed, None)
                        }
                    };
                    rows.insert(*id, row);
                }
            }
            Err(e) => return Err(e.into()),
        }
    }

    // In the order asked for. Every id has a row: the three arms above are exhaustive.
    Ok(segments
        .iter()
        .filter_map(|(id, _)| rows.remove(id))
        .collect())
}

/// Every countable segment's count as one `union all`, under the same timeout a preview gets.
async fn count_many(
    pool: &PgPool,
    workspace_id: Uuid,
    countable: &[(Uuid, &Group)],
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
    plan_rows(&plan)
}

/// The estimate out of an `EXPLAIN (FORMAT JSON)` document: the **top** node of the first (and
/// only) plan.
///
/// Nested `Plans` are per worker and per scan, so a `Gather`'s child reports what **one** worker
/// expects — read it and the answer is short by the worker count. Pure, and pinned by a unit test
/// against a captured parallel plan, because a fixture that reliably plans in parallel is
/// expensive and moves with the planner's version.
fn plan_rows(plan: &Value) -> ApiResult<i64> {
    plan[0]["Plan"]["Plan Rows"].as_i64().ok_or_else(|| {
        ApiError::Internal(anyhow::anyhow!("no `Plan Rows` in the plan for a preview"))
    })
}

fn timed_out(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| code == QUERY_CANCELED)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::plan_rows;

    /// A real `EXPLAIN (FORMAT JSON)` of a parallel sequential scan, trimmed to the fields
    /// `plan_rows` reads. The `Gather` expects 10,000 rows; its child expects 4,167 — what **one**
    /// of two workers plus the leader produces. Reading the child under-reports by the worker
    /// count, which is the mistake section 5 records as measured.
    ///
    /// shortcut: a captured plan rather than a fixture that really plans in parallel. A live one
    /// needs a table large enough to beat `min_parallel_table_scan_size` and costs seconds per
    /// run, and which node the planner picks moves between Postgres versions. Recapture this from
    /// a real `EXPLAIN` the day the shape of a plan document changes.
    fn parallel_plan() -> serde_json::Value {
        json!([{
            "Plan": {
                "Node Type": "Gather",
                "Plan Rows": 10000,
                "Workers Planned": 2,
                "Plans": [{
                    "Node Type": "Seq Scan",
                    "Parent Relationship": "Outer",
                    "Parallel Aware": true,
                    "Plan Rows": 4167,
                }],
            },
        }])
    }

    #[test]
    fn the_estimate_is_the_top_node_not_the_per_worker_child() {
        let plan = parallel_plan();
        assert_eq!(plan_rows(&plan).expect("a plan with a top node"), 10_000);
        // The figure this must never return, stated so the assertion above cannot be satisfied
        // by a coincidence: the child really does carry a different number.
        assert_eq!(plan[0]["Plan"]["Plans"][0]["Plan Rows"], 4167);
    }

    #[test]
    fn a_plan_with_no_row_estimate_is_an_error_not_a_zero() {
        // `0` would read as "nothing matches", which is a number somebody would act on.
        assert!(plan_rows(&json!([{"Plan": {"Node Type": "Seq Scan"}}])).is_err());
        assert!(plan_rows(&json!([])).is_err());
    }
}
