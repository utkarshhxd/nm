// SPDX-License-Identifier: AGPL-3.0-only

//! The segment compiler (spec M1b sections 4.2 to 4.5), asserted against real rows. Nothing here
//! compares generated SQL text for equality: a query can be spelled right and mean the wrong
//! thing, and every protection in `segments::compile` has been shown to go red here when it was
//! removed.
//!
//! One fixture serves every test: three contacts in one workspace, chosen so that each operator
//! has a contact it must match, a contact it must not, and a contact with the value *absent* —
//! which is the half that three-valued logic silently drops.

mod common;

use std::collections::HashMap;

use common::{PW, create_user, create_workspace};
use nm::{
    config::SecretKey,
    crypto::secrets::Secrets,
    custom_fields::FieldKind,
    segments::{
        compile::push_rules,
        rules::{Group, Refs},
    },
};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

/// Every contact in the fixture's workspace. A compiled document that returns all three has lost
/// its restriction and become "every contact in the workspace".
const ALL: [&str; 3] = ["a@acme.com", "b@acme.com", "c@globex.com"];

struct Fx {
    ws: Uuid,
    other_ws: Uuid,
    list: Uuid,
    other_list: Uuid,
    tag: Uuid,
    other_tag: Uuid,
}

/// The definitions the rule parser resolves `attr:` against. The last key is not a key any
/// `validate_key` would accept; it is here because the compiler must be safe even if one ever
/// reached it.
fn attrs() -> HashMap<String, FieldKind> {
    [
        ("plan", FieldKind::Text),
        ("note", FieldKind::Text),
        ("seats", FieldKind::Number),
        ("joined", FieldKind::Date),
        ("seen", FieldKind::DateTime),
        ("vip", FieldKind::Boolean),
        ("tier", FieldKind::Select),
        ("interests", FieldKind::MultiSelect),
        (INJECTION, FieldKind::Text),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}

/// The one list and tag id the two database-free tests name.
const NAMED: &str = "7b0f2c3e-0000-4000-8000-000000000001";

/// [`refs`] for a test with no pool: the definitions, and `ids` as both this workspace's lists and
/// its tags, so a document naming one of them parses.
fn attr_refs(ids: &[&str]) -> Refs {
    let ids: std::collections::HashSet<Uuid> = ids
        .iter()
        .map(|s| Uuid::parse_str(s).expect("a test uuid"))
        .collect();
    Refs {
        kinds: attrs(),
        lists: ids.clone(),
        tags: ids,
    }
}

/// What the parser resolves a document against: the definitions above, plus the lists and tags
/// this workspace really holds — read back from the database rather than listed here, so a test
/// cannot name a row that is not there without saying so in its own body.
async fn refs(pool: &PgPool, ws: Uuid) -> Refs {
    let lists = sqlx::query_scalar("select id from lists where workspace_id = $1")
        .bind(ws)
        .fetch_all(pool)
        .await
        .expect("the workspace's lists");
    let tags = sqlx::query_scalar("select id from tags where workspace_id = $1")
        .bind(ws)
        .fetch_all(pool)
        .await
        .expect("the workspace's tags");
    Refs {
        kinds: attrs(),
        lists: lists.into_iter().collect(),
        tags: tags.into_iter().collect(),
    }
}

const INJECTION: &str = "plan'); drop table contacts; --";

async fn insert_contact(
    pool: &PgPool,
    ws: Uuid,
    email: &str,
    name: (&str, &str),
    tz: Option<&str>,
    attributes: Value,
    age_days: i32,
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into contacts (id, workspace_id, email, first_name, last_name, timezone, \
         attributes, created_at, updated_at) \
         values ($1, $2, $3, $4, $5, $6, $7::jsonb, now() - make_interval(days => $8), \
                 now() - make_interval(days => $8))",
    )
    .bind(id)
    .bind(ws)
    .bind(email)
    .bind(name.0)
    .bind(name.1)
    .bind(tz)
    .bind(attributes)
    .bind(age_days)
    .execute(pool)
    .await
    .expect("insert contact");
    id
}

async fn insert_list(pool: &PgPool, ws: Uuid, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into lists (id, workspace_id, name) values ($1, $2, $3)")
        .bind(id)
        .bind(ws)
        .bind(name)
        .execute(pool)
        .await
        .expect("insert list");
    id
}

async fn insert_tag(pool: &PgPool, ws: Uuid, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("insert into tags (id, workspace_id, name) values ($1, $2, $3)")
        .bind(id)
        .bind(ws)
        .bind(name)
        .execute(pool)
        .await
        .expect("insert tag");
    id
}

/// A live suppression row. `ws` is `None` for an instance-wide block. An address row needs a hash
/// (it is what lets an erasure tombstone outlive its plaintext); the value is irrelevant to this
/// set path, which matches on plaintext like `mailable_list_members`, but it must differ per
/// address or the `suppressions_live` unique index rejects the second row in a workspace.
async fn suppress(pool: &PgPool, ws: Option<Uuid>, scope: &str, value: &str) -> Uuid {
    let id = Uuid::now_v7();
    let address = scope == "address";
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, domain, email_hmac, reason) \
         values ($1, $2, $3, $4, $5, $6, 'bounced')",
    )
    .bind(id)
    .bind(ws)
    .bind(scope)
    .bind(address.then_some(value))
    .bind((!address).then_some(value))
    .bind(address.then(|| value.as_bytes().to_vec()))
    .execute(pool)
    .await
    .expect("insert suppression");
    id
}

