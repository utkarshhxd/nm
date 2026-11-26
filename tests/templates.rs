// SPDX-License-Identifier: AGPL-3.0-only

//! Migration 0032: `templates` and `brand_kits`. Raw SQL on purpose: "exactly one of tree and
//! source" is the database's guarantee, so a handler that happens to check first proves nothing.

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn workspace(pool: &PgPool, slug: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into workspaces (id, slug, name) values ($1, $2, $2)")
        .bind(id)
        .bind(slug)
        .execute(pool)
        .await
        .expect("insert workspace");
    id
}

async fn template(
    pool: &PgPool,
    ws: Uuid,
    kind: &str,
    tree: Option<serde_json::Value>,
    source: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "insert into templates (id, workspace_id, name, kind, tree, source)
         values ($1, $2, 't', $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(kind)
    .bind(tree)
    .bind(source)
    .execute(pool)
    .await
    .map(|_| ())
}

fn is_check_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|e| e.code().map(|c| c.into_owned()))
        .as_deref()
        == Some("23514")
}

#[sqlx::test]
async fn tables_exist_and_reject_a_tree_on_a_raw_template(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let tree = json!({"blocks": []});

    // A raw template carrying a tree, and a block template carrying none.
    for (kind, t, s) in [
        ("mjml", Some(tree.clone()), Some("<mjml/>")),
        ("html", Some(tree.clone()), None),
        ("blocks", None, None),
        ("blocks", None, Some("<mjml/>")),
        ("blocks", Some(tree.clone()), Some("<mjml/>")),
        ("mjml", None, None),
        ("html", Some(tree.clone()), Some("<p/>")),
        ("mjml", Some(tree.clone()), None),
    ] {
        let err = template(&pool, ws, kind, t, s)
            .await
            .expect_err("must be refused");
        assert!(is_check_violation(&err), "{kind}: {err}");
    }

    // The three well-formed shapes are accepted.
    template(&pool, ws, "blocks", Some(tree), None)
        .await
        .expect("blocks");
    template(&pool, ws, "mjml", None, Some("<mjml/>"))
        .await
        .expect("mjml");
    template(&pool, ws, "html", None, Some("<p>hi</p>"))
        .await
        .expect("html");

    // An unknown kind is refused too.
    let err = template(&pool, ws, "docx", None, Some("x"))
        .await
        .expect_err("kind");
    assert!(is_check_violation(&err), "{err}");
}

#[sqlx::test]
async fn a_workspace_has_at_most_one_brand_kit(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    let insert = || {
        sqlx::query("insert into brand_kits (workspace_id) values ($1)")
            .bind(ws)
            .execute(&pool)
    };
    insert().await.expect("first kit");
    let err = insert().await.expect_err("second kit");
    assert_eq!(
        err.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23505")
    );
}

/// A child table's composite foreign key needs a unique key on exactly `(workspace_id, id)`: creating
/// the child fails without it, and the reference then refuses another workspace's template.
#[sqlx::test]
async fn templates_workspace_id_id_key_backs_a_composite_foreign_key(pool: PgPool) {
    sqlx::query(
        "create table child (workspace_id uuid not null, template_id uuid not null,
         foreign key (workspace_id, template_id) references templates (workspace_id, id))",
    )
    .execute(&pool)
    .await
    .expect("composite foreign key needs templates_workspace_id_id_key");

    let (a, b) = (workspace(&pool, "a").await, workspace(&pool, "b").await);
    let t = Uuid::now_v7();
    sqlx::query("insert into templates (id, workspace_id, name, kind, source) values ($1, $2, 't', 'html', 'x')")
        .bind(t)
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    let insert = |ws: Uuid| {
        sqlx::query("insert into child values ($1, $2)")
            .bind(ws)
            .bind(t)
            .execute(&pool)
    };
    insert(a).await.expect("own workspace");
    let err = insert(b).await.expect_err("other workspace");
    assert_eq!(
        err.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23503")
    );
}

