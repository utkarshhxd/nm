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
    let body = uploaded(
        &app,
        ws,
        "book.csv",
        "email,notes\nada@example.com,SECRET-CELL\n",
    )
    .await;
    let import: Uuid = body["id"].as_str().unwrap().parse().unwrap();

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
        about_the_import.len(),
        1,
        "the upload is logged and the two reads are not: {rows:?}"
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