/// `a` has every attribute and matches the positive rules; `c` has every attribute with the other
/// value; `b` has **none** of them, no tag, no list row and a NULL timezone — it is the contact
/// every negative operator must match and a negated join would lose.
///
/// The other workspace is not a decoy that merely exists: its list and tag have a real member, and
/// it holds a suppression on `a@acme.com` and on the whole of `acme.com`. Without those rows a
/// tenancy test proves only "an id with no rows matches nothing", which is not tenancy.
async fn seed(pool: &PgPool) -> Fx {
    let owner = create_user(pool, "owner@example.com", PW, false).await;
    let ws = create_workspace(pool, owner, "acme").await;
    let other_ws = create_workspace(pool, owner, "globex").await;

    let a = insert_contact(
        pool,
        ws,
        "a@acme.com",
        ("Ann", "Lee"),
        Some("Europe/Paris"),
        json!({"plan": "pro", "note": "50% off", "seats": 5, "joined": "2026-01-10",
               "seen": "2026-10-01T12:00:00Z", "vip": true, "tier": "gold",
               "interests": ["rust", "sql"]}),
        0,
    )
    .await;
    insert_contact(pool, ws, "b@acme.com", ("Bob", ""), None, json!({}), 0).await;
    let c = insert_contact(
        pool,
        ws,
        "c@globex.com",
        ("Cy", "Ray"),
        Some("America/New_York"),
        json!({"plan": "free", "note": "5000 off", "seats": 50, "joined": "2020-01-01",
               "seen": "2020-01-01T12:00:00Z", "vip": false, "tier": "silver",
               "interests": ["go"]}),
        400,
    )
    .await;
    // The other tenant: the same address, in another workspace, matching everything.
    let other_a = insert_contact(
        pool,
        other_ws,
        "a@acme.com",
        ("Ann", "Lee"),
        Some("Europe/Paris"),
        json!({"plan": "pro"}),
        0,
    )
    .await;

    let list = insert_list(pool, ws, "news").await;
    let other_list = insert_list(pool, other_ws, "news").await;
    for (contact, status) in [(a, "subscribed"), (c, "unsubscribed")] {
        sqlx::query(
            "insert into list_memberships (workspace_id, contact_id, list_id, status) \
             values ($1, $2, $3, $4)",
        )
        .bind(ws)
        .bind(contact)
        .bind(list)
        .bind(status)
        .execute(pool)
        .await
        .expect("insert membership");
    }

    // The other workspace's list has a member of its own, so a rule naming it is asking about a
    // populated list belonging to someone else, not about an empty one.
    sqlx::query(
        "insert into list_memberships (workspace_id, contact_id, list_id, status) \
         values ($1, $2, $3, 'subscribed')",
    )
    .bind(other_ws)
    .bind(other_a)
    .bind(other_list)
    .execute(pool)
    .await
    .expect("insert other membership");

    let tag = insert_tag(pool, ws, "vip").await;
    let other_tag = insert_tag(pool, other_ws, "vip").await;
    for (w, contact, t) in [(ws, a, tag), (other_ws, other_a, other_tag)] {
        sqlx::query(
            "insert into contact_tags (workspace_id, contact_id, tag_id) values ($1, $2, $3)",
        )
        .bind(w)
        .bind(contact)
        .bind(t)
        .execute(pool)
        .await
        .expect("insert contact_tag");
    }

    // `c` alone is suppressed in this workspace, by address.
    suppress(pool, Some(ws), "address", "c@globex.com").await;
    // The other tenant suppresses `a@acme.com`, and the whole of `acme.com`. Neither may make any
    // contact of *this* workspace look suppressed: the suppression arms correlate on an address,
    // not on a contact id, and the same address legitimately exists in both workspaces, so
    // `s.workspace_id = contacts.workspace_id` is the only thing separating the two tenants.
    suppress(pool, Some(other_ws), "address", "a@acme.com").await;
    suppress(pool, Some(other_ws), "domain", "acme.com").await;

    Fx {
        ws,
        other_ws,
        list,
        other_list,
        tag,
        other_tag,
    }
}

fn all(rules: Vec<Value>) -> Value {
    json!({"match": "all", "rules": rules})
}

/// Parses `doc`, compiles it onto a workspace-scoped query and returns the matching addresses.
/// Every document that passes through here is also checked to re-serialise to exactly what was
/// parsed, so the absent-versus-null distinction `rules`'s `present` deserializer introduced is
/// asserted on every case below, not only in its own test.
async fn matching(pool: &PgPool, ws: Uuid, doc: &Value) -> Vec<String> {
    matching_against(pool, ws, doc, &refs(pool, ws).await).await
}

/// [`matching`] with the references the parser resolves against supplied by the caller, for the
/// one test that has to compile a rule the parser would now refuse.
async fn matching_against(pool: &PgPool, ws: Uuid, doc: &Value, refs: &Refs) -> Vec<String> {
    let group = Group::parse(doc, refs).expect("the document parses");
    assert_eq!(
        serde_json::to_value(&group).expect("a parsed group serialises"),
        *doc,
        "a parsed document must re-serialise to exactly what was parsed"
    );
    let mut q = QueryBuilder::<Postgres>::new("select email from contacts where workspace_id = ");
    q.push_bind(ws);
    q.push(" and ");
    push_rules(&mut q, &group);
    q.push(" order by email");
    q.build_query_scalar::<String>()
        .fetch_all(pool)
        .await
        .expect("the compiled query runs")
}