#[sqlx::test]
async fn json_columns_must_be_objects(pool: PgPool) {
    let ws = workspace(&pool, "acme").await;
    for bad in [json!(null), json!(7), json!([])] {
        let err = template(&pool, ws, "blocks", Some(bad.clone()), None)
            .await
            .expect_err("tree");
        assert!(is_check_violation(&err), "tree {bad}: {err}");
        for sql in [
            "insert into brand_kits (workspace_id, palette) values ($1, $2)",
            "insert into brand_kits (workspace_id, footer) values ($1, $2)",
        ] {
            let err = sqlx::query(sql)
                .bind(ws)
                .bind(bad.clone())
                .execute(&pool)
                .await;
            let err = err.expect_err("brand kit");
            assert!(is_check_violation(&err), "{sql} {bad}: {err}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The API (task R7). Everything above this line is the schema's own guarantees, in raw SQL.

mod common;

use axum::http::StatusCode;
use common::{PW, TestApp, add_member, create_user, create_workspace};
use nm::workspaces::roles::Role;

struct Tenant {
    id: Uuid,
    admin: TestApp,
    editor: TestApp,
    viewer: TestApp,
}

async fn tenant(pool: &PgPool, slug: &str) -> Tenant {
    let owner = create_user(pool, &format!("owner-{slug}@example.com"), PW, false).await;
    let id = create_workspace(pool, owner, slug).await;
    let member = |role: Role| {
        let email = format!("{role:?}-{slug}@example.com").to_lowercase();
        async move {
            let user = create_user(pool, &email, PW, false).await;
            add_member(pool, id, user, role).await;
            let mut app = TestApp::new(pool.clone()).await;
            app.login(&email, PW).await;
            app
        }
    };
    Tenant {
        id,
        admin: member(Role::Admin).await,
        editor: member(Role::Editor).await,
        viewer: member(Role::Viewer).await,
    }
}

fn templates_path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/templates")
}

fn template_path(ws: Uuid, id: Uuid) -> String {
    format!("{}/{id}", templates_path(ws))
}

fn preview_path(ws: Uuid, id: Uuid) -> String {
    format!("{}/preview", template_path(ws, id))
}

fn kit_path(ws: Uuid) -> String {
    format!("/api/v1/workspaces/{ws}/brand-kit")
}

fn error_fields(r: &axum_test::TestResponse) -> Vec<String> {
    r.json::<serde_json::Value>()["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("no errors array: {}", r.text()))
        .iter()
        .map(|e| e["field"].as_str().unwrap().to_owned())
        .collect()
}

/// A one-block document whose only run is `tag`.
fn doc(tag: &str) -> serde_json::Value {
    json!({"blocks": [{"type": "Text", "runs": [{"type": "Tag", "source": tag}]}]})
}

async fn custom_field(pool: &PgPool, ws: Uuid, key: &str, label: &str, kind: &str) {
    sqlx::query(
        "insert into custom_fields (id, workspace_id, key, label, kind) values ($1,$2,$3,$4,$5)",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .bind(key)
    .bind(label)
    .bind(kind)
    .execute(pool)
    .await
    .expect("custom field");
}

async fn archive_fields(pool: &PgPool, ws: Uuid) {
    sqlx::query("update custom_fields set archived_at = now() where workspace_id = $1")
        .bind(ws)
        .execute(pool)
        .await
        .expect("archive");
}

/// Creates a blocks template and returns its id.
async fn create(t: &Tenant, name: &str, tree: serde_json::Value) -> Uuid {
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": name, "kind": "blocks", "tree": tree}))
        .await;
    r.assert_status(StatusCode::CREATED);
    Uuid::parse_str(r.json::<serde_json::Value>()["id"].as_str().unwrap()).unwrap()
}

/// Previews what is STORED: no document in the body.
async fn preview_stored(t: &Tenant, id: Uuid) -> axum_test::TestResponse {
    t.viewer
        .post(&preview_path(t.id, id))
        .json(&json!({}))
        .await
}

#[sqlx::test]
async fn another_workspaces_template_is_404(pool: PgPool) {
    let mine = tenant(&pool, "acme").await;
    let theirs = tenant(&pool, "other").await;
    let id = create(&mine, "mine", doc("{{ first_name }}")).await;

    // Asked for through the other workspace's path: 404, never 403 and never the document.
    for r in [
        theirs.viewer.get(&template_path(theirs.id, id)).await,
        theirs
            .editor
            .patch(&template_path(theirs.id, id))
            .json(&json!({"name": "stolen"}))
            .await,
        theirs.editor.delete(&template_path(theirs.id, id)).await,
        theirs
            .viewer
            .post(&preview_path(theirs.id, id))
            .json(&json!({}))
            .await,
    ] {
        r.assert_status(StatusCode::NOT_FOUND);
        assert!(!r.text().contains("mine"), "leaked the name: {}", r.text());
    }

    // And asked for through MY path by a member of the other workspace: still 404, because the
    // membership lookup fails before the row is read.
    theirs
        .viewer
        .get(&template_path(mine.id, id))
        .await
        .assert_status(StatusCode::NOT_FOUND);

    // THE LIST, which is the route whose payload is every row rather than one. Its `where`
    // clause is a separate statement from `load`'s and nothing above exercises it: deleted, a
    // viewer of one workspace receives the other's names, block trees and merge-tag sources, and
    // a 404 on the by-id route is no comfort at all.
    let r = theirs.viewer.get(&templates_path(theirs.id)).await;
    r.assert_status_ok();
    assert!(
        r.json::<serde_json::Value>()["data"]
            .as_array()
            .expect("data")
            .is_empty(),
        "another workspace's templates in the list: {}",
        r.text()
    );
    assert!(!r.text().contains("mine"), "leaked: {}", r.text());
    // The owning workspace does see it, so the assertion above is about the filter and not about
    // an empty table.
    let r = mine.viewer.get(&templates_path(mine.id)).await;
    r.assert_status_ok();
    assert!(r.text().contains("mine"), "{}", r.text());
}

#[sqlx::test]
async fn every_mutating_verb_is_role_gated(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = create(&t, "t", doc("{{ first_name }}")).await;

    // POST, PATCH and DELETE — all three, not one of three.
    t.viewer
        .post(&templates_path(t.id))
        .json(&json!({"name": "v", "kind": "blocks", "tree": doc("{{ email }}")}))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    t.viewer
        .patch(&template_path(t.id, id))
        .json(&json!({"name": "renamed"}))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    t.viewer
        .delete(&template_path(t.id, id))
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // A viewer may still read and preview.
    t.viewer
        .get(&template_path(t.id, id))
        .await
        .assert_status_ok();
    preview_stored(&t, id).await.assert_status_ok();

    // The editor may do all three.
    t.editor
        .patch(&template_path(t.id, id))
        .json(&json!({"name": "renamed"}))
        .await
        .assert_status_ok();
    t.editor
        .delete(&template_path(t.id, id))
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

#[sqlx::test]
async fn the_brand_kit_is_admin_gated(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let kit = json!({"logo_url": null, "palette": {}, "font_stack": null, "footer": {}});
    // An editor composes emails; an admin configures what every email is legally obliged to say.
    t.editor
        .put(&kit_path(t.id))
        .json(&kit)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    t.viewer
        .put(&kit_path(t.id))
        .json(&kit)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    t.admin
        .put(&kit_path(t.id))
        .json(&kit)
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn a_footer_the_renderer_would_refuse_is_refused_at_the_brand_kit(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    // The same policy compile applies to every blocks template's footer: a kit holding one would
    // make each save and preview fail with an error naming the template, not the kit.
    for (key, bad) in [
        ("text", "x ~nmmt~0~tmmn~"),
        ("address", "x \u{e000}"),
        ("text", "x \u{1}"),
        ("address", "x \u{3}"),
    ] {
        let r = t
            .admin
            .put(&kit_path(t.id))
            .json(
                &json!({"logo_url": null, "palette": {}, "font_stack": null, "footer": {key: bad}}),
            )
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error_fields(&r), [format!("footer.{key}")], "{bad:?}");
    }
    // A brace is not reserved: it is escaped, and typed tag syntax stays literal.
    t.admin
        .put(&kit_path(t.id))
        .json(&json!({"logo_url": null, "palette": {}, "font_stack": null,
            "footer": {"text": "Hi {{ first_name }}"}}))
        .await
        .assert_status_ok();
}

#[sqlx::test]
async fn a_logo_with_a_user_name_is_refused_with_a_sentence_that_is_true(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let r = t
        .admin
        .put(&kit_path(t.id))
        .json(&json!({"logo_url": "https://u:p@cdn.example/x.png", "palette": {}, "font_stack": null, "footer": {}}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["logo_url"]);
    assert!(
        r.text().contains("no user name or password"),
        "{}",
        r.text()
    );
}

#[sqlx::test]
async fn saving_a_tag_naming_an_unknown_field_is_422_with_the_field_name(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "t", "kind": "blocks", "tree": doc("{{ nope }}")}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["tree"]);
    assert!(
        r.text().contains("nope"),
        "the refusal must name the field: {}",
        r.text()
    );
}

#[sqlx::test]
async fn saving_a_tag_naming_an_archived_field_is_422(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    custom_field(&pool, t.id, "colour", "Teal", "text").await;
    archive_fields(&pool, t.id).await;
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "t", "kind": "blocks", "tree": doc("{{ colour }}")}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["tree"]);
    assert!(
        r.text().contains("archived"),
        "a new tag on an archived field is told why: {}",
        r.text()
    );
}

#[sqlx::test]
async fn a_template_compiled_before_archiving_still_renders(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    custom_field(&pool, t.id, "colour", "Teal", "text").await;
    let id = create(&t, "t", doc("{{ colour }}")).await;
    archive_fields(&pool, t.id).await;

    // The other half of R2's rule: a NEW tag on an archived field is refused, an already
    // compiled one still renders. The contact keeps its value until the field is purged.
    let r = preview_stored(&t, id).await;
    r.assert_status_ok();
    let body = r.json::<serde_json::Value>();
    assert!(body["html"].as_str().unwrap().contains("Teal"));
    // And the operator is told, rather than left to find out when the field means nothing.
    let findings = body["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0]["rule"], "archived_field");
    assert!(findings[0]["message"].as_str().unwrap().contains("colour"));
}

#[sqlx::test]
async fn both_parts_are_filled_from_one_stored_ast_after_a_field_is_archived(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    custom_field(&pool, t.id, "colour", "Teal", "text").await;
    let id = create(&t, "t", doc("{{ colour }}")).await;
    archive_fields(&pool, t.id).await;

    // THE divergence R5 was restructured to make impossible: compiled separately, the archive
    // lands between the two calls and the tag fails in one part while the other still renders
    // it — one message disagreeing with itself about the recipient.
    let r = preview_stored(&t, id).await;
    r.assert_status_ok();
    let body = r.json::<serde_json::Value>();
    let (html, text) = (
        body["html"].as_str().unwrap(),
        body["text"].as_str().unwrap(),
    );
    assert!(html.contains("Teal"), "html: {html}");
    assert!(text.contains("Teal"), "text: {text}");
    assert!(!text.contains("~nmmt~"), "text carries markers: {text}");
}

#[sqlx::test]
async fn a_field_kind_change_after_compile_is_detected_not_mis_rendered(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    custom_field(&pool, t.id, "colour", "Teal", "text").await;
    let id = create(&t, "t", doc("{{ colour }}")).await;
    // The API refuses a kind change, so this is the row written by another path — which is the
    // case the stored AST cannot be trusted about.
    sqlx::query("update custom_fields set kind = 'select', options = $2 where workspace_id = $1")
        .bind(t.id)
        .bind(json!(["a"]))
        .execute(&pool)
        .await
        .unwrap();

    let r = preview_stored(&t, id).await;
    r.assert_status(StatusCode::CONFLICT);
    assert!(r.text().contains("template-stale"), "{}", r.text());
    assert!(r.text().contains("colour"), "names the field: {}", r.text());
}

#[sqlx::test]
async fn a_bogus_marker_format_version_is_refused(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = create(&t, "t", doc("{{ first_name }}")).await;
    preview_stored(&t, id).await.assert_status_ok();

    sqlx::query(
        "update templates set compiled = jsonb_set(compiled, '{marker_format_version}', '99') \
         where id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();

    // Not guessed at: a boundary marker read as a field marker drops the wrong range of a
    // message, silently.
    let r = preview_stored(&t, id).await;
    r.assert_status(StatusCode::CONFLICT);
    assert!(r.text().contains("marker format 99"), "{}", r.text());
}

#[sqlx::test]
async fn an_over_deep_stored_ast_is_refused(pool: PgPool) {
    let t = tenant(&pool, "acme").await;

    // Written straight to the column, never through `compile`: `serde`'s `Deserialize` on
    // `tag::Node` re-validates nothing, so the depth cap every renderer's recursion assumes is
    // not a property of a row.
    let mut deep = json!([]);
    for _ in 0..=nm::template::tag::MAX_DEPTH {
        deep = json!([{"Present": {"key": "first_name", "body": deep}}]);
    }

    // BOTH ASTs, one each into its own template. The plain-text part has a tree of its own and
    // `render_text` recurses over it at send exactly as the HTML path does over `nodes`, so a
    // test that plants into one of the two leaves the other's re-check held by nothing. A fresh
    // template per arm, because a row is only interesting here while the arm's own plant is the
    // only thing wrong with it.
    for path in ["{nodes}", "{text,nodes}"] {
        let id = create(&t, path, doc("{{ first_name }}")).await;
        preview_stored(&t, id).await.assert_status_ok();
        sqlx::query(
            "update templates set compiled = jsonb_set(compiled, $3::text[], $2) where id = $1",
        )
        .bind(id)
        .bind(&deep)
        .bind(path)
        .execute(&pool)
        .await
        .unwrap();
        let r = preview_stored(&t, id).await;
        r.assert_status(StatusCode::CONFLICT);
        assert!(r.text().contains("deeper than"), "{path}: {}", r.text());
    }
}

#[sqlx::test]
async fn changing_the_brand_kit_alone_invalidates_the_stored_renditions(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = create(&t, "t", doc("{{ first_name }}")).await;
    preview_stored(&t, id).await.assert_status_ok();

    t.admin
        .put(&kit_path(t.id))
        .json(&json!({
            "logo_url": null,
            "palette": {"accent": "#ff0000"},
            "font_stack": null,
            "footer": {}
        }))
        .await
        .assert_status_ok();

    // The kit is emitted into the MJML the classifier ran over, so the stored classification now
    // describes a document this template no longer produces. A stale classification is a
    // security bug, not a cache miss.
    let r = preview_stored(&t, id).await;
    r.assert_status(StatusCode::CONFLICT);
    assert!(r.text().contains("template-stale"), "{}", r.text());

    // Saving the template again compiles it against the kit as it is now.
    t.editor
        .patch(&template_path(t.id, id))
        .json(&json!({"name": "t"}))
        .await
        .assert_status_ok();
    preview_stored(&t, id).await.assert_status_ok();
}

#[sqlx::test]
async fn preview_returns_html_text_and_findings(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let tree = json!({"blocks": [
        {"type": "Heading", "level": 1, "runs": [{"type": "Text", "text": "Hi", "marks": []}]},
        {"type": "Button", "label": "Go", "href": ""}
    ]});
    let id = create(&t, "t", tree).await;

    let r = preview_stored(&t, id).await;
    r.assert_status_ok();
    let body = r.json::<serde_json::Value>();
    let html = body["html"].as_str().expect("html");
    assert!(html.contains("<html"), "a whole document: {html}");
    assert!(body["text"].as_str().expect("text").contains("Hi"));
    let findings = body["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0]["rule"], "empty_href");
    assert_eq!(findings[0]["check"], "link");
    assert_eq!(findings[0]["severity"], "warning");
    assert_eq!(findings[0]["at"], json!([1]), "a PATH, not an index");
}

#[sqlx::test]
async fn preview_takes_the_unsaved_document_when_one_is_sent(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = create(&t, "t", doc("{{ first_name }}")).await;
    let r = t
        .viewer
        .post(&preview_path(t.id, id))
        .json(&json!({"tree": {"blocks": [
            {"type": "Text", "runs": [{"type": "Text", "text": "unsaved", "marks": []}]}]}}))
        .await;
    r.assert_status_ok();
    assert!(
        r.json::<serde_json::Value>()["text"]
            .as_str()
            .unwrap()
            .contains("unsaved")
    );
    // Nothing was written: what is stored is still the saved document.
    let stored = preview_stored(&t, id).await;
    assert!(
        !stored.json::<serde_json::Value>()["text"]
            .as_str()
            .unwrap()
            .contains("unsaved")
    );
}

#[sqlx::test]
async fn a_dangerous_merge_tag_refusal_names_the_operators_own_text(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    // The byte offset `classify` returns is into `mrml`'s OUTPUT, a document the operator has
    // never seen. The refusal has to point at what they wrote.
    let tree = json!({"blocks": [
        {"type": "Button", "label": "Go", "href": "https://{{ first_name }}.example.com/"}]});
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "t", "kind": "blocks", "tree": tree}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        r.text().contains("{{ first_name }}"),
        "must quote the operator's own tag: {}",
        r.text()
    );

    // And for a raw template, where the source IS what they typed.
    let source = "<mjml><mj-body><mj-raw><script>var a = \"{{ email }}\";</script></mj-raw>\
                  </mj-body></mjml>";
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "raw", "kind": "mjml", "source": source}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.text().contains("{{ email }}"), "{}", r.text());
}

#[sqlx::test]
async fn an_empty_raw_source_is_refused(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    // `templates.source is not null` admits the empty string. RULING: the handler refuses it. An
    // empty raw template compiles to an email with nothing in it, and `lint` cannot check it at
    // all, so it is a template that could never be previewed.
    //
    // The MESSAGE is asserted, not only the field: an empty MJML source also fails in
    // `mrml::parse`, which reports the same field, so a test reading only the field name passes
    // with this rule deleted. An empty `html` source has no second refusal behind it at all and
    // would be saved.
    for kind in ["mjml", "html"] {
        for source in ["", "   \n "] {
            let r = t
                .editor
                .post(&templates_path(t.id))
                .json(&json!({"name": "t", "kind": kind, "source": source}))
                .await;
            r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(error_fields(&r), ["source"], "{kind} {source:?}");
            assert!(
                r.text().contains("cannot be empty"),
                "{kind} {source:?}: {}",
                r.text()
            );
        }
    }
}

#[sqlx::test]
async fn a_link_scheme_outside_the_allowlist_is_refused_at_save(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    // `static_url` would rewrite it to `#` at render, so the operator's link would be silently
    // dead rather than refused.
    let tree = json!({"blocks": [
        {"type": "Button", "label": "Go", "href": "javascript:alert(1)"}]});
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "t", "kind": "blocks", "tree": tree}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["tree.blocks[0].href"]);

    // A relative href is NOT refused: it names no scheme, and M2c's unsubscribe links are
    // written that way.
    t.editor
        .post(&templates_path(t.id))
        .json(
            &json!({"name": "rel", "kind": "blocks", "tree": json!({"blocks": [
            {"type": "Button", "label": "Go", "href": "/u/abc"}]})}),
        )
        .await
        .assert_status(StatusCode::CREATED);
}

