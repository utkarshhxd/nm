// SPDX-License-Identifier: AGPL-3.0-only

//! The schema M1c's importer writes to: `imports`, `import_errors`, and the `import` provenance.

mod common;

use common::{PW, create_user, create_workspace};
use nm::lists::MembershipSource;
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(pool: &PgPool, slug: &str) -> Uuid {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    create_workspace(pool, owner, slug).await
}

async fn insert_import(pool: &PgPool, ws: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into imports (id, workspace_id, status, filename, delimiter, has_header) values ($1, $2, 'pending', 'a.csv', 44, true)")
        .bind(id)
        .bind(ws)
        .execute(pool)
        .await
        .expect("insert import");
    id
}

async fn insert_error(pool: &PgPool, ws: Uuid, import: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "insert into import_errors (import_id, workspace_id, row_number, reason) values ($1, $2, 1, 'bad')",
    )
    .bind(import)
    .bind(ws)
    .execute(pool)
    .await
    .map(|_| ())
}

#[sqlx::test]
async fn an_import_error_cannot_name_another_workspaces_import(pool: PgPool) {
    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let import_of_a = insert_import(&pool, a).await;

    // Control: the owning workspace may write it, so the refusal below is the foreign key's.
    insert_error(&pool, a, import_of_a)
        .await
        .expect("own workspace");
    let err = insert_error(&pool, b, import_of_a)
        .await
        .expect_err("cross-tenant");
    assert!(
        err.as_database_error()
            .is_some_and(|e| e.is_foreign_key_violation()),
        "expected a foreign key violation, got {err}"
    );
    let n: i64 = sqlx::query_scalar("select count(*) from import_errors")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test]
async fn import_source_round_trips(pool: PgPool) {
    let ws = workspace(&pool, "a").await;
    let (contact, list) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("insert into contacts (id, workspace_id, email) values ($1, $2, 'x@example.com')")
        .bind(contact)
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("insert into lists (id, workspace_id, name) values ($1, $2, 'news')")
        .bind(list)
        .bind(ws)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "insert into list_memberships (workspace_id, contact_id, list_id, status, source) \
         values ($1, $2, $3, 'subscribed', $4)",
    )
    .bind(ws)
    .bind(contact)
    .bind(list)
    .bind(MembershipSource::Import)
    .execute(&pool)
    .await
    .unwrap();

    let got: MembershipSource = sqlx::query_scalar(
        "select source from list_memberships where contact_id = $1 and list_id = $2",
    )
    .bind(contact)
    .bind(list)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(got, MembershipSource::Import);
    let raw: String =
        sqlx::query_scalar("select source from list_memberships where contact_id = $1")
            .bind(contact)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(raw, "import");
}

#[sqlx::test]
async fn an_import_cannot_name_another_workspaces_list(pool: PgPool) {
    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let list_of_a = Uuid::now_v7();
    sqlx::query("insert into lists (id, workspace_id, name) values ($1, $2, 'news')")
        .bind(list_of_a)
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    let insert = |ws: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "insert into imports (id, workspace_id, status, filename, delimiter, has_header, list_id) \
                 values ($1, $2, 'pending', 'a.csv', 44, true, $3)",
            )
            .bind(Uuid::now_v7())
            .bind(ws)
            .bind(list_of_a)
            .execute(&pool)
            .await
        }
    };
    insert(a).await.expect("own workspace");
    let err = insert(b).await.expect_err("cross-tenant");
    assert!(
        err.as_database_error()
            .is_some_and(|e| e.is_foreign_key_violation()),
        "expected a foreign key violation, got {err}"
    );

    // Deleting the list keeps the import and nulls only list_id.
    sqlx::query("delete from lists where id = $1")
        .bind(list_of_a)
        .execute(&pool)
        .await
        .unwrap();
    let (n, nulls): (i64, i64) =
        sqlx::query_as("select count(*), count(*) filter (where list_id is null) from imports where workspace_id = $1")
            .bind(a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((n, nulls), (1, 1));
}

// ---- Task 3: the upload and the mapping proposal --------------------------------------------

use axum::http::StatusCode;
use axum_test::multipart::{MultipartForm, Part};
use common::{TestApp, add_member};
use nm::{
    contacts::import::{
        ColumnTarget, ContactField,
        sniff::{Sniffed, sniff},
        upload_path,
    },
    custom_fields::Definition,
    workspaces::roles::Role,
};
use serde_json::{Value, json};

async fn member(pool: &PgPool, ws: Uuid, email: &str, role: Role) -> TestApp {
    let id = create_user(pool, email, PW, false).await;
    add_member(pool, ws, id, role).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login(email, PW).await;
    app
}

fn imports_uri(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/imports")
}

async fn upload(app: &TestApp, ws: Uuid, name: &str, body: &str) -> axum_test::TestResponse {
    let part = Part::bytes(body.as_bytes().to_vec()).file_name(name);
    app.post(&imports_uri(ws))
        .multipart(MultipartForm::new().add_part("file", part))
        .await
}

/// Uploads `body`, expects 201, and removes the stored file; returns the response body.
async fn uploaded(app: &TestApp, ws: Uuid, name: &str, body: &str) -> Value {
    let r = upload(app, ws, name, body).await;
    r.assert_status(StatusCode::CREATED);
    let v: Value = r.json();
    std::fs::remove_file(upload_path(v["id"].as_str().unwrap().parse().unwrap())).unwrap();
    v
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    let sql = match table {
        "contacts" => "select count(*) from contacts",
        "imports" => "select count(*) from imports",
        _ => unreachable!(),
    };
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

fn proposals(body: &Value) -> Vec<Value> {
    body["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["proposed"].clone())
        .collect()
}

#[sqlx::test]
async fn upload_returns_a_mapping_proposal_and_writes_no_contacts(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    // A hostile filename: it must become a label, never a path.
    let csv = "id,Email,first_name,notes\r\n1,ada@example.com,'=SUM(A1),hello\r\n";
    let r = upload(&app, ws, "../../etc/passwd.csv", csv).await;
    r.assert_status(StatusCode::CREATED);
    let body: Value = r.json();

    assert_eq!(body["delimiter"], ",");
    assert_eq!(body["has_header"], true);
    assert_eq!(body["total_bytes"], csv.len());
    assert_eq!(
        proposals(&body),
        [
            json!({"kind": "ignore"}),
            json!({"kind": "field", "value": "email"}),
            json!({"kind": "field", "value": "first_name"}),
            json!({"kind": "ignore"}),
        ]
    );
    // A sample reads as the spreadsheet shows it, without the exporter's leading apostrophe.
    assert_eq!(body["columns"][2]["samples"], json!(["=SUM(A1)"]));

    let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    // The file is where the id says, whatever the client called it.
    let path = upload_path(id);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), csv);
    std::fs::remove_file(&path).unwrap();

    let (status, filename): (String, String) =
        sqlx::query_as("select status, filename from imports where id = $1 and workspace_id = $2")
            .bind(id)
            .bind(ws)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (status.as_str(), filename.as_str()),
        ("pending", "passwd.csv")
    );
    assert_eq!(count(&pool, "contacts").await, 0);
    let audited: i64 = sqlx::query_scalar(
        "select count(*) from audit_log where action = 'import.uploaded' and target_id = $1",
    )
    .bind(id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);
}