/// Asserts the exact matching set, and that it is a *strict* subset of the workspace: a document
/// that matches every contact has lost its restriction.
async fn check(pool: &PgPool, ws: Uuid, rule: Value, want: &[&str]) {
    let doc = all(vec![rule.clone()]);
    let got = matching(pool, ws, &doc).await;
    assert_eq!(got, want, "{rule}");
    assert!(
        got.len() < ALL.len(),
        "{rule} matched every contact in the workspace"
    );
}

/// Every operator in spec section 4.3, each with a contact it matches and one it does not. The
/// strict-subset assertion in `expect` is the structural guard: no rule here may compile to
/// "every contact in the workspace".
#[sqlx::test]
async fn every_operator_compiles_and_restricts_beyond_workspace_id(pool: PgPool) {
    let fx = seed(&pool).await;
    let id = |u: Uuid| json!(u.to_string());
    let cases: Vec<(Value, Vec<&str>)> = vec![
        // text columns
        (
            json!({"field": "email", "op": "eq", "value": "a@acme.com"}),
            vec!["a@acme.com"],
        ),
        (
            json!({"field": "email", "op": "neq", "value": "a@acme.com"}),
            vec!["b@acme.com", "c@globex.com"],
        ),
        (
            json!({"field": "email", "op": "contains", "value": "@acme"}),
            vec!["a@acme.com", "b@acme.com"],
        ),
        (
            json!({"field": "email", "op": "not_contains", "value": "acme"}),
            vec!["c@globex.com"],
        ),
        (
            json!({"field": "email", "op": "starts_with", "value": "a@"}),
            vec!["a@acme.com"],
        ),
        (
            json!({"field": "email", "op": "ends_with", "value": "globex.com"}),
            vec!["c@globex.com"],
        ),
        (
            json!({"field": "first_name", "op": "contains", "value": "nn"}),
            vec!["a@acme.com"],
        ),
        // `last_name` is `not null default ''`, so "not set" must mean the empty string too.
        (
            json!({"field": "last_name", "op": "is_set"}),
            vec!["a@acme.com", "c@globex.com"],
        ),
        (
            json!({"field": "last_name", "op": "is_not_set"}),
            vec!["b@acme.com"],
        ),
        (
            json!({"field": "timezone", "op": "is_set"}),
            vec!["a@acme.com", "c@globex.com"],
        ),
        (
            json!({"field": "timezone", "op": "is_not_set"}),
            vec!["b@acme.com"],
        ),
        (
            json!({"field": "email_domain", "op": "eq", "value": "globex.com"}),
            vec!["c@globex.com"],
        ),
        // timestamptz columns
        (
            json!({"field": "created_at", "op": "in_last_days", "value": 30}),
            vec!["a@acme.com", "b@acme.com"],
        ),
        (
            json!({"field": "created_at", "op": "before", "value": "2026-06-01"}),
            vec!["c@globex.com"],
        ),
        (
            json!({"field": "updated_at", "op": "before", "value": "2026-06-01"}),
            vec!["c@globex.com"],
        ),
        // text and select attributes
        (
            json!({"field": "attr:plan", "op": "eq", "value": "pro"}),
            vec!["a@acme.com"],
        ),
        (
            json!({"field": "attr:plan", "op": "is_set"}),
            vec!["a@acme.com", "c@globex.com"],
        ),
        (
            json!({"field": "attr:plan", "op": "is_not_set"}),
            vec!["b@acme.com"],
        ),
        (
            json!({"field": "attr:tier", "op": "eq", "value": "gold"}),
            vec!["a@acme.com"],
        ),
        (
            json!({"field": "attr:tier", "op": "neq", "value": "gold"}),
            vec!["b@acme.com", "c@globex.com"],
        ),
        // number attributes: the cast follows the definition, not the value
        (
            json!({"field": "attr:seats", "op": "eq", "value": 5}),
            vec!["a@acme.com"],
        ),
        (
            json!({"field": "attr:seats", "op": "gt", "value": 10}),
            vec!["c@globex.com"],
        ),
        (
            json!({"field": "attr:seats", "op": "gte", "value": 50}),
            vec!["c@globex.com"],
        ),
        (
            json!({"field": "attr:seats", "op": "lt", "value": 10}),
            vec!["a@acme.com"],
        ),
        (
            json!({"field": "attr:seats", "op": "lte", "value": 5}),
            vec!["a@acme.com"],
        ),
        // "50" sorts before "5" as text; only a numeric cast orders these right.
        (
            json!({"field": "attr:seats", "op": "gt", "value": 6}),
            vec!["c@globex.com"],
        ),
        // date and datetime attributes
        (
            json!({"field": "attr:joined", "op": "before", "value": "2026-01-01"}),
            vec!["c@globex.com"],
        ),
        (
            json!({"field": "attr:joined", "op": "after", "value": "2026-01-01"}),
            vec!["a@acme.com"],
        ),
        (
            json!({"field": "attr:joined", "op": "on", "value": "2026-01-10"}),
            vec!["a@acme.com"],
        ),
        (
            json!({"field": "attr:joined", "op": "in_last_days", "value": 365}),
            vec!["a@acme.com"],
        ),
        (
            json!({"field": "attr:seen", "op": "before", "value": "2026-01-01"}),
            vec!["c@globex.com"],
        ),
        (
            json!({"field": "attr:seen", "op": "on", "value": "2026-10-01"}),
            vec!["a@acme.com"],
        ),
        (
            json!({"field": "attr:seen", "op": "in_last_days", "value": 365}),
            vec!["a@acme.com"],
        ),
        // boolean attributes
        (
            json!({"field": "attr:vip", "op": "is_true"}),
            vec!["a@acme.com"],
        ),
        (
            json!({"field": "attr:vip", "op": "is_false"}),
            vec!["c@globex.com"],
        ),
        (
            json!({"field": "attr:vip", "op": "is_set"}),
            vec!["a@acme.com", "c@globex.com"],
        ),
        (
            json!({"field": "attr:vip", "op": "is_not_set"}),
            vec!["b@acme.com"],
        ),
        // list, tag, suppressed
        (
            json!({"field": "list", "op": "member_of", "value": id(fx.list)}),
            vec!["a@acme.com", "c@globex.com"],
        ),
        (
            json!({"field": "list", "op": "member_of", "value": id(fx.list), "status": "subscribed"}),
            vec!["a@acme.com"],
        ),
        (
            json!({"field": "tag", "op": "has", "value": id(fx.tag)}),
            vec!["a@acme.com"],
        ),
        (
            json!({"field": "suppressed", "op": "is_true"}),
            vec!["c@globex.com"],
        ),
        (
            json!({"field": "suppressed", "op": "is_false"}),
            vec!["a@acme.com", "b@acme.com"],
        ),
    ];
    for (rule, want) in cases {
        check(&pool, fx.ws, rule, &want).await;
    }
}

