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
    custom_fields::FieldKind,
    segments::{compile::push_rules, rules::Group},
};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

/// Every contact in the fixture's workspace. A compiled document that returns all three has lost
/// its restriction and become "every contact in the workspace".
const ALL: [&str; 3] = ["a@acme.com", "b@acme.com", "c@globex.com"];

struct Fx {
    ws: Uuid,
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

/// `a` has every attribute and matches the positive rules; `c` has every attribute with the other
/// value; `b` has **none** of them, no tag, no list row and a NULL timezone — it is the contact
/// every negative operator must match and a negated join would lose.
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
    // The decoy: same address shape, in another workspace, matching everything.
    insert_contact(
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

    let tag = insert_tag(pool, ws, "vip").await;
    let other_tag = insert_tag(pool, other_ws, "vip").await;
    sqlx::query("insert into contact_tags (workspace_id, contact_id, tag_id) values ($1, $2, $3)")
        .bind(ws)
        .bind(a)
        .bind(tag)
        .execute(pool)
        .await
        .expect("insert contact_tag");

    // `c` alone is suppressed, by address, in this workspace.
    sqlx::query(
        "insert into suppressions (id, workspace_id, scope, email, email_hmac, reason) \
         values ($1, $2, 'address', 'c@globex.com', '\\x00'::bytea, 'bounced')",
    )
    .bind(Uuid::now_v7())
    .bind(ws)
    .execute(pool)
    .await
    .expect("insert suppression");

    Fx {
        ws,
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
    let group = Group::parse(doc, &attrs()).expect("the document parses");
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
            json!({"field": "attr:vip", "op": "is_set"}),
            vec!["a@acme.com", "c@globex.com"],
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
        (
            json!({"field": "attr:vip", "op": "is_false"}),
            vec!["b@acme.com", "c@globex.com"],
        ),
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

/// A list or tag of another workspace matches nothing: the `exists` clauses are correlated on
/// `workspace_id` as well as the contact.
#[sqlx::test]
async fn a_list_or_tag_of_another_workspace_matches_nothing(pool: PgPool) {
    let fx = seed(&pool).await;
    let id = |u: Uuid| json!(u.to_string());
    for rule in [
        json!({"field": "list", "op": "member_of", "value": id(fx.other_list)}),
        json!({"field": "tag", "op": "has", "value": id(fx.other_tag)}),
    ] {
        let doc = all(vec![rule.clone()]);
        assert_eq!(
            matching(&pool, fx.ws, &doc).await,
            Vec::<String>::new(),
            "{rule}"
        );
    }
    // ... and their negations match everyone, rather than failing closed on the wrong workspace.
    for rule in [
        json!({"field": "list", "op": "not_member_of", "value": id(fx.other_list)}),
        json!({"field": "tag", "op": "not_has", "value": id(fx.other_tag)}),
    ] {
        let doc = all(vec![rule.clone()]);
        assert_eq!(matching(&pool, fx.ws, &doc).await, ALL, "{rule}");
    }
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
            Group::parse(&doc, &attrs()).is_err(),
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
    ];
    let doc = json!({"match": "any", "rules": [
        {"field": format!("attr:{INJECTION}"), "op": "eq", "value": "pro' or '1'='1"},
        {"field": "attr:note", "op": "contains", "value": "50%"},
        {"field": "attr:note", "op": "not_contains", "value": "50%"},
        {"field": "attr:interests", "op": "has_any", "value": ["rust"]},
        {"field": "attr:interests", "op": "has_none", "value": ["rust"]},
        {"field": "tag", "op": "not_has", "value": "7b0f2c3e-0000-4000-8000-000000000001"},
        {"field": "list", "op": "member_of", "value": "7b0f2c3e-0000-4000-8000-000000000001",
         "status": "subscribed"},
    ]});
    let group = Group::parse(&doc, &attrs()).expect("the document parses");
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
    // And the predicate is more than the workspace restriction the caller wrote.
    let (_, rules) = sql.split_once("workspace_id = $0 and ").expect("prefix");
    assert!(rules.starts_with('(') && rules.len() > 2, "{sql}");
    assert!(rules.contains("$1"), "nothing was bound: {sql}");
}
