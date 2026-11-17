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
    let counts = copy(
        &mut tx,
        &j,
        b"email,first_name\na@b.co,Ada\nc@d.co,Jos\xe9\n",
    )
    .await;
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
    // A `text` custom field, because this test is about the PARSER and needs a column that stores
    // a control character rather than refusing it. It used to use `tags`, which now goes through
    // `lists::validate_name` and would refuse a bare CR — and then this test would be asserting
    // the tag validator instead of the record terminator it is named for.
    let mapping = vec![
        ColumnTarget::Field(ContactField::Email),
        ColumnTarget::Custom("note".to_owned()),
    ];
    let j = job(import, ws, mapping, vec![field("note", FieldKind::Text)]);
    let mut tx = pool.begin().await.unwrap();

    // CRLF endings, and a quoted cell holding a bare CR: neither may end a record early or add an
    // empty one. A file read that way reports twice its rows and fails half of them.
    let counts = copy(&mut tx, &j, b"email,note\r\na@b.co,\"x\ry\"\r\nc@d.co,\r\n").await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (2, 2, 0));
    assert_eq!(
        staged(&mut tx, import).await,
        [(2, "a@b.co".to_owned()), (3, "c@d.co".to_owned())]
    );
    let attrs: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "select attributes from {} where row_number = 2",
        staging_table(import)
    )))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(attrs, json!({"note": "x\ry"}));
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
        errors[0]
            .2
            .contains(&format!("more than {MAX_IMPORT_COLUMNS} fields")),
        "{}",
        errors[0].2
    );
}

#[sqlx::test]
async fn a_cell_larger_than_the_parsers_buffer_stages_intact(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    // `lists`, because this test needs a 20 KiB cell to reach staging whole and every other
    // column now has a length rule that would refuse it first: `tags` is `lists::validate_name`
    // (100 characters), a `text` custom field is `MAX_TEXT` (2000). A list NAME is not validated
    // on the way in — it is matched against the workspace's lists later, and one that matches
    // nothing writes nothing — so it is the column that still shows what the parser produced.
    let mapping = vec![
        ColumnTarget::Field(ContactField::Email),
        ColumnTarget::Field(ContactField::Lists),
    ];
    let j = job(import, ws, mapping, vec![]);
    let mut tx = pool.begin().await.unwrap();

    // Over the 8 KiB the output buffer starts at, so the record only parses if the buffer grows
    // and the offsets carry across the grow. Under `MAX_RECORD_BYTES`, so it is a legitimate row.
    let big = "t".repeat(20_000);
    let csv = format!("email,lists\na@b.co,{big}\nc@d.co,x\n");
    let counts = copy(&mut tx, &j, csv.as_bytes()).await;
    assert_eq!((counts.total_rows, counts.staged, counts.failed), (2, 2, 0));

    let lists: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "select lists from {} where row_number = 2",
        staging_table(import)
    )))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        lists,
        json!([{"name": big, "status": null}]),
        "the big cell survives the grow intact"
    );
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
    upsert_from_staging(conn, j, staged.failed)
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
            failed: 0,
            memberships_declined: 0,
            suppressed: 0,
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
            failed: 0,
            memberships_declined: 0,
            suppressed: 0,
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
    let err = upsert_from_staging(&mut tx, &j, staged.failed)
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
            failed: 0,
            memberships_declined: 0,
            suppressed: 0,
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
            failed: 2,
            memberships_declined: 0,
            suppressed: 0,
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
            failed: 1,
            memberships_declined: 0,
            suppressed: 0,
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
            failed: 0,
            memberships_declined: 0,
            suppressed: 0,
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
    // `upsert_from_staging` takes the `COPY`'s refusal count is that the five come out apportioned
    // in one place: those rows never reached staging, so nothing downstream can see them.
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

    let counts = upsert_from_staging(&mut tx, &j, staged.failed)
        .await
        .unwrap();
    assert_eq!(
        counts,
        UpsertCounts {
            imported: 2,
            updated: 0,
            skipped: 0,
            collapsed: 1,
            failed: 1,
            memberships_declined: 0,
            suppressed: 0,
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

    let counts = upsert_from_staging(&mut tx, &j, staged.failed)
        .await
        .unwrap();
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

// ---- Task 6: what an import may and may not do to memberships and tags ------------------------

use nm::lists::MembershipStatus;

/// A list in this workspace. `double_optin` decides what a fresh membership is written as, and
/// nothing here depends on the list's name.
async fn insert_list(conn: &mut PgConnection, ws: Uuid, name: &str, double_optin: bool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into lists (id, workspace_id, name, double_optin) values ($1, $2, $3, $4)")
        .bind(id)
        .bind(ws)
        .bind(name)
        .bind(double_optin)
        .execute(conn)
        .await
        .expect("insert list");
    id
}

/// The membership as the import left it: its status, who wrote it, and the day its unsubscribe was
/// stamped. The stamp is read as a date string so that "the import did not touch it" is one
/// equality and this file needs no date library.
async fn membership(
    conn: &mut PgConnection,
    ws: Uuid,
    list: Uuid,
    email: &str,
) -> Option<(MembershipStatus, Option<MembershipSource>, Option<String>)> {
    sqlx::query_as(
        "select m.status, m.source, to_char(m.unsubscribed_at at time zone 'UTC', 'YYYY-MM-DD') \
           from list_memberships m \
           join contacts c on c.workspace_id = m.workspace_id and c.id = m.contact_id \
          where m.workspace_id = $1 and m.list_id = $2 and c.email = $3",
    )
    .bind(ws)
    .bind(list)
    .bind(email)
    .fetch_optional(conn)
    .await
    .expect("read membership")
}

/// An unsubscribe that happened before this import, stamped on a fixed day so the assertion that
/// the import left it alone is exact. `source` is `ui`: the consent log's answer to "who put this
/// person on the list" belongs to the first writer, and an import may not rewrite it.
async fn unsubscribe(conn: &mut PgConnection, ws: Uuid, list: Uuid, email: &str) {
    let done = sqlx::query(
        "insert into list_memberships \
             (workspace_id, contact_id, list_id, status, unsubscribed_at, source) \
         select $1, c.id, $2, 'unsubscribed', timestamptz '2020-01-01T00:00:00Z', 'ui' \
           from contacts c where c.workspace_id = $1 and c.email = $3",
    )
    .bind(ws)
    .bind(list)
    .bind(email)
    .execute(conn)
    .await
    .expect("unsubscribe");
    assert_eq!(done.rows_affected(), 1, "{email} is not a contact yet");
}

/// A membership that was already there before this import, written by somebody else.
async fn subscribe(conn: &mut PgConnection, ws: Uuid, list: Uuid, email: &str) {
    let done = sqlx::query(
        "insert into list_memberships \
             (workspace_id, contact_id, list_id, status, subscribed_at, source) \
         select $1, c.id, $2, 'subscribed', now(), 'ui' \
           from contacts c where c.workspace_id = $1 and c.email = $3",
    )
    .bind(ws)
    .bind(list)
    .bind(email)
    .execute(conn)
    .await
    .expect("subscribe");
    assert_eq!(done.rows_affected(), 1, "{email} is not a contact yet");
}

/// `mailable_list_members` is the one definition of "may this member be emailed". A test that
/// re-stated the rule instead of reading the view would pass while the send path disagreed.
async fn is_mailable(conn: &mut PgConnection, ws: Uuid, list: Uuid, email: &str) -> bool {
    sqlx::query_scalar(
        "select exists (select 1 from mailable_list_members \
                         where workspace_id = $1 and list_id = $2 and email = $3)",
    )
    .bind(ws)
    .bind(list)
    .bind(email)
    .fetch_one(conn)
    .await
    .expect("read mailable_list_members")
}

/// A standing block on one address. Not an erasure: a suppressed address is still importable.
async fn suppress_address(conn: &mut PgConnection, ws: Option<Uuid>, email: &str) {
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason) \
         values ($1, $2, 'address', $3, $4, 'bounced')",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(email)
    .bind(suppression_hmac(&secrets(), email).as_slice())
    .execute(conn)
    .await
    .expect("suppress address");
}

async fn suppress_domain(conn: &mut PgConnection, ws: Option<Uuid>, domain: &str) {
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, domain, reason) \
         values ($1, $2, 'domain', $3, 'complained')",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(domain)
    .execute(conn)
    .await
    .expect("suppress domain");
}

/// Every tag on one contact, by name.
async fn tags_of(conn: &mut PgConnection, ws: Uuid, email: &str) -> Vec<String> {
    sqlx::query_scalar(
        "select t.name from contact_tags ct \
           join tags t on t.workspace_id = ct.workspace_id and t.id = ct.tag_id \
           join contacts c on c.workspace_id = ct.workspace_id and c.id = ct.contact_id \
          where ct.workspace_id = $1 and c.email = $2 order by t.name",
    )
    .bind(ws)
    .bind(email)
    .fetch_all(conn)
    .await
    .expect("read contact_tags")
}

/// `email, tags, lists` in that order.
fn tag_and_list_columns() -> Vec<ColumnTarget> {
    vec![
        ColumnTarget::Field(ContactField::Email),
        ColumnTarget::Field(ContactField::Tags),
        ColumnTarget::Field(ContactField::Lists),
    ]
}

/// The same job with a list for every imported row to be subscribed to.
fn job_for_list(id: Uuid, ws: Uuid, mapping: Vec<ColumnTarget>, list: Uuid) -> ImportJob {
    ImportJob {
        list_id: Some(list),
        ..job(id, ws, mapping, vec![])
    }
}

#[sqlx::test]
async fn an_import_never_resubscribes_an_unsubscribed_membership(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mut tx = pool.begin().await.unwrap();
    let list = insert_list(&mut tx, ws, "news", false).await;
    insert_contact(&mut tx, ws, "ada@example.com", "Ada", "Lovelace").await;
    unsubscribe(&mut tx, ws, list, "ada@example.com").await;
    let j = job_for_list(import, ws, name_columns(), list);

    import_csv(
        &mut tx,
        &j,
        "email,first_name,last_name,timezone\nada@example.com,Ada,Byron,UTC\n",
    )
    .await;

    // THE COMPLIANCE ASSERTION OF THIS MILESTONE. Unsubscribing removes a row from the send, it
    // does not un-send; an import that quietly puts the person back is the failure the whole
    // suppression design exists to prevent.
    assert_eq!(
        membership(&mut tx, ws, list, "ada@example.com").await,
        Some((
            MembershipStatus::Unsubscribed,
            Some(MembershipSource::Ui),
            Some("2020-01-01".to_owned())
        )),
        "the import may not touch an unsubscribed membership: not its status, not the day it was \
         stamped, and not who wrote it"
    );
    assert!(
        !is_mailable(&mut tx, ws, list, "ada@example.com").await,
        "and the send-time view must agree"
    );
}