#[sqlx::test]
async fn a_tab_separated_file_is_detected(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let csv = "email\tfirst_name\tlast_name\nada@example.com\tAda\tLovelace, Countess\n";
    let body = uploaded(&app, ws, "book.tsv", csv).await;
    assert_eq!(body["delimiter"], "\t");
    assert_eq!(body["columns"].as_array().unwrap().len(), 3);
    assert_eq!(body["columns"][2]["samples"], json!(["Lovelace, Countess"]));
    // What the operator was shown is what is stored, so the run cannot sniff differently.
    let (delimiter, has_header): (i16, bool) =
        sqlx::query_as("select delimiter, has_header from imports where id = $1")
            .bind(body["id"].as_str().unwrap().parse::<Uuid>().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((delimiter, has_header), (9, true));
}

#[sqlx::test]
async fn a_column_named_attr_colour_proposes_that_custom_field(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Admin).await;
    for (key, label) in [("colour", "Colour"), ("size", "Size")] {
        app.post(&format!("/api/v1/workspaces/{ws}/custom-fields"))
            .json(&json!({"key": key, "label": label, "kind": "text"}))
            .await
            .assert_status(StatusCode::CREATED);
    }
    let csv = "email,attr:colour,SIZE\nada@example.com,red,L\n";
    let body = uploaded(&app, ws, "c.csv", csv).await;
    assert_eq!(
        proposals(&body),
        [
            json!({"kind": "field", "value": "email"}),
            json!({"kind": "custom", "value": "colour"}),
            json!({"kind": "custom", "value": "size"}),
        ]
    );
}

#[sqlx::test]
async fn an_unrecognised_column_proposes_nothing(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let csv = "Email Address,Given Name,attr:ghost,id,suppressed\nada@example.com,Ada,x,1,true\n";
    let body = uploaded(&app, ws, "u.csv", csv).await;
    assert!(
        proposals(&body)
            .iter()
            .all(|p| *p == json!({"kind": "ignore"}))
    );
}

#[sqlx::test]
async fn another_workspaces_import_is_404(pool: PgPool) {
    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let app = member(&pool, a, "ed@example.com", Role::Editor).await;
    let viewer = member(&pool, a, "v@example.com", Role::Viewer).await;
    let csv = "email\nada@example.com\n";

    // Control: its own workspace takes the upload, so the refusals below are the tenancy's.
    uploaded(&app, a, "x.csv", csv).await;
    assert_eq!(count(&pool, "imports").await, 1);

    // Not 403, and nothing is recorded for a workspace that is not theirs.
    let r = upload(&app, b, "x.csv", csv).await;
    r.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(r.json::<Value>()["type"], "urn:nm:problem:not-found");
    assert_eq!(count(&pool, "imports").await, 1);

    // A member who may not import is told so: they already know the workspace exists.
    upload(&viewer, a, "x.csv", csv)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    assert_eq!(count(&pool, "imports").await, 1);
}

fn def(key: &str, label: &str, archived: bool) -> Definition {
    Definition {
        id: Uuid::now_v7(),
        key: key.to_owned(),
        label: label.to_owned(),
        kind: nm::custom_fields::FieldKind::Text,
        options: vec![],
        archived_at: archived.then(chrono::Utc::now),
    }
}

fn proposed(s: &Sniffed) -> Vec<ColumnTarget> {
    s.columns.iter().map(|c| c.proposed.clone()).collect()
}

#[test]
fn a_target_two_columns_claim_goes_to_neither() {
    let s = sniff("email,Email,first_name\na@b.co,c@d.co,Ada\n", &[]);
    assert_eq!(
        proposed(&s),
        [
            ColumnTarget::Ignore,
            ColumnTarget::Ignore,
            ColumnTarget::Field(ContactField::FirstName)
        ]
    );
}

#[test]
fn an_ambiguous_name_proposes_nothing_and_an_archived_field_is_never_proposed() {
    // A label that is also a built-in's name: unsure, so blank. An archived field takes no data.
    let defs = [def("mail", "Email", false), def("old", "Old", true)];
    let s = sniff("email,old\na@b.co,x\n", &defs);
    assert_eq!(proposed(&s), [ColumnTarget::Ignore, ColumnTarget::Ignore]);
}

#[test]
fn a_semicolon_file_with_decimal_commas_is_read_as_semicolons() {
    let s = sniff(
        "email;first_name;score\na@b.co;Ada;1,5\nc@d.co;Bo;2,5\n",
        &[],
    );
    assert_eq!(s.delimiter, b';');
    assert_eq!(s.columns.len(), 3);
}

#[test]
fn a_file_without_a_header_row_proposes_nothing_and_a_bom_is_not_part_of_a_name() {
    let s = sniff("a@b.co,Ada\nc@d.co,Bo\n", &[]);
    assert!(!s.has_header);
    assert_eq!(s.columns[0].name, "Column 1");
    assert_eq!(s.columns[0].samples, ["a@b.co", "c@d.co"]);
    assert!(proposed(&s).iter().all(|p| *p == ColumnTarget::Ignore));

    let s = sniff("\u{feff}email,first_name\na@b.co,Ada\n", &[]);
    assert_eq!(s.columns[0].name, "email");
    assert_eq!(
        s.columns[0].proposed,
        ColumnTarget::Field(ContactField::Email)
    );
}

