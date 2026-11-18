// SPDX-License-Identifier: AGPL-3.0-only

//! An upload that is refused leaves nothing on disk. Its own test binary with a single test,
//! because it points the temp directory at a directory of its own and counts what is in it: in
//! a binary with other uploads running, that count would be a race.

mod common;

use axum::http::StatusCode;
use axum_test::multipart::{MultipartForm, Part};
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::workspaces::roles::Role;
use sqlx::PgPool;

fn left_behind(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("nm-import-"))
        .collect()
}

#[sqlx::test]
async fn a_refused_upload_leaves_no_file_and_an_accepted_one_leaves_one(pool: PgPool) {
    let dir = std::env::temp_dir().join(format!("nm-cleanup-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir(&dir).unwrap();
    // SAFETY: the only test in this binary, set before any request is made.
    unsafe { std::env::set_var("TMPDIR", &dir) };

    let owner = create_user(&pool, "owner@example.com", PW, false).await;
    let ws = create_workspace(&pool, owner, "acme").await;
    let ed = create_user(&pool, "ed@example.com", PW, false).await;
    add_member(&pool, ws, ed, Role::Editor).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login("ed@example.com", PW).await;
    let uri = format!("/api/v1/workspaces/{ws}/imports");
    let send = |form: MultipartForm| app.post(&uri).multipart(form);
    let file = |bytes: Vec<u8>| {
        MultipartForm::new().add_part("file", Part::bytes(bytes).file_name("a.csv"))
    };

    // Refused after the file was created and written: empty, and too wide.
    let r = send(file(vec![])).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        r.json::<serde_json::Value>()["type"],
        "urn:nm:problem:empty-file"
    );
    let wide = format!("{}\n", ",".repeat(1000));
    let r = send(file(wide.into_bytes())).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        r.json::<serde_json::Value>()["type"],
        "urn:nm:problem:too-many-columns"
    );
    // Refused before or while reading the body.
    send(MultipartForm::new().add_text("note", "x"))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    send(MultipartForm::new())
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(left_behind(&dir), Vec::<String>::new());
    let n: i64 = sqlx::query_scalar("select count(*) from imports")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);

    // Control: an accepted upload does leave its file, so the empty directory above is the
    // cleanup's doing and not a temp directory the handler never wrote to.
    let r = send(file(b"email\na@b.co\n".to_vec())).await;
    r.assert_status(StatusCode::CREATED);
    assert_eq!(left_behind(&dir).len(), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}