#[sqlx::test]
async fn that_row_is_counted_in_memberships_declined_and_the_contact_still_updates(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mut tx = pool.begin().await.unwrap();
    let list = insert_list(&mut tx, ws, "news", false).await;
    insert_contact(&mut tx, ws, "ada@example.com", "Ada", "Lovelace").await;
    unsubscribe(&mut tx, ws, list, "ada@example.com").await;
    // Carol is the control the count needs: she is already ON the list, so a `declined` that
    // counted "this membership already existed" instead of "this membership is unsubscribed"
    // would count her too, and the operator would read two refusals where there was one.
    insert_contact(&mut tx, ws, "carol@example.com", "Carol", "Shaw").await;
    subscribe(&mut tx, ws, list, "carol@example.com").await;
    let j = job_for_list(import, ws, name_columns(), list);

    let staged = copy(
        &mut tx,
        &j,
        b"email,first_name,last_name,timezone\n\
          ada@example.com,Ada,Byron,UTC\n\
          bob@example.com,Bob,Barker,UTC\n\
          carol@example.com,Carol,Shaw,UTC\n",
    )
    .await;
    let counts = upsert_from_staging(&mut tx, &j, staged.failed)
        .await
        .unwrap();

    assert_eq!(
        counts,
        UpsertCounts {
            imported: 1,
            updated: 2,
            skipped: 0,
            collapsed: 0,
            failed: 0,
            memberships_declined: 1,
            suppressed: 0,
        },
        "the contact is still updated; only the subscription is declined"
    );
    assert_eq!(
        membership(&mut tx, ws, list, "carol@example.com")
            .await
            .map(|m| (m.0, m.1)),
        Some((MembershipStatus::Subscribed, Some(MembershipSource::Ui))),
        "a membership that already exists is not declined and is not rewritten"
    );
    // `memberships_declined` is NOT one of the five, and adding it in would count Ada twice.
    assert_eq!(
        counts.imported + counts.updated + counts.skipped + counts.collapsed + counts.failed,
        staged.total_rows,
        "the five are still the whole file: {counts:?}"
    );
    assert_eq!(
        contact(&mut tx, ws, "ada@example.com").await.unwrap().1,
        "Byron",
        "the row is not skipped: the contact's own fields still move"
    );
    assert_eq!(
        membership(&mut tx, ws, list, "bob@example.com")
            .await
            .map(|m| m.0),
        Some(MembershipStatus::Subscribed),
        "and the row beside it is subscribed as normal"
    );
}

#[sqlx::test]
async fn a_suppressed_address_imports_and_is_not_mailable(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mut tx = pool.begin().await.unwrap();
    let list = insert_list(&mut tx, ws, "news", false).await;
    suppress_address(&mut tx, Some(ws), "bounced@example.com").await;
    let j = job_for_list(import, ws, name_columns(), list);

    let counts = import_csv(
        &mut tx,
        &j,
        "email,first_name,last_name,timezone\nbounced@example.com,Bo,Unced,UTC\n",
    )
    .await;

    // Suppression is about mailing, not about existence: the contact and its membership are both
    // written, and the eligibility view is what keeps the mail from going out.
    assert_eq!(counts.imported, 1);
    assert!(contact(&mut tx, ws, "bounced@example.com").await.is_some());
    assert_eq!(
        membership(&mut tx, ws, list, "bounced@example.com")
            .await
            .map(|m| m.0),
        Some(MembershipStatus::Subscribed),
    );
    assert!(
        !is_mailable(&mut tx, ws, list, "bounced@example.com").await,
        "a subscribed membership on a suppressed address is still not mailable"
    );
}

#[sqlx::test]
async fn the_summary_counts_imported_contacts_that_are_suppressed(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mut tx = pool.begin().await.unwrap();
    suppress_address(&mut tx, Some(ws), "bounced@example.com").await;
    // An instance-wide domain block counts too: the operator's question is "how many of these can
    // I never mail", and it does not care which row said so.
    suppress_domain(&mut tx, None, "blocked.example").await;
    let j = job_with(import, ws, name_columns(), vec![], Dedup::UpdateExisting);

    let counts = import_csv(
        &mut tx,
        &j,
        "email,first_name,last_name,timezone\n\
         bounced@example.com,Bo,Unced,UTC\n\
         someone@blocked.example,Some,One,UTC\n\
         ada@example.com,Ada,Lovelace,UTC\n",
    )
    .await;

    assert_eq!(counts.imported, 3, "all three are contacts: {counts:?}");
    assert_eq!(
        counts.suppressed, 2,
        "two of the three may never be mailed: {counts:?}"
    );
    assert_eq!(
        counts.imported + counts.updated + counts.skipped + counts.collapsed + counts.failed,
        3,
        "`suppressed` describes rows already counted among the five: {counts:?}"
    );
}

#[sqlx::test]
async fn every_membership_an_import_writes_has_source_import(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mut tx = pool.begin().await.unwrap();
    let chosen = insert_list(&mut tx, ws, "news", false).await;
    // A double-opt-in list the FILE names, so both ways a membership can be written are covered,
    // and the list's own setting still decides the status.
    let named = insert_list(&mut tx, ws, "offers", true).await;
    let j = job_for_list(import, ws, tag_and_list_columns(), chosen);

    import_csv(
        &mut tx,
        &j,
        "email,tags,lists\nada@example.com,,offers:subscribed\n",
    )
    .await;

    assert_eq!(
        membership(&mut tx, ws, chosen, "ada@example.com")
            .await
            .map(|m| (m.0, m.1)),
        Some((MembershipStatus::Subscribed, Some(MembershipSource::Import))),
        "the list the operator chose"
    );
    assert_eq!(
        membership(&mut tx, ws, named, "ada@example.com")
            .await
            .map(|m| (m.0, m.1)),
        Some((MembershipStatus::Pending, Some(MembershipSource::Import))),
        "a list the file named, pending because that list is double opt-in"
    );
    let unstamped: i64 = sqlx::query_scalar(
        "select count(*) from list_memberships \
          where workspace_id = $1 and source is distinct from 'import'",
    )
    .bind(ws)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(unstamped, 0, "an unexplainable membership is a consent bug");
}

#[sqlx::test]
async fn tags_are_applied_and_an_unknown_tag_is_created_once(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mut tx = pool.begin().await.unwrap();
    let vip = Uuid::now_v7();
    sqlx::query("insert into tags (id, workspace_id, name) values ($1, $2, 'vip')")
        .bind(vip)
        .bind(ws)
        .execute(&mut *tx)
        .await
        .unwrap();
    let j = job_with(import, ws, tag_and_list_columns(), vec![], Dedup::default());

    import_csv(
        &mut tx,
        &j,
        "email,tags,lists\n\
         ada@example.com,vip;founder,\n\
         bob@example.com,founder,\n",
    )
    .await;

    assert_eq!(
        tags_of(&mut tx, ws, "ada@example.com").await,
        ["founder", "vip"]
    );
    assert_eq!(tags_of(&mut tx, ws, "bob@example.com").await, ["founder"]);
    let names: Vec<(String, Uuid)> =
        sqlx::query_as("select name, id from tags where workspace_id = $1 order by name")
            .bind(ws)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    assert_eq!(
        names.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(),
        ["founder", "vip"],
        "two rows name `founder` and it is created once"
    );
    assert_eq!(
        names[1].1, vip,
        "and the tag that existed is reused, not remade"
    );
    // The tag list is `order by id desc`, documented as "Newest first": an id that is not time
    // ordered would scatter imported tags through that list for ever.
    assert!(names[0].1 > vip, "an imported tag's id is time-ordered");
}

#[sqlx::test]
async fn an_unsubscribed_entry_and_an_unknown_list_name_both_write_nothing(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mut tx = pool.begin().await.unwrap();
    let news = insert_list(&mut tx, ws, "news", false).await;
    let j = job_with(import, ws, tag_and_list_columns(), vec![], Dedup::default());

    let counts = import_csv(
        &mut tx,
        &j,
        "email,tags,lists\nada@example.com,,news:unsubscribed;ghost\n",
    )
    .await;

    assert_eq!(counts.imported, 1, "the contact is still imported");
    assert_eq!(
        membership(&mut tx, ws, news, "ada@example.com").await,
        None,
        "a file entry that says `unsubscribed` is not a request to subscribe anyone"
    );
    assert_eq!(
        counts.memberships_declined, 0,
        "and nothing was declined: the file never asked"
    );
    let lists: i64 = sqlx::query_scalar("select count(*) from lists where workspace_id = $1")
        .bind(ws)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        lists, 1,
        "an import creates tags, which only label; it never creates a list, which takes mail"
    );
}

#[sqlx::test]
async fn a_tag_name_the_api_would_refuse_is_an_error_row_naming_its_column(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mut tx = pool.begin().await.unwrap();
    let j = job_with(import, ws, tag_and_list_columns(), vec![], Dedup::default());

    // `tags.name` has no constraint behind it, so the import is the only thing standing between a
    // CSV and a tag row `PUT /tags/{id}` could never rewrite. One name too long and one holding a
    // control character: both are what `POST /tags` refuses, and both must be refused here in the
    // same words. The third row is the rest of the file, which carries on.
    let long = "x".repeat(101);
    let belled = "news\u{7}letter";
    let csv = format!(
        "email,tags,lists\n\
         ada@example.com,vip;{long},\n\
         bob@example.com,{belled},\n\
         carol@example.com,vip,\n"
    );
    let staged = copy(&mut tx, &j, csv.as_bytes()).await;
    let counts = upsert_from_staging(&mut tx, &j, staged.failed)
        .await
        .unwrap();

    let refused = "must be 1-100 characters with no control characters".to_owned();
    assert_eq!(
        error_rows(&mut tx, import).await,
        [
            (2, Some("tags".to_owned()), refused.clone(), Some(long)),
            (3, Some("tags".to_owned()), refused, Some(belled.to_owned())),
        ],
        "an operator whose tag is refused is told which row, which column and which value"
    );
    // One error row per failed ROW, as everything else this stage validates: the row is refused
    // whole, so `failed` and `import_errors` still agree on what a row is.
    assert_eq!((counts.imported, counts.failed), (1, 2), "{counts:?}");
    assert!(contact(&mut tx, ws, "ada@example.com").await.is_none());
    let names: Vec<String> =
        sqlx::query_scalar("select name from tags where workspace_id = $1 order by name")
            .bind(ws)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    assert_eq!(
        names,
        ["vip"],
        "a CSV may not put a name into `tags` that the API's own validator would reject"
    );
    assert_eq!(tags_of(&mut tx, ws, "carol@example.com").await, ["vip"]);
}