#[test]
fn only_the_first_hundred_rows_are_read_and_an_empty_head_has_no_columns() {
    let mut csv = String::from("email,n\n");
    for i in 0..300 {
        // Blank for the first 100 rows after the header, so a sample can only come from later.
        let n = if i < 100 { "" } else { "late" };
        csv.push_str(&format!("u{i}@b.co,{n}\n"));
    }
    let s = sniff(&csv, &[]);
    assert!(s.columns[1].samples.is_empty());
    assert_eq!(sniff("", &[]).columns.len(), 0);
}

#[test]
fn a_header_is_matched_after_lowercasing_and_dropping_punctuation_and_nothing_more() {
    let s = sniff(
        "First Name,LAST_NAME,e-mail,Email Address,time zone\nAda,Lovelace,a@b.co,a@b.co,UTC\n",
        &[],
    );
    assert_eq!(
        proposed(&s),
        [
            ColumnTarget::Field(ContactField::FirstName),
            ColumnTarget::Field(ContactField::LastName),
            // `e-mail` is `email`; `Email Address` is a different word and must stay blank.
            ColumnTarget::Field(ContactField::Email),
            ColumnTarget::Ignore,
            ColumnTarget::Field(ContactField::Timezone),
        ]
    );
}

#[test]
fn a_custom_key_that_normalises_onto_a_built_in_blanks_both_and_attr_keeps_its_key() {
    // The built-in and the custom field both read as `firstname`: unsure, so blank.
    let defs = [
        def("first_name", "First name", false),
        def("colour", "Colour", false),
    ];
    let s = sniff("First Name,attr:colour,attr:first_name\nAda,red,x\n", &defs);
    assert_eq!(
        proposed(&s),
        [
            ColumnTarget::Ignore,
            ColumnTarget::Custom("colour".to_owned()),
            // `attr:` names a custom key only, so this one is certain even though the plain
            // header above is not.
            ColumnTarget::Custom("first_name".to_owned()),
        ]
    );
}

#[sqlx::test]
async fn a_three_megabyte_upload_is_accepted(pool: PgPool) {
    // axum's own multipart limit is 2 MB; the route raises it. If that layer is lost every real
    // upload is a 413 and nothing else here would notice.
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let mut csv = String::from("email,first_name\n");
    let mut i = 0;
    while csv.len() < 3 * 1024 * 1024 {
        csv.push_str(&format!("user{i}@example.com,Ada\n"));
        i += 1;
    }
    let body = uploaded(&app, ws, "big.csv", &csv).await;
    assert_eq!(body["total_bytes"], csv.len());
    // Sniffed from the head only, so the answer is the same as for a small file.
    assert_eq!(body["columns"].as_array().unwrap().len(), 2);
    assert_eq!(body["columns"][0]["samples"].as_array().unwrap().len(), 5);
}

// ---- Task 4: COPY into staging -----------------------------------------------------------------

use nm::{
    config::SecretKey,
    contacts::import::{
        Dedup, ImportJob,
        staging::{StagingCounts, copy_into_staging, staging_table},
    },
    crypto::{secrets::Secrets, suppression_hmac},
    custom_fields::FieldKind,
    users::normalize_email,
};
use sqlx::{AssertSqlSafe, PgConnection};

fn secrets() -> Secrets {
    Secrets::new(&SecretKey([7u8; 32]))
}

fn field(key: &str, kind: FieldKind) -> Definition {
    Definition {
        id: Uuid::now_v7(),
        key: key.to_owned(),
        label: key.to_owned(),
        kind,
        options: vec![],
        archived_at: None,
    }
}

fn job(id: Uuid, ws: Uuid, mapping: Vec<ColumnTarget>, defs: Vec<Definition>) -> ImportJob {
    ImportJob {
        id,
        workspace_id: ws,
        path: upload_path(id),
        delimiter: b',',
        has_header: true,
        mapping,
        dedup: Dedup::default(),
        list_id: None,
        defs,
    }
}

fn email_column() -> Vec<ColumnTarget> {
    vec![ColumnTarget::Field(ContactField::Email)]
}

async fn copy(conn: &mut PgConnection, j: &ImportJob, csv: &[u8]) -> StagingCounts {
    copy_into_staging(conn, &secrets(), j, csv)
        .await
        .expect("an unreadable row is an error row, never a failed call")
}

/// `row_number` and `email` of everything staged, in file order.
async fn staged(conn: &mut PgConnection, import: Uuid) -> Vec<(i64, String)> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select row_number, email from {} order by row_number",
        staging_table(import)
    )))
    .fetch_all(conn)
    .await
    .expect("read staging")
}

async fn error_rows(
    conn: &mut PgConnection,
    import: Uuid,
) -> Vec<(i64, Option<String>, String, Option<String>)> {
    sqlx::query_as(
        "select row_number, column_name, reason, value from import_errors \
         where import_id = $1 order by row_number",
    )
    .bind(import)
    .fetch_all(conn)
    .await
    .expect("read import_errors")
}

#[sqlx::test]
async fn every_staged_id_is_uuid_v7(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let j = job(import, ws, email_column(), vec![]);
    let mut tx = pool.begin().await.unwrap();

    let counts = copy(&mut tx, &j, b"email\na@b.co\nc@d.co\n").await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (2, 2, 0));

    let ids: Vec<Uuid> = sqlx::query_scalar(AssertSqlSafe(format!(
        "select id from {} order by row_number",
        staging_table(import)
    )))
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(ids.len(), 2);
    for id in &ids {
        // A v4 parses, and `gen_random_uuid()` would too: the version nibble is the assertion. A
        // selection snapshot is `id <= as_of`, so an id that is not time-ordered escapes it.
        assert_eq!(id.get_version_num(), 7, "{id} is not a UUIDv7");
    }
    assert!(ids[0] < ids[1], "v7 ids rise with the rows that made them");
}