#[sqlx::test]
async fn too_many_blocks_is_refused(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let flat: Vec<serde_json::Value> = (0..nm::template::routes::MAX_BLOCKS + 1)
        .map(|_| json!({"type": "Divider"}))
        .collect();
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "t", "kind": "blocks", "tree": {"blocks": flat}}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["tree"]);

    // Width inside a `Columns` counts too: depth is bounded, width is not.
    let wide: Vec<serde_json::Value> = (0..nm::template::routes::MAX_BLOCKS)
        .map(|_| json!({"type": "Divider"}))
        .collect();
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "t", "kind": "blocks", "tree": {"blocks": [
            {"type": "Columns", "children": [wide]}]}}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test]
async fn the_brand_kit_is_one_row_per_workspace(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let other = tenant(&pool, "other").await;

    // A workspace with no row has an EMPTY kit, never a 404: the screen must be reachable on day
    // one, when no workspace has saved one.
    let r = t.viewer.get(&kit_path(t.id)).await;
    r.assert_status_ok();
    r.assert_json(&json!({"logo_url": null, "palette": {}, "font_stack": null, "footer": {}}));

    let kit = json!({
        "logo_url": "https://cdn.example.com/logo.png",
        "palette": {"background": "#fff", "text": "#111111", "accent": "#2563EB",
                    "accent_text": "#ffffff"},
        "font_stack": "Helvetica Neue, Arial, sans-serif",
        "footer": {"text": "Acme", "address": "1 Way, Town"}
    });
    t.admin
        .put(&kit_path(t.id))
        .json(&kit)
        .await
        .assert_status_ok();
    // Saved twice: a PUT replaces the one row rather than adding a second.
    t.admin
        .put(&kit_path(t.id))
        .json(&kit)
        .await
        .assert_status_ok();
    let rows: i64 = sqlx::query_scalar("select count(*) from brand_kits where workspace_id = $1")
        .bind(t.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
    t.viewer.get(&kit_path(t.id)).await.assert_json(&kit);

    // And it is the WORKSPACE's: the other one still has none.
    other
        .viewer
        .get(&kit_path(other.id))
        .await
        .assert_json(&json!({"logo_url": null, "palette": {}, "font_stack": null, "footer": {}}));
}

#[sqlx::test]
async fn unknown_brand_kit_keys_survive_a_save(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let kit = json!({
        "logo_url": null,
        "palette": {"accent": "#2563eb", "from_a_newer_version": "#abc"},
        "font_stack": null,
        "footer": {"address": "1 Way", "legalese": "..."}
    });
    t.admin
        .put(&kit_path(t.id))
        .json(&kit)
        .await
        .assert_status_ok();
    // A kit written by a newer version must survive a round trip through an older screen.
    t.viewer.get(&kit_path(t.id)).await.assert_json(&kit);
}

#[sqlx::test]
async fn a_font_stack_that_breaks_out_of_css_is_refused(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    for font in [
        "Arial; } body { background: url(https://evil.example/x) ",
        "Arial}",
        "url(javascript:alert(1))",
        "Arial;",
    ] {
        let r = t
            .admin
            .put(&kit_path(t.id))
            .json(&json!({"logo_url": null, "palette": {}, "font_stack": font, "footer": {}}))
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error_fields(&r), ["font_stack"], "{font}");
    }
}