#[sqlx::test]
async fn a_blank_entry_between_two_tags_is_dropped_and_does_not_refuse_the_row(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mut tx = pool.begin().await.unwrap();
    let j = job_with(import, ws, tag_and_list_columns(), vec![], Dedup::default());

    // `vip;;founder` is the file saying nothing in the middle slot, the way a blank cell does.
    // There is no tag in it to create, so it is dropped — refusing the row over a doubled
    // separator would cost the operator a contact and protect nobody. Padding goes the same way
    // as everywhere else in this stage.
    let counts = import_csv(
        &mut tx,
        &j,
        "email,tags,lists\nada@example.com,vip;; founder ;,\n",
    )
    .await;

    assert_eq!(
        counts.imported,
        1,
        "{:?}",
        error_rows(&mut tx, import).await
    );
    assert_eq!(
        tags_of(&mut tx, ws, "ada@example.com").await,
        ["founder", "vip"]
    );
}

#[sqlx::test]
async fn skip_existing_writes_no_tags_and_no_membership_either(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mut tx = pool.begin().await.unwrap();
    let list = insert_list(&mut tx, ws, "news", false).await;
    insert_contact(&mut tx, ws, "ada@example.com", "Ada", "Lovelace").await;
    let j = ImportJob {
        list_id: Some(list),
        ..job_with(
            import,
            ws,
            tag_and_list_columns(),
            vec![],
            Dedup::SkipExisting,
        )
    };

    let counts = import_csv(
        &mut tx,
        &j,
        "email,tags,lists\n\
         ada@example.com,vip,\n\
         bob@example.com,vip,\n",
    )
    .await;

    assert_eq!((counts.imported, counts.skipped), (1, 1));
    // "Skip existing — matched rows are counted in `skipped` and nothing is written" (spec 7.3).
    // Nothing is nothing: not a tag, and not a subscription either.
    assert!(tags_of(&mut tx, ws, "ada@example.com").await.is_empty());
    assert_eq!(membership(&mut tx, ws, list, "ada@example.com").await, None);
    assert_eq!(tags_of(&mut tx, ws, "bob@example.com").await, ["vip"]);
    assert_eq!(
        membership(&mut tx, ws, list, "bob@example.com")
            .await
            .map(|m| m.0),
        Some(MembershipStatus::Subscribed),
        "the row that was not skipped is subscribed as normal"
    );
}

#[sqlx::test]
async fn a_padded_custom_cell_is_stored_without_its_padding(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let import = insert_import(&pool, ws).await;
    let mut tx = pool.begin().await.unwrap();
    let mut tier = field("tier", FieldKind::Select);
    tier.options = vec!["gold".to_owned()];
    let defs = vec![field("plan", FieldKind::Text), tier];
    let j = job_with(
        import,
        ws,
        vec![
            ColumnTarget::Field(ContactField::Email),
            ColumnTarget::Custom("plan".to_owned()),
            ColumnTarget::Custom("tier".to_owned()),
        ],
        defs,
        Dedup::default(),
    );

    // The arm already treats a cell of nothing but spaces as the file saying nothing; a cell with
    // padding AROUND a value has to lose the padding the same way, or `" gold "` is not one of the
    // field's options and a good row becomes an error row.
    let counts = import_csv(
        &mut tx,
        &j,
        "email,plan,tier\nada@example.com, pro , gold \n",
    )
    .await;

    assert_eq!(
        counts.imported,
        1,
        "{:?}",
        error_rows(&mut tx, import).await
    );
    assert_eq!(
        contact(&mut tx, ws, "ada@example.com").await.unwrap().3,
        json!({"plan": "pro", "tier": "gold"}),
    );
}

// ---- Task 7: the report, and the rows it rejected ----------------------------------------------

fn report_uri(ws: Uuid, import: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/imports/{import}")
}

/// What the run will write once `POST /imports/{id}/start` exists (Task 9): the counts the
/// pipeline computed, on the import row, with the import marked `complete`. Written by hand here
/// because running an import is that route's job, not this one's — but the NUMBERS are the
/// pipeline's own, never the test's, which is the whole point of the sum assertion below.
async fn finish(conn: &mut PgConnection, import: Uuid, total_rows: i64, c: &UpsertCounts) {
    let done = sqlx::query(
        "update imports set status = 'complete', total_rows = $2, imported = $3, updated = $4, \
         skipped = $5, collapsed = $6, failed = $7, suppressed = $8, memberships_declined = $9, \
         started_at = now(), finished_at = now() where id = $1",
    )
    .bind(import)
    .bind(total_rows)
    .bind(c.imported)
    .bind(c.updated)
    .bind(c.skipped)
    .bind(c.collapsed)
    .bind(c.failed)
    .bind(c.suppressed)
    .bind(c.memberships_declined)
    .execute(conn)
    .await
    .expect("finish import");
    assert_eq!(done.rows_affected(), 1);
}

async fn insert_error_at(pool: &PgPool, ws: Uuid, import: Uuid, row: i64, value: &str) {
    sqlx::query(
        "insert into import_errors \
             (import_id, workspace_id, row_number, column_name, reason, value) \
         values ($1, $2, $3, 'email', 'bad address', $4)",
    )
    .bind(import)
    .bind(ws)
    .bind(row)
    .bind(value)
    .execute(pool)
    .await
    .expect("insert import error");
}

#[sqlx::test]
async fn the_report_names_every_count_separately(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "v@example.com", Role::Viewer).await;
    let import = insert_import(&pool, ws).await;
    // Seven different numbers, none of them a sum or a difference of the others: a handler that
    // reads one column into another field cannot pass this by arithmetic accident.
    sqlx::query(
        "update imports set status = 'complete', filename = 'book.csv', total_rows = 97, \
         imported = 11, updated = 22, skipped = 33, collapsed = 44, failed = 55, suppressed = 66, \
         memberships_declined = 77, started_at = now(), finished_at = now() where id = $1",
    )
    .bind(import)
    .execute(&pool)
    .await
    .unwrap();
    // Fewer error rows than `failed`, which is what a file past `MAX_IMPORT_ERRORS` looks like:
    // the report has to say both numbers or the operator reads the shorter one as the whole truth.
    for row in 1..=3 {
        insert_error_at(&pool, ws, import, row, "not-an-email").await;
    }

    let r = app.get(&report_uri(ws, import)).await;
    r.assert_status_ok();
    let b: Value = r.json();
    assert_eq!(b["id"], import.to_string());
    assert_eq!(b["status"], "complete");
    assert_eq!(b["filename"], "book.csv");
    assert_eq!(b["total_rows"], 97);
    assert_eq!(b["imported"], 11);
    assert_eq!(b["updated"], 22);
    assert_eq!(b["skipped"], 33);
    assert_eq!(b["collapsed"], 44);
    assert_eq!(b["failed"], 55);
    assert_eq!(b["suppressed"], 66);
    assert_eq!(b["memberships_declined"], 77);
    assert_eq!(
        b["errors_recorded"], 3,
        "55 rows failed and 3 of them were kept: an operator paging to the end of the list has \
         seen the first 3, not all 55"
    );
    assert_eq!(b["error_reason"], Value::Null);
    assert!(b["created_at"].is_string());
    assert!(b["started_at"].is_string());
    assert!(b["finished_at"].is_string());

    // An import that died part-way says why, and says it has not counted the file: `null` is
    // "still unknown", which a `0` would report as an empty file.
    let dead = insert_import(&pool, ws).await;
    sqlx::query(
        "update imports set status = 'failed', error_reason = 'the file is not UTF-8', \
         started_at = now(), finished_at = now() where id = $1",
    )
    .bind(dead)
    .execute(&pool)
    .await
    .unwrap();
    let b: Value = app.get(&report_uri(ws, dead)).await.json();
    assert_eq!(b["status"], "failed");
    assert_eq!(b["error_reason"], "the file is not UTF-8");
    assert_eq!(b["total_rows"], Value::Null);
    assert_eq!(b["errors_recorded"], 0);
}