/// Section 4.5, the whole of it: for **every** negative operator, a contact with zero related rows
/// and a contact with a NULL or absent value both match. `b` is both at once.
#[sqlx::test]
async fn every_negative_operator_matches_a_missing_row_and_a_null_value(pool: PgPool) {
    let fx = seed(&pool).await;
    let id = |u: Uuid| json!(u.to_string());
    let cases: Vec<(Value, Vec<&str>)> = vec![
        // NULL column, and an empty-string column
        (
            json!({"field": "timezone", "op": "neq", "value": "Europe/Paris"}),
            vec!["b@acme.com", "c@globex.com"],
        ),
        (
            json!({"field": "timezone", "op": "not_contains", "value": "Paris"}),
            vec!["b@acme.com", "c@globex.com"],
        ),
        (
            json!({"field": "last_name", "op": "is_not_set"}),
            vec!["b@acme.com"],
        ),
        // absent attribute, every kind
        (
            json!({"field": "attr:plan", "op": "neq", "value": "pro"}),
            vec!["b@acme.com", "c@globex.com"],
        ),
        (
            json!({"field": "attr:note", "op": "not_contains", "value": "50%"}),
            vec!["b@acme.com", "c@globex.com"],
        ),
        (
            json!({"field": "attr:seats", "op": "neq", "value": 5}),
            vec!["b@acme.com", "c@globex.com"],
        ),
        (
            json!({"field": "attr:tier", "op": "neq", "value": "gold"}),
            vec!["b@acme.com", "c@globex.com"],
        ),
        // `attr:vip is_false` is deliberately NOT here: section 4.5 scopes this rule to negations
        // of positive operators, and `is_false` is one arm of a partition. See
        // `a_boolean_attribute_is_a_four_way_partition`.
        (
            json!({"field": "attr:interests", "op": "has_none", "value": ["rust"]}),
            vec!["b@acme.com", "c@globex.com"],
        ),
        (
            json!({"field": "attr:interests", "op": "is_not_set"}),
            vec!["b@acme.com"],
        ),
        // zero related rows: `b` has no membership row and no tag row at all
        (
            json!({"field": "list", "op": "not_member_of", "value": id(fx.list)}),
            vec!["b@acme.com"],
        ),
        (
            json!({"field": "list", "op": "not_member_of", "value": id(fx.list), "status": "subscribed"}),
            vec!["b@acme.com", "c@globex.com"],
        ),
        (
            json!({"field": "tag", "op": "not_has", "value": id(fx.tag)}),
            vec!["b@acme.com", "c@globex.com"],
        ),
        // zero suppression rows
        (
            json!({"field": "suppressed", "op": "is_false"}),
            vec!["a@acme.com", "b@acme.com"],
        ),
    ];
    for (rule, want) in cases {
        assert!(
            want.contains(&"b@acme.com"),
            "{rule}: the contact with nothing set must match a negative operator"
        );
        check(&pool, fx.ws, rule, &want).await;
    }
}

/// Section 4.4's test obligation. The key is bound, so Postgres looks for a JSON key spelled
/// exactly like that — finds none, and the statement was never two statements.
#[sqlx::test]
async fn an_attribute_key_that_is_sql_compiles_runs_and_matches_nothing(pool: PgPool) {
    let fx = seed(&pool).await;
    let field = format!("attr:{INJECTION}");
    for rule in [
        json!({"field": field, "op": "eq", "value": "pro"}),
        json!({"field": field, "op": "is_set"}),
        json!({"field": field, "op": "contains", "value": "pro"}),
    ] {
        let doc = all(vec![rule.clone()]);
        assert_eq!(
            matching(&pool, fx.ws, &doc).await,
            Vec::<String>::new(),
            "{rule}"
        );
    }
    // `is_not_set` on that key matches everyone, which is the proof it was read as a key and not
    // executed: the table is still there and still holds every contact.
    let doc = all(vec![json!({"field": field, "op": "is_not_set"})]);
    assert_eq!(matching(&pool, fx.ws, &doc).await, ALL);
    let n: i64 = sqlx::query_scalar("select count(*) from contacts")
        .fetch_one(&pool)
        .await
        .expect("`contacts` still exists");
    assert_eq!(n, 4, "three contacts in the workspace and one decoy");
}