#[sqlx::test]
async fn a_logo_url_must_be_http_or_https(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    // The link allowlist is wider than this field: `mailto:` and `tel:` are meaningless as an
    // image source (contract section 11).
    for logo in [
        "mailto:a@b.example",
        "tel:+1",
        "javascript:alert(1)",
        "not a url",
    ] {
        let r = t
            .admin
            .put(&kit_path(t.id))
            .json(&json!({"logo_url": logo, "palette": {}, "font_stack": null, "footer": {}}))
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error_fields(&r), ["logo_url"], "{logo}");
    }
}

#[sqlx::test]
async fn a_colour_outside_the_frozen_format_is_refused(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    for (bad, field) in [
        (json!({"accent": "red"}), "palette.accent"),
        (json!({"background": "#12345"}), "palette.background"),
        (json!({"text": "rgb(0,0,0)"}), "palette.text"),
        // A non-string would otherwise be stored, returned, and silently ignored by the emitter.
        (json!({"accent_text": 7}), "palette.accent_text"),
    ] {
        let r = t
            .admin
            .put(&kit_path(t.id))
            .json(&json!({"logo_url": null, "palette": bad, "font_stack": null, "footer": {}}))
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error_fields(&r), [field], "{bad}");
    }
}

#[sqlx::test]
async fn a_raw_html_template_is_never_rendered_through_mrml(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "h", "kind": "html",
                      "source": "<p>Hi {{ first_name }}</p>"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let id = Uuid::parse_str(r.json::<serde_json::Value>()["id"].as_str().unwrap()).unwrap();
    let r = preview_stored(&t, id).await;
    r.assert_status_ok();
    let body = r.json::<serde_json::Value>();
    let html = body["html"].as_str().unwrap();
    assert!(html.starts_with("<p>Hi Sam</p>"), "{html}");
    assert!(!html.contains("<mjml"), "mrml must not have run: {html}");
    // The plain-text part of a raw template comes from the SUBSTITUTED html, never the marked one.
    assert_eq!(body["text"].as_str().unwrap(), "Hi Sam");

    // Neither of R6's checks has a subject here, and an empty list would read in the panel as
    // "checked, nothing wrong". It says which.
    let findings = body["findings"].as_array().expect("findings");
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0]["rule"], "not_checked");
    assert_eq!(findings[0]["severity"], "warning");
    assert_eq!(findings[0]["at"], json!(null));
}

