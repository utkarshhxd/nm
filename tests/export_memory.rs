// SPDX-License-Identifier: AGPL-3.0-only

//! Memory does not grow with the row count (spec M1b section 10): measured, not inferred.
//!
//! A counting global allocator records the high-water mark of live heap bytes while an export is
//! drained, and the same export over ten times the rows must peak at about the same height. This
//! is the only test in the binary, so nothing else allocates concurrently and the mark is the
//! export's own. The body is read straight off the router (no `axum-test`, which collects every
//! response) and each chunk is dropped as it arrives, so a client is never what holds the book.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use common::{PW, TestApp, api_key, create_user, create_workspace, post_raw};
use futures_util::StreamExt;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to `System` unchanged; the counters only observe.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(now, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                let now = LIVE.fetch_add(new - l.size(), Relaxed) + (new - l.size());
                PEAK.fetch_max(now, Relaxed);
            } else {
                LIVE.fetch_sub(l.size() - new, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

async fn seed(pool: &PgPool, ws: Uuid, from: i64, to: i64) {
    sqlx::query(
        "insert into contacts (id, workspace_id, email, first_name, last_name) \
         select gen_random_uuid(), $1, 'person' || g || '@example.com', 'First' || g, 'Last' || g \
         from generate_series($2::bigint, $3::bigint) g",
    )
    .bind(ws)
    .bind(from)
    .bind(to)
    .execute(pool)
    .await
    .unwrap();
}

/// Drains one export, dropping every chunk; returns `(bytes, peak heap above where it started)`.
async fn drain(app: &TestApp, ws: Uuid, key: &str) -> (usize, usize) {
    let uri = format!("/api/v1/workspaces/{ws}/contacts/export");
    // A warm-up request first, so connection setup is not counted against the first measurement.
    let baseline = LIVE.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    let r = post_raw(&app.state, key, &uri, &json!({})).await;
    assert_eq!(r.status(), 200);
    let mut body = r.into_body().into_data_stream();
    let mut bytes = 0;
    while let Some(chunk) = body.next().await {
        bytes += chunk.unwrap().len();
    }
    (bytes, PEAK.load(Relaxed).saturating_sub(baseline))
}

#[sqlx::test]
async fn memory_does_not_grow_with_the_row_count(pool: PgPool) {
    let owner = create_user(&pool, "owner@example.com", PW, false).await;
    let ws = create_workspace(&pool, owner, "acme").await;
    let mut o = TestApp::new(pool.clone()).await;
    o.login("owner@example.com", PW).await;
    let key = api_key(&o, ws, "admin").await;

    seed(&pool, ws, 1, 5_000).await;
    drain(&o, ws, &key).await;
    let (small_bytes, small) = drain(&o, ws, &key).await;
    seed(&pool, ws, 5_001, 50_000).await;
    let (big_bytes, big) = drain(&o, ws, &key).await;

    eprintln!("5k rows: {small_bytes} B, peak +{small} B; 50k rows: {big_bytes} B, peak +{big} B");
    assert!(
        big_bytes > 9 * small_bytes,
        "the second export did not carry ten times the rows"
    );
    // Nine times the output and the peak did not move by more than allocator noise. The fixed part
    // (about 3.8 MB here: the router, the connection, the runtime) is the same in both and is not
    // what is being measured. A buffering export holds the whole result, which is 6.6 MB more.
    assert!(
        big < small + 256 * 1024,
        "peak heap grew with the result: {small} B at 5k rows, {big} B at 50k"
    );
}