#[sqlx::test]
async fn the_staged_address_is_what_normalize_email_returns(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let j = job(import, ws, email_column(), vec![]);
    let mut tx = pool.begin().await.unwrap();

    // Untrimmed, uppercase and decomposed: three spellings the application folds into one.
    let raw = " JOSE\u{301}@Example.COM ";
    copy(&mut tx, &j, format!("email\n{raw}\n").as_bytes()).await;

    let (email, hmac): (String, Vec<u8>) = sqlx::query_as(AssertSqlSafe(format!(
        "select email, email_hmac from {}",
        staging_table(import)
    )))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(email, normalize_email(raw).unwrap());
    assert_eq!(email, "jos\u{e9}@example.com");
    // The erasure anti-join is only an anti-join if this is the hash `suppressions` stores.
    assert_eq!(hmac, suppression_hmac(&secrets(), &email).to_vec());
}

#[sqlx::test]
async fn a_row_that_is_not_utf8_is_an_error_row_naming_its_line(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mapping = vec![
        ColumnTarget::Field(ContactField::Email),
        ColumnTarget::Field(ContactField::FirstName),
    ];
    let j = job(import, ws, mapping, vec![]);
    let mut tx = pool.begin().await.unwrap();

    // Latin-1 out of someone's old system. The operator must learn that, not read "internal error".
    let counts = copy(&mut tx, &j, b"email,first_name\na@b.co,Ada\nc@d.co,Jos\xe9\n").await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (2, 1, 1));

    let errors = error_rows(&mut tx, import).await;
    assert_eq!(errors.len(), 1);
    let (row, column, reason, value) = &errors[0];
    assert_eq!(*row, 3, "the header is row 1, so the second data row is 3");
    assert_eq!(column.as_deref(), Some("first_name"));
    assert!(reason.contains("UTF-8"), "{reason}");
    assert_eq!(value.as_deref(), Some("Jos\u{fffd}"));
}

#[sqlx::test]
async fn a_lone_carriage_return_does_not_start_a_record(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mapping = vec![
        ColumnTarget::Field(ContactField::Email),
        ColumnTarget::Field(ContactField::Tags),
    ];
    let j = job(import, ws, mapping, vec![]);
    let mut tx = pool.begin().await.unwrap();

    // CRLF endings, and a quoted cell holding a bare CR: neither may end a record early or add an
    // empty one. A file read that way reports twice its rows and fails half of them.
    let counts = copy(&mut tx, &j, b"email,tags\r\na@b.co,\"x\ry\"\r\nc@d.co,\r\n").await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (2, 2, 0));
    assert_eq!(
        staged(&mut tx, import).await,
        [(2, "a@b.co".to_owned()), (3, "c@d.co".to_owned())]
    );
    let tags: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "select tags from {} where row_number = 2",
        staging_table(import)
    )))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(tags, json!(["x\ry"]));
}

#[sqlx::test]
async fn minus_five_minus_five_into_a_number_field_is_an_error_row(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let defs = vec![field("score", FieldKind::Number)];
    let mapping = vec![
        ColumnTarget::Field(ContactField::Email),
        ColumnTarget::Custom("score".to_owned()),
    ];
    let j = job(import, ws, mapping, defs);
    let mut tx = pool.begin().await.unwrap();

    let counts = copy(&mut tx, &j, b"email,attr:score\na@b.co,-5-5\nc@d.co,7\n").await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (2, 1, 1));
    assert_eq!(staged(&mut tx, import).await, [(3, "c@d.co".to_owned())]);

    let errors = error_rows(&mut tx, import).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 2);
    assert_eq!(errors[0].1.as_deref(), Some("attr:score"));
    assert_eq!(errors[0].3.as_deref(), Some("-5-5"));
}

#[sqlx::test]
async fn one_e_999_into_a_number_field_is_an_error_row(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let defs = vec![field("score", FieldKind::Number)];
    let mapping = vec![
        ColumnTarget::Field(ContactField::Email),
        ColumnTarget::Custom("score".to_owned()),
    ];
    let j = job(import, ws, mapping, defs);
    let mut tx = pool.begin().await.unwrap();

    // Parses as a float, to an infinity: not a JSON number, and not a quantity anyone meant.
    let counts = copy(&mut tx, &j, b"email,attr:score\na@b.co,1e999\nc@d.co,1e3\n").await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (2, 1, 1));

    let attributes: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "select attributes from {} where row_number = 3",
        staging_table(import)
    )))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(attributes, json!({"score": 1000.0}));
    assert_eq!(error_rows(&mut tx, import).await[0].0, 2);
}

#[sqlx::test]
async fn a_bad_row_does_not_stop_the_good_rows_after_it(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let j = job(import, ws, email_column(), vec![]);
    let mut tx = pool.begin().await.unwrap();

    let counts = copy(&mut tx, &j, b"email\na@b.co\nnot-an-email\nc@d.co\n").await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (3, 2, 1));
    assert_eq!(
        staged(&mut tx, import).await,
        [(2, "a@b.co".to_owned()), (4, "c@d.co".to_owned())],
        "the row after the bad one is staged, and keeps its own line number"
    );
    let errors = error_rows(&mut tx, import).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 3);
    assert_eq!(errors[0].3.as_deref(), Some("not-an-email"));
}

#[sqlx::test]
async fn a_row_wider_than_its_header_is_an_error_row_but_a_trailing_delimiter_is_not(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mapping = vec![
        ColumnTarget::Field(ContactField::Email),
        ColumnTarget::Field(ContactField::FirstName),
    ];
    let j = job(import, ws, mapping, vec![]);
    let mut tx = pool.begin().await.unwrap();

    // Row 2 holds a value with an unquoted comma in it, so every cell after it may be the wrong
    // cell; row 3 ends in a delimiter, which says nothing; row 4 is short, which is a blank cell.
    let counts = copy(
        &mut tx,
        &j,
        b"email,first_name\na@b.co,Lovelace,Countess\nb@b.co,Bo,\nc@d.co\n",
    )
    .await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (3, 2, 1));
    assert_eq!(
        staged(&mut tx, import).await,
        [(3, "b@b.co".to_owned()), (4, "c@d.co".to_owned())]
    );
    let names: Vec<Option<String>> = sqlx::query_scalar(AssertSqlSafe(format!(
        "select first_name from {} order by row_number",
        staging_table(import)
    )))
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    // A blank cell is not a null to write: it is the file saying nothing about the field.
    assert_eq!(names, [Some("Bo".to_owned()), None]);
    let errors = error_rows(&mut tx, import).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 2);
    assert!(errors[0].2.contains("3 fields"), "{}", errors[0].2);
}