/// Section 4.3: `%`, `_` and `\` in a *value* are escaped, with an explicit `ESCAPE` clause.
/// Unescaped, `50%` and `%` both become "matches every row that has a value at all".
#[sqlx::test]
async fn a_like_metacharacter_in_a_value_matches_itself_not_everything(pool: PgPool) {
    let fx = seed(&pool).await;
    // `a` holds "50% off"; `c` holds "5000 off", which an unescaped `%50%%` also matches.
    for (op, value) in [
        ("contains", "50%"),
        ("contains", "%"),
        ("starts_with", "50%"),
        ("ends_with", "% off"),
        ("contains", "50% o"),
    ] {
        check(
            &pool,
            fx.ws,
            json!({"field": "attr:note", "op": op, "value": value}),
            &["a@acme.com"],
        )
        .await;
    }
    // `_` is the other metacharacter, and `\` is the escape itself.
    for value in ["5_% off", "50\\% off"] {
        check(
            &pool,
            fx.ws,
            json!({"field": "attr:note", "op": "contains", "value": value}),
            &[],
        )
        .await;
    }
    // Negated, the escaping must hold too, and must not drop the contact with no note.
    check(
        &pool,
        fx.ws,
        json!({"field": "attr:note", "op": "not_contains", "value": "%"}),
        &["b@acme.com", "c@globex.com"],
    )
    .await;
}

/// Section 4.3: a multi_select is an array, so it compiles through `jsonb_exists_any` and not
/// `->>`. `has_none` must match a contact whose key is absent entirely.
#[sqlx::test]
async fn multi_select_compiles_through_jsonb_exists_any(pool: PgPool) {
    let fx = seed(&pool).await;
    let cases: Vec<(&str, Value, Vec<&str>)> = vec![
        ("has_any", json!(["rust"]), vec!["a@acme.com"]),
        (
            "has_any",
            json!(["rust", "go"]),
            vec!["a@acme.com", "c@globex.com"],
        ),
        ("has_any", json!(["cobol"]), vec![]),
        (
            "has_none",
            json!(["rust"]),
            vec!["b@acme.com", "c@globex.com"],
        ),
        ("has_none", json!(["rust", "go"]), vec!["b@acme.com"]),
        // `->>` on an array yields its JSON text, so a value test against it would be nonsense;
        // the array's elements are what is matched.
        ("has_any", json!(["[\"rust\", \"sql\"]"]), vec![]),
    ];
    for (op, value, want) in cases {
        let rule = json!({"field": "attr:interests", "op": op, "value": value});
        let doc = all(vec![rule.clone()]);
        assert_eq!(matching(&pool, fx.ws, &doc).await, want, "{rule}");
    }
}

/// Spec section 4.5: on a boolean attribute, `is_true` / `is_false` / `is_not_set` partition the
/// workspace — each contact is in exactly one — and `is_false` means *stored* false, so a contact
/// that has never been given the field does not match it. "False, or never set" stays expressible
/// as a group; folding absence into `is_false` would instead make "stored false" unaskable.
#[sqlx::test]
async fn a_boolean_attribute_is_a_four_way_partition(pool: PgPool) {
    let fx = seed(&pool).await;
    let rule = |op: &str| json!({"field": "attr:vip", "op": op});
    let is_true = matching(&pool, fx.ws, &all(vec![rule("is_true")])).await;
    let is_false = matching(&pool, fx.ws, &all(vec![rule("is_false")])).await;
    let is_not_set = matching(&pool, fx.ws, &all(vec![rule("is_not_set")])).await;
    assert_eq!(is_true, ["a@acme.com"]);
    assert_eq!(is_false, ["c@globex.com"], "`is_false` means stored false");
    assert_eq!(is_not_set, ["b@acme.com"]);

    // Exactly one arm each, and together they are the whole workspace.
    let mut union: Vec<String> = [is_true, is_false, is_not_set].concat();
    union.sort();
    let distinct = union.iter().collect::<std::collections::HashSet<_>>().len();
    assert_eq!(distinct, union.len(), "the arms overlap: {union:?}");
    assert_eq!(union, ALL);

    // And the reverse reading is still writable, as a group.
    let either = all(vec![json!({"group": {"match": "any", "rules": [
        rule("is_false"), rule("is_not_set"),
    ]}})]);
    assert_eq!(
        matching(&pool, fx.ws, &either).await,
        ["b@acme.com", "c@globex.com"]
    );
}