#[sqlx::test]
async fn a_blocks_template_stores_both_renditions_and_a_raw_one_stores_only_the_html(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let blocks = create(&t, "b", doc("{{ first_name }}")).await;
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "h", "kind": "html", "source": "<p>{{ first_name }}</p>"}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let raw = Uuid::parse_str(r.json::<serde_json::Value>()["id"].as_str().unwrap()).unwrap();

    let part = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "select jsonb_typeof(compiled->'text') from templates where id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("compiled")
        }
    };
    // A blocks template stores the marked plain text BESIDE the marked HTML, written in one
    // transaction from one AST. A raw one has no block tree to build a text skeleton from, so its
    // text part is derived from the substituted HTML per contact and nothing is stored.
    assert_eq!(part(blocks).await, "object");
    assert_eq!(part(raw).await, "null");

    // And both carry the marker format the renderer wrote them with, so a later build can refuse
    // a document whose shape it does not speak.
    for id in [blocks, raw] {
        let version: i32 = sqlx::query_scalar(
            "select (compiled->>'marker_format_version')::int from templates where id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(version as u32, nm::template::render::MARKER_FORMAT_VERSION);
        let key: String =
            sqlx::query_scalar("select compiled->>'fingerprint' from templates where id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!key.is_empty());
    }
}

#[sqlx::test]
async fn the_dangerous_tag_is_the_nth_one_the_operator_wrote_not_the_nth_brace(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    // A conditional's CLOSING tag takes no marker slot of its own, so the tag that marker 1
    // stands for is `{{ email }}` and not `{{/ first_name }}`.
    let source = "<mjml><mj-body><mj-text>{{# first_name }}hi{{/ first_name }}</mj-text>\
                  <mj-raw><script>var a = \"{{ email }}\";</script></mj-raw></mj-body></mjml>";
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "t", "kind": "mjml", "source": source}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.text().contains("{{ email }}"), "{}", r.text());
    assert!(!r.text().contains("{{/ first_name }}"), "{}", r.text());

    // A conditional's BOUNDARY marker carries an `o` or `c` after its index. Read as part of the
    // index it is not a number at all, so without the trim the refusal names no tag — which is
    // what the assertion below holds against.
    let source = "<mjml><mj-body><mj-raw><script>{{# first_name }}x{{/ first_name }}</script>\
                  </mj-raw></mj-body></mjml>";
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "t2", "kind": "mjml", "source": source}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.text().contains("{{# first_name }}"), "{}", r.text());
}