// ---- Task 4, fix round 1: the parser's two growth paths, and their caps ------------------------

use nm::contacts::import::{routes::MAX_IMPORT_COLUMNS, staging::MAX_RECORD_BYTES};

#[sqlx::test]
async fn a_row_with_more_fields_than_the_column_cap_is_refused_and_the_rows_after_it_stage(
    pool: PgPool,
) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let j = job(import, ws, email_column(), vec![]);
    let mut tx = pool.begin().await.unwrap();

    // Past the cap the parser throws the record away as it reads it, so the offsets it would have
    // needed are never allocated. The rows after it must still be read out of the same stream.
    let wide = "x,".repeat(MAX_IMPORT_COLUMNS + 100);
    let csv = format!("email\na@b.co\n{wide}\nc@d.co\n");
    let counts = copy(&mut tx, &j, csv.as_bytes()).await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (3, 2, 1));
    assert_eq!(
        staged(&mut tx, import).await,
        [(2, "a@b.co".to_owned()), (4, "c@d.co".to_owned())]
    );

    let errors = error_rows(&mut tx, import).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 3);
    assert!(
        errors[0].2.contains(&format!("more than {MAX_IMPORT_COLUMNS} fields")),
        "{}",
        errors[0].2
    );
}

#[sqlx::test]
async fn a_cell_larger_than_the_parsers_buffer_stages_intact(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mapping = vec![
        ColumnTarget::Field(ContactField::Email),
        ColumnTarget::Field(ContactField::Tags),
    ];
    let j = job(import, ws, mapping, vec![]);
    let mut tx = pool.begin().await.unwrap();

    // Over the 8 KiB the output buffer starts at, so the record only parses if the buffer grows
    // and the offsets carry across the grow. Under `MAX_RECORD_BYTES`, so it is a legitimate row.
    let big = "t".repeat(20_000);
    let csv = format!("email,tags\na@b.co,{big}\nc@d.co,x\n");
    let counts = copy(&mut tx, &j, csv.as_bytes()).await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (2, 2, 0));

    let tags: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "select tags from {} where row_number = 2",
        staging_table(import)
    )))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(tags, json!([big]), "the big cell survives the grow intact");
}

#[sqlx::test]
async fn a_row_over_the_record_byte_cap_is_refused_for_its_length_not_its_width(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mapping = vec![
        ColumnTarget::Field(ContactField::Email),
        ColumnTarget::Field(ContactField::Tags),
    ];
    let j = job(import, ws, mapping, vec![]);
    let mut tx = pool.begin().await.unwrap();

    // One record over the cap: without it the buffer would double until it held the rest of the
    // file, which an unclosed quote makes as big as the upload. The row is refused for its
    // LENGTH — a row that is too long and a row that is too wide send the operator to different
    // places in their file — and the row after it still stages, from the same stream.
    let huge = "z".repeat(MAX_RECORD_BYTES + 1024);
    let csv = format!("email,tags\na@b.co,{huge}\nc@d.co,x\n");
    let counts = copy(&mut tx, &j, csv.as_bytes()).await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (2, 1, 1));
    assert_eq!(staged(&mut tx, import).await, [(3, "c@d.co".to_owned())]);

    let errors = error_rows(&mut tx, import).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 2);
    let reason = &errors[0].2;
    assert!(reason.contains("longer than"), "{reason}");
    assert!(
        !reason.contains("columns"),
        "a long row must not be reported as a wide one: {reason}"
    );
}

// ---- Task 5: the set-based upsert ---------------------------------------------------------------

use nm::{
    contacts::import::staging::{UpsertCounts, upsert_from_staging},
    error::ApiError,
};

fn job_with(
    id: Uuid,
    ws: Uuid,
    mapping: Vec<ColumnTarget>,
    defs: Vec<Definition>,
    dedup: Dedup,
) -> ImportJob {
    ImportJob {
        dedup,
        ..job(id, ws, mapping, defs)
    }
}

/// `email, first_name, last_name, timezone` in that order: the mapping most of these tests use.
fn name_columns() -> Vec<ColumnTarget> {
    vec![
        ColumnTarget::Field(ContactField::Email),
        ColumnTarget::Field(ContactField::FirstName),
        ColumnTarget::Field(ContactField::LastName),
        ColumnTarget::Field(ContactField::Timezone),
    ]
}

/// The whole pipeline over one in-memory file: `COPY` into staging, then the upsert.
async fn import_csv(conn: &mut PgConnection, j: &ImportJob, csv: &str) -> UpsertCounts {
    let staged = copy(conn, j, csv.as_bytes()).await;
    upsert_from_staging(conn, j, staged)
        .await
        .expect("the upsert")
}

/// One contact as the upsert left it.
async fn contact(
    conn: &mut PgConnection,
    ws: Uuid,
    email: &str,
) -> Option<(String, String, Option<String>, Value)> {
    sqlx::query_as(
        "select first_name, last_name, timezone, attributes from contacts \
         where workspace_id = $1 and email = $2",
    )
    .bind(ws)
    .bind(email)
    .fetch_optional(conn)
    .await
    .expect("read contact")
}

async fn insert_contact(conn: &mut PgConnection, ws: Uuid, email: &str, first: &str, last: &str) {
    sqlx::query(
        "insert into contacts (id, workspace_id, email, first_name, last_name, timezone, attributes) \
         values ($1, $2, $3, $4, $5, 'Europe/Berlin', '{\"plan\": \"pro\"}'::jsonb)",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(email)
    .bind(first)
    .bind(last)
    .execute(conn)
    .await
    .expect("insert contact");
}

/// The GDPR tombstone: the hash and no plaintext, with a `note` and a `source` that no refusal may
/// ever echo back.
async fn erase(conn: &mut PgConnection, ws: Option<Uuid>, email: &str) {
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email_hmac, reason, source, note) \
         values ($1, $2, 'address', $3, 'gdpr_erasure', 'SOURCE-SECRET', 'NOTE-SECRET')",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(suppression_hmac(&secrets(), email).as_slice())
    .execute(conn)
    .await
    .expect("erase");
}