/// A **populated** list or tag of another workspace matches nothing. The fixture gives both a real
/// member, so this is not the weaker claim that an id with no rows anywhere matches nothing.
///
/// What actually makes list and tag safe is `m.contact_id = contacts.id`, because a contact id is
/// workspace-unique; `m.workspace_id = contacts.workspace_id` is defence in depth and index
/// selectivity on top of that. The tenancy guarantee itself is in the schema, not in a predicate,
/// so the last assertion here is that the database refuses the crossing row at all — which is the
/// only form of this claim that can be falsified. (The suppression arms are the opposite case:
/// there the correlation is load-bearing, and
/// `suppressed_sees_every_arm_and_only_this_workspaces` holds it.)
#[sqlx::test]
async fn a_list_or_tag_of_another_workspace_matches_nothing(pool: PgPool) {
    let fx = seed(&pool).await;
    let id = |u: Uuid| json!(u.to_string());
    let a: Uuid =
        sqlx::query_scalar("select id from contacts where workspace_id = $1 and email = $2")
            .bind(fx.ws)
            .bind("a@acme.com")
            .fetch_one(&pool)
            .await
            .expect("contact a");
    // A membership or tag row joining this workspace's contact to the other workspace's list or
    // tag cannot be written: both foreign keys are composite and carry `workspace_id`, so whatever
    // `workspace_id` such a row claimed would fail one of them.
    for (sql, other) in [
        (
            "insert into list_memberships (workspace_id, contact_id, list_id, status) \
             values ($1, $2, $3, 'subscribed')",
            fx.other_list,
        ),
        (
            "insert into contact_tags (workspace_id, contact_id, tag_id) values ($1, $2, $3)",
            fx.other_tag,
        ),
    ] {
        for claimed in [fx.ws, fx.other_ws] {
            let err = sqlx::query(sql)
                .bind(claimed)
                .bind(a)
                .bind(other)
                .execute(&pool)
                .await
                .expect_err("a row crossing a workspace boundary must be refused");
            assert!(
                err.as_database_error()
                    .is_some_and(|e| e.code().as_deref() == Some("23503")),
                "expected a foreign key violation, got {err}"
            );
        }
    }
    // The parser refuses the foreign id outright, which is the first of the two defences and the
    // one that matters: `not_member_of` of a row this workspace does not have is a `NOT EXISTS`
    // that is true for every contact, so compiling it at all would be "select the whole book".
    for (field, op) in [
        ("list", "member_of"),
        ("list", "not_member_of"),
        ("tag", "has"),
        ("tag", "not_has"),
    ] {
        let other = if field == "list" {
            fx.other_list
        } else {
            fx.other_tag
        };
        let doc = all(vec![json!({"field": field, "op": op, "value": id(other)})]);
        let err = Group::parse(&doc, &refs(&pool, fx.ws).await)
            .expect_err("another workspace's id must not parse");
        assert_eq!(err.code(), "invalid-value", "{field} {op}");
    }

    // The second defence, which has to hold on its own: *if* a foreign id ever reached the
    // compiler — a resolver with a bug, a row deleted between the resolve and the compile — the
    // correlation on `contacts.workspace_id` must still make the positives match nothing...
    let foreign = Refs {
        kinds: attrs(),
        lists: [fx.other_list].into_iter().collect(),
        tags: [fx.other_tag].into_iter().collect(),
    };
    for rule in [
        json!({"field": "list", "op": "member_of", "value": id(fx.other_list)}),
        json!({"field": "tag", "op": "has", "value": id(fx.other_tag)}),
    ] {
        let doc = all(vec![rule.clone()]);
        assert_eq!(
            matching_against(&pool, fx.ws, &doc, &foreign).await,
            Vec::<String>::new(),
            "{rule}"
        );
    }
    // ... and their negations match everyone, rather than failing closed on the wrong workspace.
    // This is the shape the parser now keeps out of the database entirely.
    for rule in [
        json!({"field": "list", "op": "not_member_of", "value": id(fx.other_list)}),
        json!({"field": "tag", "op": "not_has", "value": id(fx.other_tag)}),
    ] {
        let doc = all(vec![rule.clone()]);
        assert_eq!(
            matching_against(&pool, fx.ws, &doc, &foreign).await,
            ALL,
            "{rule}"
        );
    }
}

/// All four suppression arms, one at a time, and the workspace correlation that is the only thing
/// separating tenants on this field.
///
/// Unlike list and tag, these arms match on `contacts.email` and `contacts.email_domain`, and the
/// same address legitimately exists in many workspaces — so there is no composite foreign key
/// standing behind the predicate, and dropping `s.workspace_id = contacts.workspace_id` is a live
/// tenancy leak. The fixture already holds the other tenant's block on `a@acme.com` and on
/// `acme.com`; step 1 is what makes it bite.
#[sqlx::test]
async fn suppressed_sees_every_arm_and_only_this_workspaces(pool: PgPool) {
    async fn suppressed(pool: &PgPool, ws: Uuid) -> Vec<String> {
        matching(
            pool,
            ws,
            &all(vec![json!({"field": "suppressed", "op": "is_true"})]),
        )
        .await
    }
    async fn mailable(pool: &PgPool, ws: Uuid) -> Vec<String> {
        matching(
            pool,
            ws,
            &all(vec![json!({"field": "suppressed", "op": "is_false"})]),
        )
        .await
    }

    let fx = seed(&pool).await;

    // 1. Arms 1 and 3, workspace-scoped. The other tenant blocks `a@acme.com` by address and
    //    `acme.com` by domain; neither may reach this workspace, in either direction.
    assert_eq!(suppressed(&pool, fx.ws).await, ["c@globex.com"]);
    assert_eq!(mailable(&pool, fx.ws).await, ["a@acme.com", "b@acme.com"]);
    // ... and from the other side, `c@globex.com`'s block here must not reach the other tenant,
    //     whose own contact *is* blocked there.
    assert_eq!(suppressed(&pool, fx.other_ws).await, ["a@acme.com"]);
    assert_eq!(mailable(&pool, fx.other_ws).await, Vec::<String>::new());

    // 2. Arm 2: an instance-wide address block, which belongs to no workspace and reaches all.
    suppress(&pool, None, "address", "b@acme.com").await;
    assert_eq!(
        suppressed(&pool, fx.ws).await,
        ["b@acme.com", "c@globex.com"]
    );
    assert_eq!(mailable(&pool, fx.ws).await, ["a@acme.com"]);

    // 3. Arm 4: an instance-wide domain block. Now every contact is suppressed, which is the one
    //    honest way `suppressed is_false` may match nobody.
    let lifted = suppress(&pool, None, "domain", "globex.com").await;
    suppress(&pool, None, "domain", "acme.com").await;
    assert_eq!(suppressed(&pool, fx.ws).await, ALL);
    assert_eq!(mailable(&pool, fx.ws).await, Vec::<String>::new());

    // 4. `lifted_at is null` in every arm: lifting a block releases the contact it covered.
    //    `c@globex.com` stays suppressed by its own workspace-scoped address row.
    sqlx::query("update suppressions set lifted_at = now() where id = $1")
        .bind(lifted)
        .execute(&pool)
        .await
        .expect("lift");
    assert_eq!(suppressed(&pool, fx.ws).await, ALL);
    sqlx::query("update suppressions set lifted_at = now() where scope = 'domain'")
        .execute(&pool)
        .await
        .expect("lift the rest");
    assert_eq!(
        suppressed(&pool, fx.ws).await,
        ["b@acme.com", "c@globex.com"]
    );
}