#[sqlx::test]
async fn a_patch_that_sends_null_is_refused_rather_than_ignored(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = create(&t, "t", doc("{{ first_name }}")).await;
    for field in ["name", "tree", "source"] {
        let r = t
            .editor
            .patch(&template_path(t.id, id))
            .json(&json!({ field: serde_json::Value::Null }))
            .await;
        r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error_fields(&r), [field]);
    }
    // And the document column that does not belong to this kind is refused too.
    let r = t
        .editor
        .patch(&template_path(t.id, id))
        .json(&json!({"source": "<p>x</p>"}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test]
async fn a_raw_mjml_template_with_a_quoted_fallback_in_a_link_saves_and_previews(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    // A quoted fallback inside an attribute: compile lifts it into the AST, so `mrml` parses the
    // MARKED source and the template is valid. The check must read that same string; handed the
    // raw source, `mrml` cannot parse it and every preview of a valid template was a 422.
    let source = "<mjml><mj-body><mj-section><mj-column><mj-button                   href=\"https://x.example/?n={{ first_name | \"friend\" }}\">Go</mj-button>                  </mj-column></mj-section></mj-body></mjml>";
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "t", "kind": "mjml", "source": source}))
        .await;
    r.assert_status(StatusCode::CREATED);
    let id = Uuid::parse_str(r.json::<serde_json::Value>()["id"].as_str().unwrap()).unwrap();

    let r = preview_stored(&t, id).await;
    r.assert_status_ok();
    // The link check never ran on a raw template, so an empty list would read as a pass.
    let body = r.json::<serde_json::Value>();
    let findings = body["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{body}");
    assert_eq!(findings[0]["rule"], "not_checked");
    assert!(findings[0]["at"].is_null());
    assert!(
        findings[0]["message"]
            .as_str()
            .unwrap()
            .contains("links are not checked"),
        "{body}"
    );
}