#[sqlx::test]
async fn update_existing_leaves_a_blank_cell_alone(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let j = job_with(import, ws, name_columns(), vec![], Dedup::UpdateExisting);
    let mut tx = pool.begin().await.unwrap();
    insert_contact(&mut tx, ws, "ada@example.com", "Ada", "Lovelace").await;

    // A blank `first_name`, a given `last_name`, and a `timezone` whose cell is blank too:
    // decision 4 says only the second of the three may move.
    let counts = import_csv(
        &mut tx,
        &j,
        "email,first_name,last_name,timezone\nada@example.com,,Byron,\n",
    )
    .await;
    assert_eq!(
        counts,
        UpsertCounts {
            imported: 0,
            updated: 1,
            skipped: 0,
            collapsed: 0,
            failed: 0
        }
    );

    let got = contact(&mut tx, ws, "ada@example.com").await.unwrap();
    assert_eq!(got.0, "Ada", "a blank cell may not clear a stored name");
    assert_eq!(got.1, "Byron", "a cell with a value in it still writes");
    assert_eq!(got.2.as_deref(), Some("Europe/Berlin"));
    assert_eq!(got.3, json!({"plan": "pro"}), "an unnamed key is untouched");
}

#[sqlx::test]
async fn skip_existing_writes_nothing_and_counts_it(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let j = job_with(import, ws, name_columns(), vec![], Dedup::SkipExisting);
    let mut tx = pool.begin().await.unwrap();
    insert_contact(&mut tx, ws, "ada@example.com", "Ada", "Lovelace").await;

    let counts = import_csv(
        &mut tx,
        &j,
        "email,first_name,last_name,timezone\n\
         ada@example.com,Grace,Hopper,UTC\n\
         bob@example.com,Bob,Barker,UTC\n",
    )
    .await;
    assert_eq!(
        counts,
        UpsertCounts {
            imported: 1,
            updated: 0,
            skipped: 1,
            collapsed: 0,
            failed: 0
        }
    );

    let ada = contact(&mut tx, ws, "ada@example.com").await.unwrap();
    assert_eq!((ada.0.as_str(), ada.1.as_str()), ("Ada", "Lovelace"));
    assert!(contact(&mut tx, ws, "bob@example.com").await.is_some());
}

#[sqlx::test]
async fn fail_on_duplicate_aborts_before_any_write(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let j = job_with(import, ws, name_columns(), vec![], Dedup::FailOnDuplicate);
    let mut tx = pool.begin().await.unwrap();
    insert_contact(&mut tx, ws, "ada@example.com", "Ada", "Lovelace").await;

    // The new address comes FIRST in the file, so a per-row importer would already have written it
    // by the time it reached the duplicate.
    let staged = copy(
        &mut tx,
        &j,
        b"email,first_name,last_name,timezone\n\
          new@example.com,New,Person,UTC\n\
          ada@example.com,Grace,Hopper,UTC\n",
    )
    .await;
    let err = upsert_from_staging(&mut tx, &j, staged)
        .await
        .expect_err("a duplicate must abort the import");
    let ApiError::ConflictDetail { code, detail } = &err else {
        panic!("expected a conflict naming the address, got {err:?}");
    };
    assert_eq!(*code, "import-duplicate-address");
    assert!(detail.contains("ada@example.com"), "{detail}");

    assert!(
        contact(&mut tx, ws, "new@example.com").await.is_none(),
        "the abort happens before any write, so the earlier row is not in the database"
    );
    let ada = contact(&mut tx, ws, "ada@example.com").await.unwrap();
    assert_eq!(ada.0, "Ada");
}

#[sqlx::test]
async fn two_rows_with_the_same_address_keep_the_last_and_count_the_collapse(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let j = job_with(import, ws, name_columns(), vec![], Dedup::UpdateExisting);
    let mut tx = pool.begin().await.unwrap();

    let counts = import_csv(
        &mut tx,
        &j,
        "email,first_name,last_name,timezone\n\
         ada@example.com,First,Draft,UTC\n\
         ada@example.com,Last,Word,UTC\n",
    )
    .await;
    assert_eq!(
        counts,
        UpsertCounts {
            imported: 1,
            updated: 0,
            skipped: 0,
            collapsed: 1,
            failed: 0
        },
        "a within-file collapse is not a skip: the database never had the row"
    );

    let got = contact(&mut tx, ws, "ada@example.com").await.unwrap();
    assert_eq!((got.0.as_str(), got.1.as_str()), ("Last", "Word"));
}

#[sqlx::test]
async fn an_erased_address_is_never_recreated_by_an_import(pool: PgPool) {
    let (ws, other) = (
        workspace(&pool, "acme").await,
        workspace(&pool, "other").await,
    );
    let import = insert_import(&pool, ws).await;
    let j = job_with(import, ws, name_columns(), vec![], Dedup::UpdateExisting);
    let mut tx = pool.begin().await.unwrap();
    erase(&mut tx, Some(ws), "ada@example.com").await;
    erase(&mut tx, None, "ins@example.com").await;
    // Another workspace's erasure is not this workspace's business, and must not block this row.
    erase(&mut tx, Some(other), "theirs@example.com").await;

    let counts = import_csv(
        &mut tx,
        &j,
        "email,first_name,last_name,timezone\n\
         ada@example.com,Ada,Lovelace,UTC\n\
         ins@example.com,Ins,Tance,UTC\n\
         theirs@example.com,Their,Erasure,UTC\n\
         bob@example.com,Bob,Barker,UTC\n",
    )
    .await;
    assert_eq!(
        counts,
        UpsertCounts {
            imported: 2,
            updated: 0,
            skipped: 0,
            collapsed: 0,
            failed: 2
        }
    );

    assert!(contact(&mut tx, ws, "ada@example.com").await.is_none());
    assert!(contact(&mut tx, ws, "ins@example.com").await.is_none());
    assert!(contact(&mut tx, ws, "theirs@example.com").await.is_some());
    assert!(contact(&mut tx, ws, "bob@example.com").await.is_some());
}

