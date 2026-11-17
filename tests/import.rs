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