#[sqlx::test]
async fn a_source_past_the_cap_is_refused(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let big = "x".repeat(nm::template::routes::MAX_SOURCE + 1);
    let r = t
        .editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "t", "kind": "html", "source": big}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_fields(&r), ["source"]);
    // One byte under is fine, so the refusal is about the cap and not about the shape.
    t.editor
        .post(&templates_path(t.id))
        .json(&json!({"name": "ok", "kind": "html",
                      "source": "x".repeat(nm::template::routes::MAX_SOURCE)}))
        .await
        .assert_status(StatusCode::CREATED);
}

#[sqlx::test]
async fn a_stored_template_may_be_resaved_while_it_still_names_an_archived_field(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    custom_field(&pool, t.id, "colour", "Teal", "text").await;
    custom_field(&pool, t.id, "shade", "Jade", "text").await;
    let id = create(&t, "t", doc("{{ colour }}")).await;
    archive_fields(&pool, t.id).await;

    // An admin edits the brand kit, which is the ordinary way a fingerprint moves.
    t.admin
        .put(&kit_path(t.id))
        .json(&json!({"logo_url": null, "palette": {"accent": "#ff0000"},
                      "font_stack": null, "footer": {}}))
        .await
        .assert_status_ok();
    let r = preview_stored(&t, id).await;
    r.assert_status(StatusCode::CONFLICT);
    assert!(r.text().contains("save it again"), "{}", r.text());

    // THE CURE THE 409 NAMES MUST WORK. Before this, the save refused the tag the stored version
    // already had, so the preview told the operator to do the one thing that 422s and the
    // template could be neither previewed nor saved nor reached.
    t.editor
        .patch(&template_path(t.id, id))
        .json(&json!({"name": "t"}))
        .await
        .assert_status_ok();
    let r = preview_stored(&t, id).await;
    r.assert_status_ok();
    let findings = r.json::<serde_json::Value>()["findings"]
        .as_array()
        .expect("findings")
        .clone();
    assert!(
        findings.iter().any(|f| f["rule"] == "archived_field"),
        "the surviving reference is surfaced, not silently kept: {findings:?}"
    );

    // The line holds: a reference that is NEW in the submitted document is still refused, even
    // though another archived field is grandfathered in the same template.
    let r = t
        .editor
        .patch(&template_path(t.id, id))
        .json(&json!({"tree": doc("{{ shade }}")}))
        .await;
    r.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.text().contains("shade"), "{}", r.text());
    assert!(r.text().contains("archived"), "{}", r.text());
}

#[sqlx::test]
async fn template_preview_has_its_own_budget_not_the_segment_builders(pool: PgPool) {
    let t = tenant(&pool, "acme").await;
    let id = create(&t, "t", doc("{{ first_name }}")).await;

    // One past the segment preview's whole window. Sharing one counter let an operator typing in
    // the block editor starve the segment builder in the same workspace, and the other way round;
    // they are unrelated features and this is what says so.
    for i in 1..=nm::http::rate_limit::PREVIEW_PER_MINUTE + 1 {
        let r = preview_stored(&t, id).await;
        r.assert_status_ok();
        assert!(
            r.headers().contains_key("ratelimit-remaining"),
            "request {i} lost the RateLimit headers"
        );
    }
    // And the segment builder's own window is untouched by all of that: its refusal is about the
    // empty rule document it was sent, not about a budget somebody else spent.
    let r = t
        .viewer
        .post(&format!("/api/v1/workspaces/{}/segments/preview", t.id))
        .json(&json!({"rules": {"match": "all", "rules": []}}))
        .await;
    assert_ne!(
        r.status_code(),
        StatusCode::TOO_MANY_REQUESTS,
        "the template previews spent the segment builder's budget: {}",
        r.text()
    );
}