/// `match` and nesting compile to the right boolean shape, and a nested group is parenthesised:
/// without the parentheses `a or b and c` would bind differently from the document.
#[sqlx::test]
async fn groups_nest_and_match_all_and_any_mean_and_and_or(pool: PgPool) {
    let fx = seed(&pool).await;
    let pro = json!({"field": "attr:plan", "op": "eq", "value": "pro"});
    let no_tz = json!({"field": "timezone", "op": "is_not_set"});
    let acme = json!({"field": "email_domain", "op": "eq", "value": "acme.com"});

    let doc = json!({"match": "all", "rules": [acme.clone(), pro.clone()]});
    assert_eq!(matching(&pool, fx.ws, &doc).await, ["a@acme.com"]);
    let doc = json!({"match": "any", "rules": [pro.clone(), no_tz.clone()]});
    assert_eq!(
        matching(&pool, fx.ws, &doc).await,
        ["a@acme.com", "b@acme.com"]
    );

    // `(plan = pro or acme.com) and no timezone` is `b` alone. Without the parentheses around
    // the nested group it would flatten to `plan = pro or (acme.com and no timezone)`, which also
    // takes `a` — so this case is what pins the grouping, not only the operators.
    let doc = json!({"match": "all", "rules": [
        {"group": {"match": "any", "rules": [pro, acme]}},
        no_tz,
    ]});
    assert_eq!(matching(&pool, fx.ws, &doc).await, ["b@acme.com"]);
}

/// Carried from Task 1's review: the `present` deserializer keeps an absent `value` and an
/// explicit `null` apart, and this is the first place that distinction can be checked against
/// real SQL. An explicit `null` never reaches the compiler, and a document that parses compiles
/// to what it says and re-serialises unchanged.
#[sqlx::test]
async fn absent_and_null_values_stay_apart_through_compilation(pool: PgPool) {
    let fx = seed(&pool).await;

    // Absent: `is_set` takes no value, parses, and compiles to a presence test.
    let absent = all(vec![json!({"field": "attr:plan", "op": "is_set"})]);
    assert_eq!(
        matching(&pool, fx.ws, &absent).await,
        ["a@acme.com", "c@globex.com"]
    );

    // Explicit null: refused at parse, so the compiler's extractors can never see `Some(null)`.
    for doc in [
        all(vec![
            json!({"field": "attr:plan", "op": "is_set", "value": null}),
        ]),
        all(vec![
            json!({"field": "attr:plan", "op": "eq", "value": null}),
        ]),
        all(vec![
            json!({"field": "attr:interests", "op": "has_any", "value": null}),
        ]),
    ] {
        assert!(
            Group::parse(&doc, &attr_refs(&[])).is_err(),
            "an explicit null must not reach the compiler: {doc}"
        );
    }

    // A valued rule on the same field is a different predicate, not the same one.
    let valued = all(vec![
        json!({"field": "attr:plan", "op": "eq", "value": "pro"}),
    ]);
    assert_eq!(matching(&pool, fx.ws, &valued).await, ["a@acme.com"]);

    // The whole of spec section 4.1's example document, parsed and compiled, round-tripping
    // exactly: absent `value`s stay absent, present ones stay present, `status` stays where it is.
    let doc = json!({"match": "all", "rules": [
        {"field": "email", "op": "contains", "value": "@acme.com"},
        {"field": "attr:plan", "op": "eq", "value": "pro"},
        {"field": "attr:seats", "op": "gte", "value": 5},
        {"field": "attr:joined", "op": "in_last_days", "value": 365},
        {"field": "attr:vip", "op": "is_true"},
        {"field": "created_at", "op": "before", "value": "2027-01-01T00:00:00Z"},
        {"field": "list", "op": "member_of", "value": fx.list.to_string(), "status": "subscribed"},
        {"field": "tag", "op": "has", "value": fx.tag.to_string()},
        {"field": "suppressed", "op": "is_false"},
        {"group": {"match": "any", "rules": [
            {"field": "timezone", "op": "is_set"},
            {"group": {"match": "all", "rules": [
                {"field": "email_domain", "op": "eq", "value": "acme.com"}
            ]}}
        ]}}
    ]});
    // `matching` asserts the round trip itself.
    assert_eq!(matching(&pool, fx.ws, &doc).await, ["a@acme.com"]);
}