/// The FIVE row-level counts, named in the test's name so the next reader does not "fix" it by
/// adding the other two. `suppressed` and `memberships_declined` describe rows already counted
/// among these five — a suppressed address is still an imported or updated contact, and a declined
/// membership belongs to one — so adding all seven counts those rows twice. The last assertion
/// here is that the seven do NOT sum to `total_rows`, which is what makes the rule falsifiable.
#[sqlx::test]
async fn imported_updated_skipped_collapsed_and_failed_add_up_to_total_rows(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "v@example.com", Role::Viewer).await;
    let first = insert_import(&pool, ws).await;
    let second = insert_import(&pool, ws).await;
    let mut tx = pool.begin().await.unwrap();
    let list = insert_list(&mut tx, ws, "news", false).await;
    insert_contact(&mut tx, ws, "bob@example.com", "Bob", "Barker").await;
    unsubscribe(&mut tx, ws, list, "bob@example.com").await;
    suppress_address(&mut tx, Some(ws), "sue@example.com").await;

    // Six data rows, one of each kind the file can hold under `update existing`: a row the COPY
    // refuses, a row a later row repeats, three new contacts and one that already exists. Two of
    // those rows are ALSO a suppression and a declined subscription, which is exactly the overlap
    // the sum must not double-count.
    let j = job_with(
        first,
        ws,
        tag_and_list_columns(),
        vec![],
        Dedup::UpdateExisting,
    );
    let staged = copy(
        &mut tx,
        &j,
        b"email,tags,lists\n\
          not-an-email,vip,news\n\
          ada@example.com,vip,news\n\
          ada@example.com,vip,news\n\
          bob@example.com,vip,news\n\
          sue@example.com,vip,news\n\
          carol@example.com,vip,news\n",
    )
    .await;
    assert_eq!(
        staged.total_rows, 6,
        "the file the test wrote has six data rows"
    );
    let counts = upsert_from_staging(&mut tx, &j, staged.failed)
        .await
        .unwrap();
    finish(&mut tx, first, staged.total_rows, &counts).await;

    // And three rows under `skip existing`, because `skipped` is zero in every other mode: the
    // five cannot all be non-zero in one import.
    let j = job_with(
        second,
        ws,
        tag_and_list_columns(),
        vec![],
        Dedup::SkipExisting,
    );
    let other = copy(
        &mut tx,
        &j,
        b"email,tags,lists\n\
          ada@example.com,vip,news\n\
          dan@example.com,vip,news\n\
          not-an-email,vip,news\n",
    )
    .await;
    assert_eq!(other.total_rows, 3);
    let other_counts = upsert_from_staging(&mut tx, &j, other.failed)
        .await
        .unwrap();
    finish(&mut tx, second, other.total_rows, &other_counts).await;
    tx.commit().await.unwrap();

    let five = |b: &Value| -> i64 {
        ["imported", "updated", "skipped", "collapsed", "failed"]
            .iter()
            .map(|k| b[*k].as_i64().expect(k))
            .sum()
    };

    let b: Value = app.get(&report_uri(ws, first)).await.json();
    // The breakdown, not only its total: a sum assertion alone passes on any apportionment.
    assert_eq!(
        (
            &b["imported"],
            &b["updated"],
            &b["skipped"],
            &b["collapsed"],
            &b["failed"],
        ),
        (&json!(3), &json!(1), &json!(0), &json!(1), &json!(1)),
        "{b}"
    );
    assert_eq!(b["total_rows"], 6);
    assert_eq!(five(&b), 6, "the five are the whole file: {b}");
    assert_eq!(
        (&b["suppressed"], &b["memberships_declined"]),
        (&json!(1), &json!(1)),
        "{b}"
    );
    assert_eq!(
        five(&b) + b["suppressed"].as_i64().unwrap() + b["memberships_declined"].as_i64().unwrap(),
        8,
        "all seven come to more than the file's six rows, because two of the five are counted \
         again by the other two. This is the test's point, not its bug."
    );
    assert_eq!(b["errors_recorded"], 1);

    let b: Value = app.get(&report_uri(ws, second)).await.json();
    assert_eq!(
        (
            &b["imported"],
            &b["updated"],
            &b["skipped"],
            &b["collapsed"],
            &b["failed"],
        ),
        (&json!(1), &json!(0), &json!(1), &json!(0), &json!(1)),
        "{b}"
    );
    assert_eq!(b["total_rows"], 3);
    assert_eq!(five(&b), 3, "{b}");
}

#[sqlx::test]
async fn errors_are_cursor_paginated_and_scoped_to_the_workspace(pool: PgPool) {
    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let app = member(&pool, a, "v@example.com", Role::Viewer).await;
    let ours = insert_import(&pool, a).await;
    let theirs = insert_import(&pool, b).await;
    for row in 1..=5 {
        insert_error_at(&pool, a, ours, row, &format!("row-{row}@bad")).await;
    }
    for row in 1..=2 {
        insert_error_at(&pool, b, theirs, row, "THEIRS").await;
    }

    // File order, two at a time, following the cursor to the end.
    let (mut rows, mut uri, mut pages) = (
        Vec::new(),
        format!("{}/errors?limit=2", report_uri(a, ours)),
        0,
    );
    loop {
        let page: Value = app.get(&uri).await.json();
        pages += 1;
        assert!(pages <= 4, "the cursor is not advancing");
        rows.extend(page["data"].as_array().expect("data").clone());
        match page["next_cursor"].as_str() {
            Some(cursor) => uri = format!("{}/errors?limit=2&cursor={cursor}", report_uri(a, ours)),
            None => break,
        }
    }
    assert_eq!(pages, 3, "five rows, two at a time");
    assert_eq!(
        rows.iter()
            .map(|r| r["row_number"].clone())
            .collect::<Vec<_>>(),
        [json!(1), json!(2), json!(3), json!(4), json!(5)],
        "`row_number` is the CSV row, so the list is the file's own order"
    );
    assert_eq!(
        rows[0],
        json!({
            "row_number": 1,
            "column_name": "email",
            "reason": "bad address",
            "value": "row-1@bad",
        }),
        "which row, which column, why, and the cell that did it"
    );
    assert!(
        !rows.iter().any(|r| r["value"] == "THEIRS"),
        "the other workspace's rejected rows are not in this workspace's list"
    );

    // Another workspace's import, asked for under OUR workspace: 404, never its rows and never a
    // 403 — nobody learns from this whether that import exists.
    let r = app.get(&format!("{}/errors", report_uri(a, theirs))).await;
    r.assert_status(StatusCode::NOT_FOUND);
    let problem: Value = r.json();
    assert_eq!(problem["type"], "urn:nm:problem:not-found");
    assert_eq!(problem["data"], Value::Null);

    // And asked for under THEIR workspace, by someone who is not a member of it.
    app.get(&format!("{}/errors", report_uri(b, theirs)))
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // A cursor that is not one is a 422, not a silent first page.
    app.get(&format!("{}/errors?cursor=%25%25%25", report_uri(a, ours)))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test]
async fn another_workspaces_import_report_is_404_not_403(pool: PgPool) {
    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let app = member(&pool, a, "v@example.com", Role::Viewer).await;
    let theirs = insert_import(&pool, b).await;
    sqlx::query("update imports set filename = 'THEIR-BOOK.csv' where id = $1")
        .bind(theirs)
        .execute(&pool)
        .await
        .unwrap();

    // Control: our own import reads back, so the refusals below are the tenancy's.
    let ours = insert_import(&pool, a).await;
    app.get(&report_uri(a, ours)).await.assert_status_ok();

    for uri in [report_uri(a, theirs), report_uri(b, theirs)] {
        let r = app.get(&uri).await;
        r.assert_status(StatusCode::NOT_FOUND);
        let problem: Value = r.json();
        assert_eq!(problem["type"], "urn:nm:problem:not-found");
        assert!(
            !problem.to_string().contains("THEIR-BOOK"),
            "a 404 carries none of the resource it refused: {problem}"
        );
    }
}

#[sqlx::test]
async fn the_audit_row_names_the_import_and_never_the_file_contents(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let import = upload_kept(
        &app,
        ws,
        "book.csv",
        "email,notes\nada@example.com,SECRET-CELL\n",
    )
    .await;

    // AND THE RUN. Without it `import.started` and `import.finished` are structurally outside this
    // guard — the only test that reads every audit row of a workspace for the file's own cells —
    // and the next field added to either one goes unwatched.
    app.post(&start_uri(ws, import))
        .json(&json!({"mapping": [field_target("email"), {"kind": "ignore"}]}))
        .await
        .assert_status_ok();

    // Reading the report and the rejected rows is a read: it writes no audit row of its own.
    app.get(&report_uri(ws, import)).await.assert_status_ok();
    app.get(&format!("{}/errors", report_uri(ws, import)))
        .await
        .assert_status_ok();

    let rows: Vec<(String, Option<String>, Option<String>, Value)> = sqlx::query_as(
        "select action, target_type, target_id, metadata from audit_log \
         where workspace_id = $1 order by id",
    )
    .bind(ws)
    .fetch_all(&pool)
    .await
    .unwrap();
    let about_the_import: Vec<_> = rows
        .iter()
        .filter(|r| r.1.as_deref() == Some("import"))
        .collect();
    assert_eq!(
        about_the_import
            .iter()
            .map(|r| r.0.as_str())
            .collect::<Vec<_>>(),
        ["import.uploaded", "import.started", "import.finished"],
        "the upload and the run are logged; the two reads are not: {rows:?}"
    );
    let logged_import = about_the_import[0];
    assert_eq!(
        (
            logged_import.0.as_str(),
            logged_import.2.as_deref(),
            &logged_import.3["filename"],
        ),
        (
            "import.uploaded",
            Some(&*import.to_string()),
            &json!("book.csv"),
        ),
        "the row names the import by id, which is how the log is read back"
    );

    // The audit log is stored in plaintext and read by admins. The file's own cells — an address,
    // a header, a value — are not the import's identity and never belong in it.
    let logged = format!("{rows:?}");
    for leak in ["SECRET-CELL", "ada@example.com", "notes"] {
        assert!(
            !logged.contains(leak),
            "the audit row carries `{leak}` out of the file: {logged}"
        );
    }
}

// ---- Task 9: the run, and the list of a workspace's imports ------------------------------------

fn start_uri(ws: Uuid, import: Uuid) -> String {
    format!("{}/start", report_uri(ws, import))
}

fn field_target(name: &str) -> Value {
    json!({"kind": "field", "value": name})
}

/// Uploads `body` and LEAVES the file on disk: that file is what the run reads, and `uploaded`'s
/// cleanup would take it away.
async fn upload_kept(app: &TestApp, ws: Uuid, name: &str, body: &str) -> Uuid {
    let r = upload(app, ws, name, body).await;
    r.assert_status(StatusCode::CREATED);
    r.json::<Value>()["id"].as_str().unwrap().parse().unwrap()
}

/// `status`, `delimiter` and `has_header` as the row holds them. The last two are what the run
/// used, which a start may override.
async fn import_row(pool: &PgPool, import: Uuid) -> (String, i16, bool) {
    sqlx::query_as("select status, delimiter, has_header from imports where id = $1")
        .bind(import)
        .fetch_one(pool)
        .await
        .expect("read import")
}

/// Every address in the workspace, alphabetically.
async fn emails(pool: &PgPool, ws: Uuid) -> Vec<String> {
    sqlx::query_scalar("select email from contacts where workspace_id = $1 order by email")
        .bind(ws)
        .fetch_all(pool)
        .await
        .expect("read contacts")
}