#[sqlx::test]
async fn an_erased_address_is_never_updated_either(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let j = job_with(import, ws, name_columns(), vec![], Dedup::UpdateExisting);
    let mut tx = pool.begin().await.unwrap();
    // A contact the tombstone did not take with it. Erasing deletes the contact, so this is a
    // state only a bug upstream can reach — which is the point: the anti-join in the insert is
    // what refuses it, and the delete that runs last cannot undo an UPDATE to a row this import
    // did not create.
    insert_contact(&mut tx, ws, "ada@example.com", "Ada", "Lovelace").await;
    erase(&mut tx, Some(ws), "ada@example.com").await;

    let counts = import_csv(
        &mut tx,
        &j,
        "email,first_name,last_name,timezone\nada@example.com,Grace,Hopper,UTC\n",
    )
    .await;
    assert_eq!(
        counts,
        UpsertCounts {
            imported: 0,
            updated: 0,
            skipped: 0,
            collapsed: 0,
            failed: 1
        }
    );
    let got = contact(&mut tx, ws, "ada@example.com").await.unwrap();
    assert_eq!(
        (got.0.as_str(), got.1.as_str()),
        ("Ada", "Lovelace"),
        "an erased address is not written to, whether that write would be an insert or an update"
    );
}

#[sqlx::test]
async fn an_erasure_is_reported_without_echoing_anything_but_the_address(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let j = job_with(import, ws, name_columns(), vec![], Dedup::UpdateExisting);
    let mut tx = pool.begin().await.unwrap();
    erase(&mut tx, Some(ws), "ada@example.com").await;

    import_csv(
        &mut tx,
        &j,
        "email,first_name,last_name,timezone\nada@example.com,Ada,Lovelace,UTC\n",
    )
    .await;

    let errors = error_rows(&mut tx, import).await;
    assert_eq!(errors.len(), 1);
    let (row, column, reason, value) = errors[0].clone();
    assert_eq!(row, 2, "the line a spreadsheet shows");
    assert_eq!(column.as_deref(), Some("email"));
    assert!(reason.contains("address-erased"), "{reason}");
    assert_eq!(
        value.as_deref(),
        Some("ada@example.com"),
        "the operator's own address, and nothing else"
    );
    // The tombstone's own fields are the one thing an erasure refusal may never leak.
    for secret in ["NOTE-SECRET", "SOURCE-SECRET"] {
        assert!(!reason.contains(secret), "{reason}");
        assert!(!value.as_deref().unwrap().contains(secret));
    }
}

#[sqlx::test]
async fn another_workspaces_contact_is_never_updated_by_an_import(pool: PgPool) {
    let (theirs, mine) = (
        workspace(&pool, "theirs").await,
        workspace(&pool, "mine").await,
    );
    let import = insert_import(&pool, mine).await;
    let j = job_with(import, mine, name_columns(), vec![], Dedup::UpdateExisting);
    let mut tx = pool.begin().await.unwrap();
    insert_contact(&mut tx, theirs, "ada@example.com", "Ada", "Lovelace").await;

    // The same address, imported by a workspace that is not the one holding it.
    let counts = import_csv(
        &mut tx,
        &j,
        "email,first_name,last_name,timezone\nada@example.com,Grace,Hopper,UTC\n",
    )
    .await;
    assert_eq!(
        counts,
        UpsertCounts {
            imported: 1,
            updated: 0,
            skipped: 0,
            collapsed: 0,
            failed: 0
        },
        "the other workspace's row is not an existing contact here"
    );

    let theirs_now = contact(&mut tx, theirs, "ada@example.com").await.unwrap();
    assert_eq!(
        (theirs_now.0.as_str(), theirs_now.1.as_str()),
        ("Ada", "Lovelace"),
        "another workspace's contact is never read and never written"
    );
    let mine_now = contact(&mut tx, mine, "ada@example.com").await.unwrap();
    assert_eq!(
        (mine_now.0.as_str(), mine_now.1.as_str()),
        ("Grace", "Hopper")
    );
}

#[sqlx::test]
async fn a_row_the_copy_refused_is_still_one_of_the_five_counts(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let j = job_with(import, ws, name_columns(), vec![], Dedup::UpdateExisting);
    let mut tx = pool.begin().await.unwrap();

    // Three of the five terms in one file: a row the `COPY` refused (`failed`), a row a later row
    // of the same file repeats (`collapsed`), and two new contacts (`imported`). The whole reason
    // `upsert_from_staging` takes `StagingCounts` is that the five come out apportioned here, so
    // the `COPY`'s own refusals have to be carried into them.
    let staged = copy(
        &mut tx,
        &j,
        b"email,first_name,last_name,timezone\n\
          not-an-email,No,Address,UTC\n\
          ada@example.com,First,Draft,UTC\n\
          ada@example.com,Last,Word,UTC\n\
          bob@example.com,Bob,Barker,UTC\n",
    )
    .await;
    assert_eq!((staged.total_rows, staged.staged, staged.failed), (4, 3, 1));

    let counts = upsert_from_staging(&mut tx, &j, staged).await.unwrap();
    assert_eq!(
        counts,
        UpsertCounts {
            imported: 2,
            updated: 0,
            skipped: 0,
            collapsed: 1,
            failed: 1
        }
    );
    assert_eq!(
        counts.imported + counts.updated + counts.skipped + counts.collapsed + counts.failed,
        staged.total_rows,
        "the five are mutually exclusive and are the whole file: {counts:?}"
    );
}