/// Structural, and the one assertion here that reads the generated SQL: no caller-supplied text
/// may appear in it. Every value and every attribute key is a bound parameter, so the SQL holds
/// only `$n` placeholders and fragments written in `compile.rs`.
#[test]
fn no_caller_supplied_text_reaches_the_sql() {
    let nasty = [
        INJECTION,
        "50%",
        "pro' or '1'='1",
        "7b0f2c3e-0000-4000-8000-000000000001",
        "rust",
        // The one caller-supplied *enum*. `Group::parse` narrows it to three values, so it cannot
        // carry an injection, but it is the input most likely to be waved through a status check
        // as "already safe" and then written rather than bound.
        "subscribed",
    ];
    let doc = json!({"match": "any", "rules": [
        {"field": format!("attr:{INJECTION}"), "op": "eq", "value": "pro' or '1'='1"},
        {"field": "attr:note", "op": "contains", "value": "50%"},
        {"field": "attr:note", "op": "not_contains", "value": "50%"},
        {"field": "attr:interests", "op": "has_any", "value": ["rust"]},
        {"field": "attr:interests", "op": "has_none", "value": ["rust"]},
        {"field": "attr:vip", "op": "is_true"},
        {"field": "attr:vip", "op": "is_false"},
        {"field": "tag", "op": "not_has", "value": NAMED},
        {"field": "list", "op": "member_of", "value": NAMED,
         "status": "subscribed"},
    ]});
    let group = Group::parse(&doc, &attr_refs(&[NAMED])).expect("the document parses");
    let mut q =
        QueryBuilder::<Postgres>::new("select id from contacts where workspace_id = $0 and ");
    push_rules(&mut q, &group);
    let sql = q.into_sql();
    let sql = sql.as_str();
    for text in nasty {
        assert!(
            !sql.contains(text),
            "caller-supplied text reached the SQL: {text}\n{sql}"
        );
    }
    // And the predicate is more than the workspace restriction the caller wrote. Every condition
    // the compiler emits names a `contacts` column or a bound parameter, so a SQL constant in the
    // fragment can only be a tautology spliced in beside the rules — the `(true or …)` shape that
    // turns a document into "every contact in the workspace". The only `true` / `false` a
    // compiled predicate may legitimately contain is the tail of an `is true` / `is false` test on
    // a boolean attribute, and the document above has both, so this is not a vacuous strip.
    let (_, rules) = sql.split_once("workspace_id = $0 and ").expect("prefix");
    assert!(rules.starts_with('(') && rules.len() > 2, "{sql}");
    assert!(rules.contains("$1"), "nothing was bound: {sql}");
    assert!(
        rules.contains(" is true") && rules.contains(" is false"),
        "{sql}"
    );
    let stripped = rules.replace(" is true", "").replace(" is false", "");
    for constant in ["true", "false", "1=1", "1 = 1"] {
        assert!(
            !stripped.contains(constant),
            "the predicate carries the constant `{constant}`, which no rule can compile to: {sql}"
        );
    }
}

/// **A `list`-scope refusal is not the `suppressed` field, and this is the only place that could
/// have got it wrong in silence.**
///
/// `blocking` and the admin API decode `scope` and `reason` into `Scope` and `Reason`, which have
/// no variant for a list row — so a reader there that lost its scope filter fails to decode and
/// the request errors. These arms return a boolean. A fifth arm added here, or a scope filter
/// dropped from one of the four, reports somebody who unsubscribed from one list as blocked from
/// **all** mail, with nothing to notice it: `suppressed is_true` is what the Contacts screen
/// filters on and what a segment excludes recipients by.
///
/// The before/after is the test. The fixture's own `suppressed` set is read first, so the
/// assertion after the refusal is "this did not move", not "this has some value".
#[sqlx::test]
async fn a_list_scope_refusal_does_not_reach_the_suppressed_field(pool: PgPool) {
    async fn suppressed(pool: &PgPool, ws: Uuid) -> Vec<String> {
        matching(
            pool,
            ws,
            &all(vec![json!({"field": "suppressed", "op": "is_true"})]),
        )
        .await
    }
    async fn mailable(pool: &PgPool, ws: Uuid) -> Vec<String> {
        matching(
            pool,
            ws,
            &all(vec![json!({"field": "suppressed", "op": "is_false"})]),
        )
        .await
    }

    let fx = seed(&pool).await;
    let before_suppressed = suppressed(&pool, fx.ws).await;
    let before_mailable = mailable(&pool, fx.ws).await;
    assert_eq!(
        (&before_suppressed[..], &before_mailable[..]),
        (
            &["c@globex.com".to_owned()][..],
            &["a@acme.com".to_owned(), "b@acme.com".to_owned()][..]
        ),
        "the premise: `a` is mailable before it leaves one list"
    );

    // Through the writer the preference center uses, not a row built here: a hand-written row
    // would only prove this file's reading of the schema.
    let mut conn = pool.acquire().await.expect("a connection");
    nm::suppressions::unsubscribe_list(
        &mut conn,
        &Secrets::new(&SecretKey([7u8; 32])),
        fx.ws,
        fx.list,
        "a@acme.com",
    )
    .await
    .expect("refuse one list");
    drop(conn);
    let live: i64 = sqlx::query_scalar(
        "select count(*) from suppressions where scope = 'list' and lifted_at is null",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live, 1, "the premise: the refusal is really there");

    assert_eq!(
        suppressed(&pool, fx.ws).await,
        before_suppressed,
        "leaving one list made somebody blocked from all mail"
    );
    assert_eq!(
        mailable(&pool, fx.ws).await,
        before_mailable,
        "and dropped them out of every segment that selects mailable contacts"
    );
}