/// THE TEST THIS TASK EXISTS FOR. Nothing in `src/` wrote an import's counts before this route:
/// Task 7's report reads seven columns and a `total_rows` that only a test's hand-written `UPDATE`
/// had ever set. So this one runs a real file through the real handler and reads the real report
/// back - delete the handler's `update imports set …` and every number below goes to zero while
/// every other test on this branch still passes.
#[sqlx::test]
async fn a_run_writes_every_count_it_computed_to_the_report(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let mut conn = pool.acquire().await.unwrap();
    let list = insert_list(&mut conn, ws, "news", false).await;
    insert_contact(&mut conn, ws, "bob@example.com", "Bob", "Barker").await;
    unsubscribe(&mut conn, ws, list, "bob@example.com").await;
    suppress_address(&mut conn, Some(ws), "sue@example.com").await;
    // The list the REQUEST names, which no cell in the file does. The file's own `lists` column
    // names `news`, so a run that ignored `list_id` would still fill `news` and look right.
    let everyone = insert_list(&mut conn, ws, "everyone", false).await;
    drop(conn);

    // Six data rows, one of each kind `update existing` can produce, and two of them are also the
    // overlap the two non-row-level counts describe: sue is suppressed, bob's subscription is
    // declined. Seven counts and a total, no two of them equal by accident.
    let import = upload_kept(
        &app,
        ws,
        "book.csv",
        "email,tags,lists\n\
         not-an-email,vip,news\n\
         ada@example.com,vip,news\n\
         ada@example.com,vip,news\n\
         bob@example.com,vip,news\n\
         sue@example.com,vip,news\n\
         carol@example.com,vip,news\n",
    )
    .await;

    let r = app
        .post(&start_uri(ws, import))
        .json(&json!({
            "mapping": [field_target("email"), field_target("tags"), field_target("lists")],
            "dedup": "update_existing",
            "list_id": everyone.to_string(),
        }))
        .await;
    r.assert_status_ok();
    let run: Value = r.json();

    // Read back through `GET /imports/{id}`, which is the route an operator reads, not the run's
    // own answer.
    let b: Value = app.get(&report_uri(ws, import)).await.json();
    assert_eq!(
        b, run,
        "the run answers with the report the read answers with"
    );
    assert_eq!(b["status"], "complete");
    assert_eq!(
        (
            &b["total_rows"],
            &b["imported"],
            &b["updated"],
            &b["skipped"],
            &b["collapsed"],
            &b["failed"],
            &b["suppressed"],
            &b["memberships_declined"],
        ),
        (
            &json!(6),
            &json!(3),
            &json!(1),
            &json!(0),
            &json!(1),
            &json!(1),
            &json!(1),
            &json!(1),
        ),
        "the eight numbers the pipeline computed, on the row the report reads: {b}"
    );
    assert_eq!(b["errors_recorded"], 1);
    assert_eq!(b["error_reason"], Value::Null);
    assert!(
        b["started_at"].is_string() && b["finished_at"].is_string(),
        "{b}"
    );

    // And the contacts themselves, so the counts are a report of work done and not of arithmetic.
    assert_eq!(
        emails(&pool, ws).await,
        [
            "ada@example.com",
            "bob@example.com",
            "carol@example.com",
            "sue@example.com"
        ]
    );
    let page: Value = app
        .get(&format!("{}/errors", report_uri(ws, import)))
        .await
        .json();
    assert_eq!(page["data"][0]["row_number"], 2, "the header is row 1");
    assert_eq!(page["data"][0]["value"], "not-an-email");
    assert!(
        !upload_path(import).exists(),
        "the uploaded file is gone once it has been imported"
    );

    // AND THE LIST THE REQUEST NAMED. `list_id` is how an import subscribes contacts to a list,
    // and a run that dropped it reports every count above correctly while subscribing nobody.
    // Every contact the run touched is on it, suppressed and updated ones included: suppression
    // is about mailing and not existence, and `update existing` reaches a contact that was
    // already here.
    let mut conn = pool.acquire().await.unwrap();
    for email in [
        "ada@example.com",
        "bob@example.com",
        "carol@example.com",
        "sue@example.com",
    ] {
        assert_eq!(
            membership(&mut conn, ws, everyone, email).await,
            Some((
                MembershipStatus::Subscribed,
                Some(MembershipSource::Import),
                None
            )),
            "{email} was not subscribed to the list the request named"
        );
    }
    // And the row records the list it was asked for, which is the other place `list_id` is used.
    let stored: Option<Uuid> = sqlx::query_scalar("select list_id from imports where id = $1")
        .bind(import)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, Some(everyone));
}

#[sqlx::test]
async fn start_naming_another_workspaces_list_is_404(pool: PgPool) {
    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let app = member(&pool, a, "ed@example.com", Role::Editor).await;
    let mut conn = pool.acquire().await.unwrap();
    let theirs = insert_list(&mut conn, b, "news", false).await;
    let ours = insert_list(&mut conn, a, "news", false).await;
    drop(conn);
    let import = upload_kept(&app, a, "book.csv", "email\nada@example.com\n").await;

    let r = app
        .post(&start_uri(a, import))
        .json(&json!({
            "mapping": [field_target("email")],
            "list_id": theirs.to_string(),
        }))
        .await;
    r.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(r.json::<Value>()["type"], "urn:nm:problem:not-found");
    // Nothing ran, and the import is still startable with a list that is ours.
    assert_eq!(count(&pool, "contacts").await, 0);
    assert_eq!(import_row(&pool, import).await.0, "pending");

    // Control: the same request with our own list runs, so the 404 is the tenancy's doing and
    // not the field's.
    app.post(&start_uri(a, import))
        .json(&json!({
            "mapping": [field_target("email")],
            "list_id": ours.to_string(),
        }))
        .await
        .assert_status_ok();
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        membership(&mut conn, a, ours, "ada@example.com")
            .await
            .is_some()
    );
}

#[sqlx::test]
async fn a_start_whose_uploaded_file_is_gone_is_422_and_ends_the_import(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let import = upload_kept(&app, ws, "book.csv", "email\nada@example.com\n").await;
    // The temp directory was swept, or this process is not the one that took the upload. Either
    // way the file is not coming back.
    std::fs::remove_file(upload_path(import)).unwrap();

    let body = json!({"mapping": [field_target("email")]});
    let r = app.post(&start_uri(ws, import)).json(&body).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json::<Value>()["type"], "urn:nm:problem:import-file-gone");

    // THE IMPORT IS OVER, not back where it started: no retry can bring the file back, and a row
    // left `pending` sits in `GET /imports` for ever with nothing that can clear it.
    let b: Value = app.get(&report_uri(ws, import)).await.json();
    assert_eq!(b["status"], "failed");
    assert_eq!(
        b["error_reason"],
        "the uploaded file is no longer on this server; upload it again"
    );
    assert_eq!(
        b["total_rows"],
        Value::Null,
        "the COPY never counted the file, and `0` would report an empty one"
    );
    assert_eq!(b["failed"], 0);
    assert!(b["finished_at"].is_string(), "{b}");

    // So a second start is the 409 an import that has run gets, not the same 422 again.
    let r = app.post(&start_uri(ws, import)).json(&body).await;
    r.assert_status(StatusCode::CONFLICT);
    assert_eq!(count(&pool, "contacts").await, 0);
}