#[sqlx::test]
async fn a_contact_erased_after_it_was_inserted_is_deleted_before_commit(pool: PgPool) {
    let (ws, other) = (
        workspace(&pool, "acme").await,
        workspace(&pool, "other").await,
    );
    let import = insert_import(&pool, ws).await;
    let j = job_with(import, ws, name_columns(), vec![], Dedup::UpdateExisting);
    let mut tx = pool.begin().await.unwrap();

    // The residual window, as close as one transaction can stage it. The first three rows are
    // already contacts under the ids THIS import minted for them, which is exactly the state the
    // insert leaves behind, and the tombstones land after that — where the anti-join in the insert
    // can no longer see them and only the last statement before COMMIT can.
    //
    // The last three rows are new, and they are here to keep the call honest: the delete may only
    // take back rows this import inserted, so `inserted >= gone` is an invariant of the real path
    // and `upsert_from_staging` asserts it. Two expected deletions need two insertions, and the
    // third insertion is the slack that lets a THIRD, wrong deletion reach the assertion below
    // that names it, instead of tripping the invariant first.
    let staged = copy(
        &mut tx,
        &j,
        b"email,first_name,last_name,timezone\n\
          ada@example.com,Ada,Lovelace,UTC\n\
          ins@example.com,Ins,Tance,UTC\n\
          theirs@example.com,Their,Erasure,UTC\n\
          bob@example.com,Bob,Barker,UTC\n\
          carol@example.com,Carol,Shaw,UTC\n\
          dave@example.com,Dave,Theurer,UTC\n",
    )
    .await;
    sqlx::query(AssertSqlSafe(format!(
        "insert into contacts (id, workspace_id, email) \
         select id, $1, email from {} where row_number between 2 and 4",
        staging_table(import)
    )))
    .bind(ws)
    .execute(&mut *tx)
    .await
    .expect("the state the insert leaves behind");

    erase(&mut tx, Some(ws), "ada@example.com").await;
    erase(&mut tx, None, "ins@example.com").await;
    // Another workspace's erasure reaches nothing here, and the sweep deleting this contact would
    // be a cross-workspace deletion of a row this import had just created.
    erase(&mut tx, Some(other), "theirs@example.com").await;

    let counts = upsert_from_staging(&mut tx, &j, staged).await.unwrap();
    assert!(
        contact(&mut tx, ws, "ada@example.com").await.is_none(),
        "this workspace's tombstone takes the contact back out"
    );
    assert!(
        contact(&mut tx, ws, "ins@example.com").await.is_none(),
        "an instance-wide tombstone takes it out too"
    );
    assert!(
        contact(&mut tx, ws, "theirs@example.com").await.is_some(),
        "another workspace's tombstone may not delete this workspace's contact"
    );
    // The counts double-count here — this one call sees both the anti-join and the delete match
    // the same two rows, which a real race never would — so only the floor is asserted.
    assert!(counts.failed >= 2, "{counts:?}");
}

// ---- Task 5: the staging defects this task was handed -----------------------------------------

#[sqlx::test]
async fn a_leading_blank_line_does_not_eat_the_header(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let j = job(
        import,
        ws,
        vec![
            ColumnTarget::Field(ContactField::Email),
            ColumnTarget::Field(ContactField::FirstName),
        ],
        vec![],
    );
    let mut tx = pool.begin().await.unwrap();

    // Line 1 is one quoted empty field, which is a blank line and not a row — an exporter writes
    // them. The header is therefore on line 2, and has to be taken from the first NON-blank
    // record. A row that fails in `first_name` is how the headers make themselves visible: the
    // error row names the column by its header, or by `Column 2` if the header was parsed as data.
    let long = "n".repeat(101);
    let csv = format!("\"\"\nemail,first_name\na@b.co,{long}\nc@d.co,Ada\n");
    let counts = copy(&mut tx, &j, csv.as_bytes()).await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (2, 1, 1));
    assert_eq!(staged(&mut tx, import).await, [(4, "c@d.co".to_owned())]);

    let errors = error_rows(&mut tx, import).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 3, "the line a spreadsheet shows");
    assert_eq!(
        errors[0].1.as_deref(),
        Some("first_name"),
        "the header was read from line 2, not swallowed as a data row"
    );
}

#[sqlx::test]
async fn a_refused_first_record_is_an_error_row_even_when_it_was_the_header(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mapping = vec![
        ColumnTarget::Field(ContactField::Email),
        ColumnTarget::Field(ContactField::Tags),
    ];
    let j = job(import, ws, mapping, vec![]);
    let mut tx = pool.begin().await.unwrap();

    // An unclosed quote on line 1 makes the header itself one over-long record. A silent
    // `StagingCounts::default()` would report an empty file for a 512 MiB upload with nothing
    // saying why; a refused header is an error row like any other, and the rows after it stage.
    let huge = "z".repeat(MAX_RECORD_BYTES + 1024);
    let csv = format!("email,{huge}\na@b.co,x\n");
    let counts = copy(&mut tx, &j, csv.as_bytes()).await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (2, 1, 1));
    assert_eq!(staged(&mut tx, import).await, [(2, "a@b.co".to_owned())]);

    let errors = error_rows(&mut tx, import).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].0, 1);
    assert!(errors[0].2.contains("longer than"), "{}", errors[0].2);
}

#[sqlx::test]
async fn a_whitespace_only_custom_cell_is_the_file_saying_nothing(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let def = field("plan", FieldKind::Text);
    let mapping = vec![
        ColumnTarget::Field(ContactField::Email),
        ColumnTarget::Custom("plan".to_owned()),
    ];
    let j = job_with(import, ws, mapping, vec![def], Dedup::UpdateExisting);
    let mut tx = pool.begin().await.unwrap();
    insert_contact(&mut tx, ws, "ada@example.com", "Ada", "Lovelace").await;

    let counts = import_csv(&mut tx, &j, "email,plan\nada@example.com,\" \"\n").await;
    assert_eq!(counts.updated, 1);
    let got = contact(&mut tx, ws, "ada@example.com").await.unwrap();
    assert_eq!(
        got.3,
        json!({"plan": "pro"}),
        "a whitespace-only cell is a blank cell, not the string \" \""
    );
}

#[sqlx::test]
async fn a_record_of_exactly_the_column_cap_is_accepted_and_one_more_is_not(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let j = job(import, ws, email_column(), vec![]);
    let mut tx = pool.begin().await.unwrap();

    // `MAX_IMPORT_COLUMNS` is the cap, so a record OF that many fields is inside it and a record of
    // one more is not. Both are refused here — the mapping has one column — but for different
    // reasons, and the reason is the whole assertion.
    let at_cap = vec!["x"; MAX_IMPORT_COLUMNS].join(",");
    let over_cap = vec!["x"; MAX_IMPORT_COLUMNS + 1].join(",");
    let csv = format!("email\n{at_cap}\n{over_cap}\n");
    let counts = copy(&mut tx, &j, csv.as_bytes()).await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (2, 0, 2));

    let errors = error_rows(&mut tx, import).await;
    assert_eq!(errors.len(), 2);
    assert!(
        errors[0]
            .2
            .contains(&format!("the row has {MAX_IMPORT_COLUMNS} fields")),
        "a record of exactly the cap is parsed, not thrown away: {}",
        errors[0].2
    );
    assert!(
        errors[1]
            .2
            .contains(&format!("more than {MAX_IMPORT_COLUMNS} fields")),
        "{}",
        errors[1].2
    );
}