#[sqlx::test]
async fn start_with_a_mapping_naming_a_deleted_field_is_422_naming_it(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ad@example.com", Role::Admin).await;
    let r = app
        .post(&format!("/api/v1/workspaces/{ws}/custom-fields"))
        .json(&json!({"key": "colour", "label": "Colour", "kind": "text"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let field = r.json::<Value>()["id"].as_str().unwrap().to_owned();

    let csv = "email,attr:colour\nada@example.com,red\n";
    let import = upload_kept(&app, ws, "book.csv", csv).await;

    // Deleted between the sniff and the start, which is the whole of this case: the mapping the
    // operator confirmed names a field that is no longer anywhere to put data. Archive and purge
    // is as deleted as a custom field gets — `purge_custom_field` removes every contact's value
    // and KEEPS the definition, so no API path ever leaves the row gone.
    app.delete(&format!("/api/v1/workspaces/{ws}/custom-fields/{field}"))
        .await
        .assert_status_ok();
    app.post(&format!(
        "/api/v1/workspaces/{ws}/custom-fields/{field}/purge"
    ))
    .await
    .assert_status_ok();

    let r = app
        .post(&start_uri(ws, import))
        .json(&json!({
            "mapping": [field_target("email"), {"kind": "custom", "value": "colour"}],
        }))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    let p: Value = r.json();
    assert_eq!(p["type"], "urn:nm:problem:validation");
    assert_eq!(
        (&p["errors"][0]["field"], &p["errors"][0]["message"]),
        (
            &json!("mapping[1]"),
            &json!("this custom field is archived and takes no new values")
        ),
        "the 422 names the column, and does not run a file whose every row would fail: {p}"
    );

    // And a key that was never a field at all. The wizard offers only live fields, but the API
    // takes a mapping directly and a client-side list is not a guard.
    let r = app
        .post(&start_uri(ws, import))
        .json(&json!({
            "mapping": [field_target("email"), {"kind": "custom", "value": "ghost"}],
        }))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    let p: Value = r.json();
    assert_eq!(
        (&p["errors"][0]["field"], &p["errors"][0]["message"]),
        (&json!("mapping[1]"), &json!("no such custom field")),
        "{p}"
    );

    // Nothing ran, and the import is still startable with a mapping that is not stale.
    assert_eq!(count(&pool, "contacts").await, 0);
    assert_eq!(import_row(&pool, import).await.0, "pending");
    std::fs::remove_file(upload_path(import)).unwrap();
}

#[sqlx::test]
async fn two_columns_aimed_at_one_field_is_422_naming_both(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let csv = "email,work_email,tags,more_tags\nada@example.com,ada@work.com,vip,eu\n";
    let import = upload_kept(&app, ws, "book.csv", csv).await;

    let r = app
        .post(&start_uri(ws, import))
        .json(&json!({
            "mapping": [
                field_target("email"), field_target("email"),
                field_target("tags"), field_target("tags"),
            ],
        }))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    let p: Value = r.json();
    assert_eq!(p["type"], "urn:nm:problem:duplicate-mapping-target");
    let detail = p["detail"].as_str().unwrap();
    assert!(
        detail.contains("columns 1 and 2") && detail.contains("email"),
        "the refusal names both columns and the target they share: {detail}"
    );
    assert_eq!(count(&pool, "contacts").await, 0);

    // Tags and lists accumulate, so two columns of them are not a collision. The same request
    // without the duplicated `email` runs, which is what makes the refusal above specific.
    let r = app
        .post(&start_uri(ws, import))
        .json(&json!({
            "mapping": [
                field_target("email"), {"kind": "ignore"},
                field_target("tags"), field_target("tags"),
            ],
        }))
        .await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["imported"], 1);
    assert_eq!(
        tags_of(&mut pool.acquire().await.unwrap(), ws, "ada@example.com").await,
        ["eu", "vip"],
        "both tag columns were read"
    );
}

#[sqlx::test]
async fn start_on_another_workspaces_import_is_404(pool: PgPool) {
    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let app = member(&pool, a, "ed@example.com", Role::Editor).await;
    let viewer = member(&pool, a, "v@example.com", Role::Viewer).await;
    let theirs = {
        let b_app = member(&pool, b, "ed-b@example.com", Role::Editor).await;
        upload_kept(&b_app, b, "THEIR-BOOK.csv", "email\nbea@example.com\n").await
    };

    let body = json!({"mapping": [field_target("email")]});

    // Control: our own import in our own workspace runs, so the refusals below are the tenancy's
    // and not an absent route answering 404 for everything.
    let ours = upload_kept(&app, a, "ours.csv", "email\nada@example.com\n").await;
    app.post(&start_uri(a, ours))
        .json(&body)
        .await
        .assert_status_ok();

    for uri in [start_uri(a, theirs), start_uri(b, theirs)] {
        let r = app.post(&uri).json(&body).await;
        r.assert_status(StatusCode::NOT_FOUND);
        let p: Value = r.json();
        assert_eq!(p["type"], "urn:nm:problem:not-found");
        assert!(
            !p.to_string().contains("THEIR-BOOK"),
            "a 404 carries none of the resource it refused: {p}"
        );
    }
    // 404 BEFORE THE ROLE CHECK: a viewer asking under the workspace they are not in learns
    // nothing more than a stranger would, and nothing less either.
    viewer
        .post(&start_uri(b, theirs))
        .json(&body)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    assert_eq!(
        emails(&pool, a).await,
        ["ada@example.com"],
        "only the control ran; nothing of theirs did"
    );
    assert_eq!(emails(&pool, b).await, Vec::<String>::new());
    assert_eq!(import_row(&pool, theirs).await.0, "pending");
    std::fs::remove_file(upload_path(theirs)).unwrap();
}

#[sqlx::test]
async fn start_twice_on_the_same_import_is_409(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let import = upload_kept(&app, ws, "book.csv", "email\nada@example.com\n").await;
    let body = json!({"mapping": [field_target("email")]});

    let r = app.post(&start_uri(ws, import)).json(&body).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["imported"], 1);

    let r = app.post(&start_uri(ws, import)).json(&body).await;
    r.assert_status(StatusCode::CONFLICT);
    let p: Value = r.json();
    assert_eq!(p["type"], "urn:nm:problem:import-already-started");
    assert!(
        p["detail"].as_str().unwrap().contains("complete"),
        "the refusal says what the import already is: {p}"
    );

    // The first run's report is untouched, and the file was imported once.
    let b: Value = app.get(&report_uri(ws, import)).await.json();
    assert_eq!(
        (&b["status"], &b["imported"], &b["total_rows"]),
        (&json!("complete"), &json!(1), &json!(1)),
    );
    assert_eq!(count(&pool, "contacts").await, 1);
}

/// The decision this task had to make and could not duck: a `fail on duplicate` refusal is a
/// `SELECT`, so the transaction is still good - and the rows the `COPY` already rejected are the
/// report an operator fixing a bad file actually needs. They are committed; the contacts are not.
#[sqlx::test]
async fn a_refused_fail_on_duplicate_keeps_the_rows_the_copy_rejected(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let mut conn = pool.acquire().await.unwrap();
    insert_contact(&mut conn, ws, "bob@example.com", "Bob", "Barker").await;
    drop(conn);

    let import = upload_kept(
        &app,
        ws,
        "book.csv",
        "email\nnot-an-email\nbob@example.com\nada@example.com\n",
    )
    .await;
    let r = app
        .post(&start_uri(ws, import))
        .json(&json!({
            "mapping": [field_target("email")],
            "dedup": "fail_on_duplicate",
        }))
        .await;
    r.assert_status(StatusCode::CONFLICT);
    let p: Value = r.json();
    assert_eq!(p["type"], "urn:nm:problem:import-duplicate-address");
    assert!(
        p["detail"].as_str().unwrap().contains("bob@example.com"),
        "{p}"
    );

    // The file was refused, so nothing it named was written.
    assert_eq!(emails(&pool, ws).await, ["bob@example.com"]);

    // But the per-row report survives the refusal: without it the operator is told one address is
    // a duplicate and never learns that row 2 is not an address at all.
    let b: Value = app.get(&report_uri(ws, import)).await.json();
    assert_eq!(b["status"], "failed");
    assert!(
        b["error_reason"]
            .as_str()
            .unwrap()
            .contains("bob@example.com"),
        "the report says why the import as a whole was refused: {b}"
    );
    assert_eq!(
        (
            &b["total_rows"],
            &b["failed"],
            &b["errors_recorded"],
            &b["imported"]
        ),
        (&json!(3), &json!(1), &json!(1), &json!(0)),
        "{b}"
    );
    let page: Value = app
        .get(&format!("{}/errors", report_uri(ws, import)))
        .await
        .json();
    assert_eq!(page["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        (&page["data"][0]["row_number"], &page["data"][0]["value"]),
        (&json!(2), &json!("not-an-email")),
    );
}

#[sqlx::test]
async fn the_start_may_override_the_stored_delimiter_and_header(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let mapping = json!([field_target("email")]);

    // A headed file the operator says has no header. The header row is then a data row, and a data
    // row reading `email` is not an address - which is exactly what the operator sees when the
    // guess goes the other way and their first contact silently vanishes.
    let headed = upload_kept(&app, ws, "a.csv", "email\nada@example.com\n").await;
    let r = app
        .post(&start_uri(ws, headed))
        .json(&json!({"mapping": mapping, "has_header": false}))
        .await;
    r.assert_status_ok();
    let b: Value = r.json();
    assert_eq!(
        (&b["total_rows"], &b["imported"], &b["failed"]),
        (&json!(2), &json!(1), &json!(1)),
        "the header row was read as data, because the request said so: {b}"
    );
    assert_eq!(
        import_row(&pool, headed).await,
        ("complete".to_owned(), 44, false)
    );

    // A comma file read as semicolons: one field per row, so the whole line lands in `email` and
    // the name is swallowed into the address. A misdetected delimiter misparses every row, which
    // is why the operator who was shown the guess has to be able to correct it.
    let commas = upload_kept(&app, ws, "b.csv", "email,first_name\nbea@example.com,Bea\n").await;
    let r = app
        .post(&start_uri(ws, commas))
        .json(&json!({
            "mapping": [field_target("email"), field_target("first_name")],
            "delimiter": ";",
        }))
        .await;
    r.assert_status_ok();
    let b: Value = r.json();
    assert_eq!(
        (&b["total_rows"], &b["imported"]),
        (&json!(1), &json!(1)),
        "{b}"
    );
    assert!(
        emails(&pool, ws)
            .await
            .contains(&"bea@example.com,bea".to_owned()),
        "the whole line was one field, because the request said the delimiter was `;`"
    );
    assert_eq!(
        import_row(&pool, commas).await,
        ("complete".to_owned(), 59, true)
    );

    // A request carrying neither uses what the sniff stored, and nothing is sniffed again.
    let stored = upload_kept(&app, ws, "c.csv", "email;first_name\ncaz@example.com;Caz\n").await;
    assert_eq!(
        import_row(&pool, stored).await,
        ("pending".to_owned(), 59, true)
    );
    let r = app
        .post(&start_uri(ws, stored))
        .json(&json!({"mapping": [field_target("email"), field_target("first_name")]}))
        .await;
    r.assert_status_ok();
    let b: Value = r.json();
    assert_eq!(
        (&b["total_rows"], &b["imported"]),
        (&json!(1), &json!(1)),
        "{b}"
    );
    assert_eq!(
        import_row(&pool, stored).await,
        ("complete".to_owned(), 59, true)
    );

    // A delimiter the sniff would never have chosen is refused, not guessed at: a `"` or a newline
    // parses the file in a way nobody was shown.
    let bad = upload_kept(&app, ws, "d.csv", "email\ndee@example.com\n").await;
    let r = app
        .post(&start_uri(ws, bad))
        .json(&json!({"mapping": mapping, "delimiter": "\""}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json::<Value>()["errors"][0]["field"], "delimiter");
    assert_eq!(import_row(&pool, bad).await.0, "pending");
    std::fs::remove_file(upload_path(bad)).unwrap();
}

#[sqlx::test]
async fn an_editor_may_import_and_a_viewer_may_not(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let editor = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let viewer = member(&pool, ws, "v@example.com", Role::Viewer).await;
    let import = upload_kept(&editor, ws, "book.csv", "email\nada@example.com\n").await;
    let body = json!({"mapping": [field_target("email")]});

    // The same workspace and the same import, so the only difference is the role.
    let r = viewer.post(&start_uri(ws, import)).json(&body).await;
    r.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(
        r.json::<Value>()["type"],
        "urn:nm:problem:insufficient-role"
    );
    assert_eq!(count(&pool, "contacts").await, 0);
    assert_eq!(import_row(&pool, import).await.0, "pending");

    // A viewer may still read: refusing the run is not refusing the report.
    viewer.get(&report_uri(ws, import)).await.assert_status_ok();
    viewer.get(&imports_uri(ws)).await.assert_status_ok();

    let r = editor.post(&start_uri(ws, import)).json(&body).await;
    r.assert_status_ok();
    assert_eq!(r.json::<Value>()["imported"], 1);
    assert_eq!(emails(&pool, ws).await, ["ada@example.com"]);
}

#[sqlx::test]
async fn the_list_is_cursor_paginated_and_ordered_newest_first(pool: PgPool) {
    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let app = member(&pool, a, "v@example.com", Role::Viewer).await;
    let mut ours = Vec::new();
    for _ in 0..5 {
        ours.push(insert_import(&pool, a).await);
    }
    let theirs = insert_import(&pool, b).await;

    let (mut rows, mut uri, mut pages) = (Vec::new(), format!("{}?limit=2", imports_uri(a)), 0);
    loop {
        let page: Value = app.get(&uri).await.json();
        pages += 1;
        assert!(pages <= 4, "the cursor is not advancing");
        rows.extend(page["data"].as_array().expect("data").clone());
        match page["next_cursor"].as_str() {
            Some(cursor) => uri = format!("{}?limit=2&cursor={cursor}", imports_uri(a)),
            None => break,
        }
    }
    assert_eq!(pages, 3, "five imports, two at a time");
    ours.reverse();
    assert_eq!(
        rows.iter().map(|r| r["id"].clone()).collect::<Vec<_>>(),
        ours.iter()
            .map(|id| json!(id.to_string()))
            .collect::<Vec<_>>(),
        "newest first, which is the order an operator looks for the import they just ran"
    );
    assert!(
        !rows.iter().any(|r| r["id"] == json!(theirs.to_string())),
        "another workspace's imports are not in this list"
    );
    // The rows are reports, not stubs: the list screen shows a status and a filename.
    assert_eq!(
        (
            &rows[0]["status"],
            &rows[0]["filename"],
            &rows[0]["errors_recorded"]
        ),
        (&json!("pending"), &json!("a.csv"), &json!(0)),
    );

    // A cursor that is not one is a 422, not a silent first page.
    app.get(&format!("{}?cursor=%25%25%25", imports_uri(a)))
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    // And a workspace this caller is not in is a 404, not an empty page.
    app.get(&imports_uri(b))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

// ---- Task 8: one import per workspace, the row cap, and the boot sweep -------------------------

use std::time::Duration;

use common::api_key;
use nm::{
    contacts::import::routes::{
        MAX_IMPORT_BYTES, MAX_IMPORT_ROWS, sweep_orphaned_uploads, sweep_running_imports,
    },
    http::rate_limit::IMPORT_PER_MINUTE,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn set_status(pool: &PgPool, import: Uuid, status: &str) {
    sqlx::query("update imports set status = $2 where id = $1")
        .bind(import)
        .bind(status)
        .execute(pool)
        .await
        .expect("set status");
}

async fn error_reason(pool: &PgPool, import: Uuid) -> (Option<String>, bool) {
    sqlx::query_as("select error_reason, finished_at is not null from imports where id = $1")
        .bind(import)
        .fetch_one(pool)
        .await
        .expect("read import")
}

/// A file of `rows` addresses: a run long enough that it is still inside its transaction while the
/// second request below is made.
fn big_csv(rows: usize) -> String {
    let mut csv = String::from("email\n");
    for i in 0..rows {
        csv.push_str(&format!("c{i}@example.com\n"));
    }
    csv
}

/// Waits until a session holds an advisory lock in this test's database.
///
/// This is what makes the two tests below races rather than hopes: the second start is made while
/// the first is holding the workspace, not merely soon after it began. Bounded, and silent when it
/// gives up - with the lock replaced by a status check no lock ever appears, and a test that hung
/// there would be a worse red than the assertions that follow.
///
/// `#[sqlx::test]` gives each test its own database and `pg_locks.database` scopes an advisory
/// lock to one, so another test's lock is never this one's.
async fn wait_for_the_workspace_lock(pool: &PgPool) {
    for _ in 0..200 {
        let held: i64 = sqlx::query_scalar(
            "select count(*) from pg_locks where locktype = 'advisory' \
             and database = (select oid from pg_database where datname = current_database())",
        )
        .fetch_one(pool)
        .await
        .expect("read pg_locks");
        if held > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// An import that is `running` holds its workspace, and the refusal says which one it is: "an
/// import is already running" with no id sends an operator to look through the list for it.
#[sqlx::test]
async fn a_second_import_in_the_same_workspace_is_409(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let mine = upload_kept(&app, ws, "mine.csv", "email\nada@example.com\n").await;
    let body = json!({"mapping": [field_target("email")]});
    let theirs = insert_import(&pool, ws).await;
    set_status(&pool, theirs, "running").await;

    let r = app.post(&start_uri(ws, mine)).json(&body).await;
    r.assert_status(StatusCode::CONFLICT);
    let p: Value = r.json();
    assert_eq!(p["type"], "urn:nm:problem:import-in-progress");
    assert!(
        p["detail"].as_str().unwrap().contains(&theirs.to_string()),
        "the refusal names the import that is holding the workspace: {p}"
    );
    // Refused, not ruined: the import is still `pending`, so the operator may start it when the
    // other one is over, and nothing of its file was written.
    assert_eq!(import_row(&pool, mine).await.0, "pending");
    assert_eq!(count(&pool, "contacts").await, 0);

    // Control: with that import over, the same request runs. The refusal above was the running
    // import and not the route, the mapping or the file.
    set_status(&pool, theirs, "failed").await;
    app.post(&start_uri(ws, mine))
        .json(&body)
        .await
        .assert_status_ok();
    assert_eq!(emails(&pool, ws).await, ["ada@example.com"]);
}

/// TWO TASKS, TWO CONNECTIONS, BOTH INSIDE THE HANDLER. Called one after the other this proves
/// nothing: the first start would be over before the second began, and a `select status` check
/// would pass them both - neither request can see the other's `running`, because the run holds it
/// inside its own transaction until the COMMIT. That is why the guard is a lock.
#[sqlx::test]
async fn two_concurrent_starts_do_not_both_pass_the_check(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let first_app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let second_app = member(&pool, ws, "ed2@example.com", Role::Editor).await;
    let first = upload_kept(&first_app, ws, "a.csv", &big_csv(5_000)).await;
    let second = upload_kept(&second_app, ws, "b.csv", "email\nbea@example.com\n").await;
    let body = json!({"mapping": [field_target("email")]});

    let (ran, refused) = tokio::join!(
        async { first_app.post(&start_uri(ws, first)).json(&body).await },
        async {
            wait_for_the_workspace_lock(&pool).await;
            second_app.post(&start_uri(ws, second)).json(&body).await
        },
    );

    ran.assert_status_ok();
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        refused.json::<Value>()["type"],
        "urn:nm:problem:import-in-progress"
    );
    assert_eq!(count(&pool, "contacts").await, 5_000, "one of them ran");
    // The refused one kept everything it needs to be started again.
    assert_eq!(import_row(&pool, second).await.0, "pending");
    assert!(upload_path(second).exists());
    std::fs::remove_file(upload_path(second)).unwrap();
}

/// The lock is keyed on the workspace, not on the instance: one tenant's million rows do not stop
/// another tenant importing. The positive control for the two refusals above.
#[sqlx::test]
async fn another_workspace_may_import_at_the_same_time(pool: PgPool) {
    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let app_a = member(&pool, a, "ed-a@example.com", Role::Editor).await;
    let app_b = member(&pool, b, "ed-b@example.com", Role::Editor).await;
    let theirs = upload_kept(&app_a, a, "a.csv", &big_csv(5_000)).await;
    let ours = upload_kept(&app_b, b, "b.csv", "email\nbea@example.com\n").await;
    let body = json!({"mapping": [field_target("email")]});

    let (first, second) = tokio::join!(
        async { app_a.post(&start_uri(a, theirs)).json(&body).await },
        async {
            wait_for_the_workspace_lock(&pool).await;
            app_b.post(&start_uri(b, ours)).json(&body).await
        },
    );

    first.assert_status_ok();
    second.assert_status_ok();
    assert_eq!(emails(&pool, b).await, ["bea@example.com"]);
    assert_eq!(import_row(&pool, ours).await.0, "complete");
}

/// A file over the cap is refused at the upload, naming the cap - not accepted and timed out two
/// minutes into a run nobody can stop.
#[sqlx::test]
async fn a_file_over_the_row_cap_is_refused_naming_the_cap(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;

    // Control: a file of the same shape under the cap is accepted, so the refusal below is the cap
    // and not the request.
    uploaded(&app, ws, "small.csv", &big_csv(999)).await;

    // THE EDGE, FROM THE ACCEPTING SIDE. A header and exactly the cap in data rows is the largest
    // file that passes: the count is line feeds, with one discounted for a header the sniff has
    // not read yet. The refusal below is this file plus one row and nothing else, so the two
    // together pin the boundary rather than straddling it.
    let row = "a\n";
    uploaded(
        &app,
        ws,
        "at-the-cap.csv",
        &format!("email\n{}", row.repeat(MAX_IMPORT_ROWS as usize)),
    )
    .await;

    let over = format!("email\n{}", row.repeat(MAX_IMPORT_ROWS as usize + 1));
    let r = upload(&app, ws, "one-row-over.csv", &over).await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    let p: Value = r.json();
    assert_eq!(p["type"], "urn:nm:problem:too-many-rows");
    assert!(
        p["detail"]
            .as_str()
            .unwrap()
            .contains(&MAX_IMPORT_ROWS.to_string()),
        "the refusal names the cap: {p}"
    );
    assert_eq!(
        count(&pool, "imports").await,
        2,
        "the two accepted files were recorded and the refused one was not"
    );
}

/// A row stuck in `running` for ever is worse than a clear failure: `GET /imports` shows it as in
/// progress, and the workspace's guard has to be argued with to start anything else.
#[sqlx::test]
async fn a_running_import_is_swept_to_failed_at_boot(pool: PgPool) {
    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let stuck = insert_import(&pool, a).await;
    let stuck_elsewhere = insert_import(&pool, b).await;
    let pending = insert_import(&pool, a).await;
    let complete = insert_import(&pool, a).await;
    set_status(&pool, stuck, "running").await;
    set_status(&pool, stuck_elsewhere, "running").await;
    set_status(&pool, complete, "complete").await;

    assert_eq!(
        sweep_running_imports(&pool).await.expect("sweep"),
        2,
        "every workspace's, because a boot has no tenant"
    );
    for id in [stuck, stuck_elsewhere] {
        assert_eq!(import_row(&pool, id).await.0, "failed");
        let (reason, finished) = error_reason(&pool, id).await;
        assert!(
            reason.as_deref().is_some_and(|r| r.contains("restart")),
            "the failure says why: {reason:?}"
        );
        assert!(finished, "a swept import is over, and the report says when");
    }
    // Nothing else was touched: a pending import is still startable and a finished one keeps its
    // report.
    assert_eq!(import_row(&pool, pending).await.0, "pending");
    assert_eq!(import_row(&pool, complete).await.0, "complete");
    assert_eq!(error_reason(&pool, pending).await.0, None);

    assert_eq!(
        sweep_running_imports(&pool).await.expect("second boot"),
        0,
        "the second boot has nothing left to sweep"
    );
}

/// The other half of the boot pass: the uploaded files of imports that are over. Half a gigabyte
/// of someone's contacts is not left on disk because a process died between the COMMIT and the
/// unlink.
#[sqlx::test]
async fn the_upload_of_an_import_that_is_over_is_swept_and_the_others_are_kept(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let (over, pending, stranger, unremovable) = (
        insert_import(&pool, ws).await,
        insert_import(&pool, ws).await,
        Uuid::now_v7(),
        insert_import(&pool, ws).await,
    );
    set_status(&pool, over, "complete").await;
    set_status(&pool, unremovable, "failed").await;
    for id in [over, pending, stranger] {
        std::fs::write(upload_path(id), b"email\n").unwrap();
    }
    // A path the unlink cannot take, so the sweep has one more target than it can remove. The
    // count it answers with is what the boot logs, and a log that reports work it did not do is
    // how a failed unlink becomes invisible.
    std::fs::create_dir(upload_path(unremovable)).unwrap();

    assert_eq!(
        sweep_orphaned_uploads(&pool).await.expect("sweep"),
        1,
        "the file it removed, not the two it meant to"
    );
    assert!(
        upload_path(unremovable).exists(),
        "the one it could not take is still there, and was not counted"
    );
    assert!(
        !upload_path(over).exists(),
        "the import is over; nothing will read its file again"
    );
    assert!(
        upload_path(pending).exists(),
        "a pending import's file is exactly what its start reads"
    );
    assert!(
        upload_path(stranger).exists(),
        "a file this database cannot account for is not ours to delete"
    );
    std::fs::remove_file(upload_path(pending)).unwrap();
    std::fs::remove_file(upload_path(stranger)).unwrap();
    std::fs::remove_dir(upload_path(unremovable)).unwrap();
}

/// AN OPEN THAT FAILS FOR ANY OTHER REASON LEAVES THE IMPORT RETRYABLE. `import-file-gone` is
/// permanent and must be: no retry brings a deleted upload back. A descriptor blip or a
/// permissions change is not that, and failing a 512 MB upload for one is unrecoverable damage
/// done by the error path.
#[cfg(unix)]
#[sqlx::test]
async fn a_start_whose_upload_cannot_be_opened_leaves_the_import_pending(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let app = member(&pool, ws, "ed@example.com", Role::Editor).await;
    let import = upload_kept(&app, ws, "book.csv", "email\nada@example.com\n").await;
    // A symbolic link to itself where the upload was: `open` fails with ELOOP, which is not
    // `NotFound` and is not the file being gone.
    let path = upload_path(import);
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&path, &path).unwrap();

    let r = app
        .post(&start_uri(ws, import))
        .json(&json!({"mapping": [field_target("email")]}))
        .await;
    r.assert_status(StatusCode::INTERNAL_SERVER_ERROR);
    let p: Value = r.json();
    assert_eq!(p["type"], "urn:nm:problem:internal");
    assert_eq!(
        import_row(&pool, import).await.0,
        "pending",
        "still startable: the file may well open on the next request"
    );
    assert_eq!(error_reason(&pool, import).await.0, None);
    std::fs::remove_file(&path).unwrap();
}

/// THE 413 HAS TO REACH THE BROWSER AS A PROBLEM BODY. A refusal taken only once the body has been
/// read answers an upload that is still being sent, and what the browser gets then is a network
/// error, where the UI's `problemText` never runs. So this test speaks HTTP itself: it declares a
/// body over the cap and reads what comes back off the socket.
///
/// **And it puts the body in flight**, which is the half that matters. With no body sent, the
/// handler returns without consuming the request and the close is a clean FIN — every client reads
/// the answer, and the test would pass against a server that could never answer a real browser. A
/// browser sending half a gigabyte leaves bytes in the server's receive buffer when it gives up on
/// them, and a close with unread data is an RST, which can take a response the client has received
/// but not yet read. So this writes megabytes of the declared body and reads at the same time, as
/// a browser's socket does.
#[sqlx::test]
async fn an_upload_declaring_more_than_the_cap_is_413_with_a_problem_body(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let mut owner = TestApp::new(pool.clone()).await;
    owner.login("owner-acme@example.com", PW).await;
    let key = api_key(&owner, ws, "editor").await;
    let url = owner.server.server_address().expect("a real HTTP server");
    let host = format!(
        "{}:{}",
        url.host_str().expect("host"),
        url.port().expect("port")
    );

    let socket = tokio::net::TcpStream::connect(&host)
        .await
        .expect("connect");
    let (mut reader, mut writer) = socket.into_split();
    let declared = MAX_IMPORT_BYTES * 2;
    let head = format!(
        "POST /api/v1/workspaces/{ws}/imports HTTP/1.1\r\n\
         Host: {host}\r\n\
         Authorization: Bearer {key}\r\n\
         Content-Type: multipart/form-data; boundary=b\r\n\
         Content-Length: {declared}\r\n\
         Connection: close\r\n\r\n"
    );
    writer.write_all(head.as_bytes()).await.expect("send");
    // THE SENDER OWNS THE WRITE HALF, AND DROPPING IT IS WHAT CLOSES IT. That matters for more
    // than tidiness: it is the whole mechanism of this test's mutation. With the precheck removed
    // the server reads the body instead of refusing it, this task writes its 8 MB and ends, the
    // write half drops and sends FIN, the multipart parser hits the end of a body that promised a
    // gigabyte, and only then does the server answer. The `refused` assertion below is what
    // notices.
    let sending = tokio::spawn(async move {
        let chunk = vec![b'x'; 64 * 1024];
        // 8 MB of a declared 1 GB, which is more than the socket's buffers hold, so the server is
        // refusing a body that is genuinely in flight. The server stops reading, so these writes
        // fail partway: that failure is the condition being tested, not an error.
        //
        // THIS DEPENDS ON THE HOST'S SOCKET BUFFERS BEING SMALLER THAN 8 MiB with a receiver that
        // never reads, which is true of every Linux default and of the CI container. A host that
        // auto-tunes past that would take the whole body, and the assertion below would red
        // saying so rather than passing quietly: read that red as "this premise no longer holds
        // here, raise the figure", not as a flake and not as a defect in the route.
        for sent in 0..128u64 {
            if writer.write_all(&chunk).await.is_err() {
                return (sent * 64 * 1024, true);
            }
        }
        (128 * 64 * 1024, false)
    });
    // Read as it arrives, as a browser does, and keep whatever arrived: a reset after the answer
    // is still an answer, and a reset that takes the answer leaves nothing to assert on - which is
    // the failure this test exists to catch.
    //
    // Bounded, because the other failure is silence: a server that answers only once it has read
    // the body never answers this request at all, and that has to be a failure and not a hang.
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await
    .expect("the refusal came before the body, not after it");
    let (sent, refused) = sending.await.expect("the sending task");
    // The test's own configuration, pinned: the body really was in flight and the server really
    // did close under it. Without this the test could quietly become the easy case - a body the
    // kernel swallowed whole, closed with a clean FIN, which any client reads.
    assert!(
        refused,
        "the server took all {sent} bytes of the body, so this did not test a close over an \
         unread one"
    );
    let answer = String::from_utf8_lossy(&raw).into_owned();

    assert!(
        answer.starts_with("HTTP/1.1 413 "),
        "the refusal comes before the body: {answer}"
    );
    assert!(
        answer
            .to_ascii_lowercase()
            .contains("content-type: application/problem+json"),
        "{answer}"
    );
    let body = answer.split("\r\n\r\n").nth(1).unwrap_or_default();
    let p: Value = serde_json::from_str(body.trim()).expect("a problem body reached the client");
    assert_eq!(p["type"], "urn:nm:problem:import-too-large");
    assert!(
        p["detail"]
            .as_str()
            .unwrap()
            .contains(&MAX_IMPORT_BYTES.to_string()),
        "the refusal names the limit: {p}"
    );
    assert_eq!(count(&pool, "imports").await, 0);
}

/// Nothing counted uploads before, and an upload writes up to half a gigabyte to disk.
#[sqlx::test]
async fn an_upload_past_the_per_minute_limit_is_429(pool: PgPool) {
    let (ws, other) = (
        workspace(&pool, "acme").await,
        workspace(&pool, "other").await,
    );
    let ed = create_user(&pool, "ed@example.com", PW, false).await;
    add_member(&pool, ws, ed, Role::Editor).await;
    add_member(&pool, other, ed, Role::Editor).await;
    let mut app = TestApp::new(pool.clone()).await;
    app.login("ed@example.com", PW).await;

    // The window is spent through the limiter the route consults, rather than by sending sixty
    // files: what is being tested here is that the route is behind it, and at a file each that
    // would be a minute of the suite spent proving arithmetic `rate_limit.rs` already tests.
    for _ in 0..IMPORT_PER_MINUTE {
        app.state.import_limit.check(ws);
    }

    let r = upload(&app, ws, "over.csv", "email\nbea@example.com\n").await;
    r.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.json::<Value>()["type"], "urn:nm:problem:rate-limited");
    assert_eq!(
        r.headers()["ratelimit-remaining"],
        "0",
        "the refusal carries the headers that say when to come back"
    );
    assert_eq!(
        count(&pool, "imports").await,
        0,
        "the refused upload recorded nothing"
    );

    // Control, and the shape of the limit: the window is one workspace's, so another workspace is
    // not refused - and the same caller, the same app and the same limiter are what prove it.
    uploaded(&app, other, "fine.csv", "email\nada@example.com\n").await;
    assert_eq!(count(&pool, "imports").await, 1);
}
