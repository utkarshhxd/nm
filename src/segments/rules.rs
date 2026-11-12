// SPDX-License-Identifier: AGPL-3.0-only

//! The segment rule AST (spec M1b section 4.1 to 4.3 and 4.6) and its parser. Parsing is the only
//! way to build a tree, so a tree that exists has passed every check here. No SQL.

use std::{collections::HashMap, fmt};

use chrono::{DateTime, NaiveDate};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};
use serde_json::Value;
use uuid::Uuid;

use crate::{custom_fields::FieldKind, error::ApiError, lists::MembershipStatus};

/// The top-level group is depth 1.
pub const MAX_DEPTH: usize = 3;
/// Leaf rules in the whole document.
pub const MAX_RULES: usize = 50;
/// Direct children (rules and groups) of one group.
pub const MAX_RULES_PER_GROUP: usize = 20;
/// 100 years. `now() - interval 'N days'` errors in SQL past the date range, which would be a 500.
const MAX_DAYS: u64 = 36_500;

/// Why a rule document was refused. Every variant is a 422 with its own problem type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleError {
    /// Not the document shape: unknown key, a node that is both rule and group, a wrong type.
    Malformed,
    /// A group with no rules. Vacuously true under `match: "all"`, so it would select everyone.
    EmptyRules,
    TooDeep,
    TooManyRules,
    GroupTooLarge,
    /// Not on the whitelist, or an `attr:` key naming no definition. Never an always-false rule.
    UnknownField(String),
    /// Reserved engagement field (`opened`, `clicked`, `bounced`, `sent`).
    NotYetAvailable(String),
    InvalidOperator {
        field: String,
        op: String,
    },
    InvalidValue {
        field: String,
        why: &'static str,
    },
}

impl RuleError {
    pub fn code(&self) -> &'static str {
        match self {
            RuleError::Malformed => "malformed-rules",
            RuleError::EmptyRules => "empty-rules",
            RuleError::TooDeep => "rules-too-deep",
            RuleError::TooManyRules => "too-many-rules",
            RuleError::GroupTooLarge => "group-too-large",
            RuleError::UnknownField(_) => "unknown-field",
            RuleError::NotYetAvailable(_) => "not-yet-available",
            RuleError::InvalidOperator { .. } => "invalid-operator",
            RuleError::InvalidValue { .. } => "invalid-value",
        }
    }
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Serde's message is not echoed: it can quote request content.
            RuleError::Malformed => write!(
                f,
                "a rule document is {{match, rules}}; a node is either {{field, op, value?, status?}} or {{group}}, never both, and no other keys are allowed"
            ),
            RuleError::EmptyRules => write!(
                f,
                "a group must have at least one rule: an empty group would select every contact"
            ),
            RuleError::TooDeep => write!(f, "groups may nest at most {MAX_DEPTH} deep"),
            RuleError::TooManyRules => {
                write!(f, "a segment may have at most {MAX_RULES} rules")
            }
            RuleError::GroupTooLarge => {
                write!(f, "a group may have at most {MAX_RULES_PER_GROUP} rules")
            }
            RuleError::UnknownField(name) => write!(f, "unknown field `{name}`"),
            RuleError::NotYetAvailable(name) => write!(
                f,
                "`{name}` rules need send tracking, which arrives in milestone M2"
            ),
            RuleError::InvalidOperator { field, op } => {
                write!(f, "operator `{op}` is not valid for field `{field}`")
            }
            RuleError::InvalidValue { field, why } => {
                write!(f, "invalid value for field `{field}`: {why}")
            }
        }
    }
}

impl From<RuleError> for ApiError {
    fn from(e: RuleError) -> ApiError {
        ApiError::UnprocessableDetail {
            code: e.code(),
            detail: e.to_string(),
        }
    }
}

/// Resolves an `attr:<key>` to its definition's kind. The parser validates shape only; the caller
/// owns the database lookup (and decides whether an archived definition may be named).
pub trait AttrKinds {
    fn kind_of(&self, key: &str) -> Option<FieldKind>;
}

impl AttrKinds for HashMap<String, FieldKind> {
    fn kind_of(&self, key: &str) -> Option<FieldKind> {
        self.get(key).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Match {
    All,
    Any,
}

/// A whitelisted field. There is no variant for a caller-supplied identifier except the attribute
/// key, which is only ever bound as a value. `Attr` carries the definition's kind so the compiler
/// needs no second lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    Email,
    FirstName,
    LastName,
    Timezone,
    EmailDomain,
    CreatedAt,
    UpdatedAt,
    Attr { key: String, kind: FieldKind },
    List,
    Tag,
    Suppressed,
}

impl Field {
    fn parse(name: &str, attrs: &dyn AttrKinds) -> Result<Field, RuleError> {
        Ok(match name {
            "email" => Field::Email,
            "first_name" => Field::FirstName,
            "last_name" => Field::LastName,
            "timezone" => Field::Timezone,
            "email_domain" => Field::EmailDomain,
            "created_at" => Field::CreatedAt,
            "updated_at" => Field::UpdatedAt,
            "list" => Field::List,
            "tag" => Field::Tag,
            "suppressed" => Field::Suppressed,
            // Reserved so the builder's shape does not change when sends land. There is no Field
            // variant for them: a tree that exists cannot contain one.
            "opened" | "clicked" | "bounced" | "sent" => {
                return Err(RuleError::NotYetAvailable(name.to_owned()));
            }
            _ => match name.strip_prefix("attr:") {
                Some(key) => match attrs.kind_of(key) {
                    Some(kind) => Field::Attr {
                        key: key.to_owned(),
                        kind,
                    },
                    None => return Err(RuleError::UnknownField(name.to_owned())),
                },
                None => return Err(RuleError::UnknownField(name.to_owned())),
            },
        })
    }

    /// `NOT NULL` on `contacts` (and, for `email`, validated non-empty), so `is_set` would be true
    /// for every contact in the workspace and `is_not_set` true for none. Both are refused as
    /// `invalid-operator`: section 4.1's "matches everything" shape has to be unrepresentable, not
    /// merely unlikely, and `created_at is_set` is one misclick away in a rule builder. `timezone`
    /// is genuinely nullable, and `first_name` / `last_name` are meaningful because the compiler
    /// treats `''` as unset, so none of those three is listed.
    fn never_unset(&self) -> bool {
        matches!(
            self,
            Field::Email | Field::EmailDomain | Field::CreatedAt | Field::UpdatedAt
        )
    }

    fn operand(&self) -> Operand {
        match self {
            Field::Email
            | Field::FirstName
            | Field::LastName
            | Field::Timezone
            | Field::EmailDomain => Operand::Text,
            Field::CreatedAt | Field::UpdatedAt => Operand::DateTime,
            Field::Attr { kind, .. } => match kind {
                FieldKind::Text => Operand::Text,
                FieldKind::Number => Operand::Number,
                FieldKind::Date => Operand::Date,
                FieldKind::DateTime => Operand::DateTime,
                FieldKind::Boolean => Operand::Boolean,
                FieldKind::Select => Operand::Select,
                FieldKind::MultiSelect => Operand::MultiSelect,
            },
            Field::List => Operand::List,
            Field::Tag => Operand::Tag,
            Field::Suppressed => Operand::Suppressed,
        }
    }
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Field::Email => f.write_str("email"),
            Field::FirstName => f.write_str("first_name"),
            Field::LastName => f.write_str("last_name"),
            Field::Timezone => f.write_str("timezone"),
            Field::EmailDomain => f.write_str("email_domain"),
            Field::CreatedAt => f.write_str("created_at"),
            Field::UpdatedAt => f.write_str("updated_at"),
            Field::Attr { key, .. } => write!(f, "attr:{key}"),
            Field::List => f.write_str("list"),
            Field::Tag => f.write_str("tag"),
            Field::Suppressed => f.write_str("suppressed"),
        }
    }
}

impl Serialize for Field {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Eq,
    Neq,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    Gt,
    Gte,
    Lt,
    Lte,
    Before,
    After,
    On,
    InLastDays,
    IsSet,
    IsNotSet,
    IsTrue,
    IsFalse,
    MemberOf,
    NotMemberOf,
    Has,
    NotHas,
    HasAny,
    HasNone,
}

/// What a field's operators and value are typed by (spec 4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operand {
    Text,
    Number,
    Date,
    DateTime,
    Boolean,
    // A select holds one string, so it gets equality. A multi-select is a JSON array, so `->>`
    // text comparison is wrong: it gets `has_any` / `has_none` over a list of values instead.
    Select,
    MultiSelect,
    List,
    Tag,
    Suppressed,
}

impl Operand {
    fn ops(self) -> &'static [Op] {
        use Op::*;
        match self {
            Operand::Text => &[
                Eq,
                Neq,
                Contains,
                NotContains,
                StartsWith,
                EndsWith,
                IsSet,
                IsNotSet,
            ],
            Operand::Number => &[Eq, Neq, Gt, Gte, Lt, Lte, IsSet, IsNotSet],
            Operand::Date | Operand::DateTime => &[Before, After, On, InLastDays, IsSet, IsNotSet],
            Operand::Boolean => &[IsTrue, IsFalse, IsSet, IsNotSet],
            Operand::Select => &[Eq, Neq, IsSet, IsNotSet],
            Operand::MultiSelect => &[HasAny, HasNone, IsSet, IsNotSet],
            Operand::List => &[MemberOf, NotMemberOf],
            Operand::Tag => &[Has, NotHas],
            Operand::Suppressed => &[IsTrue, IsFalse],
        }
    }
}

/// A leaf. `value` and `status` have been checked against `field` and `op` at parse, but stay as
/// JSON so the document round-trips exactly.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rule {
    pub field: Field,
    pub op: Op,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<MembershipStatus>,
}

/// A rule or a group, never both: two variants, so the invalid state has no representation.
#[derive(Debug, Clone, PartialEq)]
pub enum RuleNode {
    Rule(Rule),
    Group(Group),
}

impl Serialize for RuleNode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            RuleNode::Rule(r) => r.serialize(s),
            RuleNode::Group(g) => {
                let mut m = s.serialize_map(Some(1))?;
                m.serialize_entry("group", g)?;
                m.end()
            }
        }
    }
}

/// A rule document is a group. `rules` is never empty once parsed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Group {
    #[serde(rename = "match")]
    pub match_: Match,
    pub rules: Vec<RuleNode>,
}

// The wire shapes. Parsing goes through these and nothing else; the checked types above do not
// implement Deserialize, so no stored document can skip `Group::parse`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroup {
    #[serde(rename = "match")]
    match_: Match,
    rules: Vec<RawNode>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawNode {
    Group(RawGroupNode),
    Rule(RawRule),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroupNode {
    group: RawGroup,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    field: String,
    op: String,
    #[serde(default, deserialize_with = "present")]
    value: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    status: Option<String>,
}

/// Keeps an explicit `null` as `Some(null)` instead of folding it into absent, so the checks
/// below refuse it and a loaded document always re-serialises to what was loaded.
fn present<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

impl Group {
    /// Parses and validates a rule document. `attrs` resolves `attr:<key>` fields.
    pub fn parse(doc: &Value, attrs: &dyn AttrKinds) -> Result<Group, RuleError> {
        let raw = RawGroup::deserialize(doc).map_err(|_| RuleError::Malformed)?;
        Walk { attrs, rules: 0 }.group(raw, 1)
    }
}

struct Walk<'a> {
    attrs: &'a dyn AttrKinds,
    rules: usize,
}

impl Walk<'_> {
    fn group(&mut self, raw: RawGroup, depth: usize) -> Result<Group, RuleError> {
        if raw.rules.is_empty() {
            return Err(RuleError::EmptyRules);
        }
        if raw.rules.len() > MAX_RULES_PER_GROUP {
            return Err(RuleError::GroupTooLarge);
        }
        let mut rules = Vec::with_capacity(raw.rules.len());
        for node in raw.rules {
            rules.push(match node {
                RawNode::Group(g) => {
                    if depth + 1 > MAX_DEPTH {
                        return Err(RuleError::TooDeep);
                    }
                    RuleNode::Group(self.group(g.group, depth + 1)?)
                }
                RawNode::Rule(r) => {
                    self.rules += 1;
                    if self.rules > MAX_RULES {
                        return Err(RuleError::TooManyRules);
                    }
                    RuleNode::Rule(self.rule(r)?)
                }
            });
        }
        Ok(Group {
            match_: raw.match_,
            rules,
        })
    }

    fn rule(&self, raw: RawRule) -> Result<Rule, RuleError> {
        let field = Field::parse(&raw.field, self.attrs)?;
        let invalid_op = || RuleError::InvalidOperator {
            field: raw.field.clone(),
            op: raw.op.clone(),
        };
        let op: Op =
            serde_json::from_value(Value::String(raw.op.clone())).map_err(|_| invalid_op())?;
        let operand = field.operand();
        if !operand.ops().contains(&op)
            || (matches!(op, Op::IsSet | Op::IsNotSet) && field.never_unset())
        {
            return Err(invalid_op());
        }
        let bad = |why| RuleError::InvalidValue {
            field: raw.field.clone(),
            why,
        };
        let status = match (&raw.status, op) {
            (None, _) => None,
            (Some(s), Op::MemberOf | Op::NotMemberOf) => Some(
                serde_json::from_value::<MembershipStatus>(Value::String(s.clone()))
                    .map_err(|_| bad("status must be pending, subscribed or unsubscribed"))?,
            ),
            (Some(_), _) => return Err(bad("status applies to list membership only")),
        };
        check_value(op, operand, raw.value.as_ref()).map_err(bad)?;
        Ok(Rule {
            field,
            op,
            value: raw.value,
            status,
        })
    }
}

fn check_value(op: Op, operand: Operand, value: Option<&Value>) -> Result<(), &'static str> {
    use Op::*;
    match op {
        IsSet | IsNotSet | IsTrue | IsFalse => match value {
            None => Ok(()),
            Some(_) => Err("this operator takes no value"),
        },
        _ => {
            let v = value.ok_or("a value is required")?;
            match op {
                Contains | NotContains | StartsWith | EndsWith => match v.as_str() {
                    // `contains ""` matches every contact that has a value at all.
                    Some("") => Err("a pattern value must not be empty"),
                    Some(_) => Ok(()),
                    None => Err("value must be a string"),
                },
                Eq | Neq if operand != Operand::Number => {
                    v.is_string().then_some(()).ok_or("value must be a string")
                }
                Eq | Neq | Gt | Gte | Lt | Lte => {
                    v.is_number().then_some(()).ok_or("value must be a number")
                }
                Before | After | On => {
                    let s = v.as_str().ok_or("value must be a date string")?;
                    let ok = NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
                        || (operand == Operand::DateTime
                            && DateTime::parse_from_rfc3339(s).is_ok());
                    ok.then_some(())
                        .ok_or("value must be YYYY-MM-DD (or RFC 3339 for a datetime)")
                }
                InLastDays => match v.as_u64() {
                    Some(n) if n <= MAX_DAYS => Ok(()),
                    _ => Err("value must be a whole number of days, at most 36500"),
                },
                // Empty is refused for the reason an empty rule set is: `has_any: []` is false for
                // everyone and `has_none: []` is true for everyone.
                HasAny | HasNone => match v.as_array() {
                    Some(a) if a.is_empty() => Err("value must not be an empty list"),
                    Some(a) if a.iter().all(Value::is_string) => Ok(()),
                    _ => Err("value must be a list of strings"),
                },
                MemberOf | NotMemberOf | Has | NotHas => v
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .map(|_| ())
                    .ok_or("value must be a UUID"),
                IsSet | IsNotSet | IsTrue | IsFalse => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attrs() -> HashMap<String, FieldKind> {
        [
            ("plan", FieldKind::Text),
            ("seats", FieldKind::Number),
            ("joined", FieldKind::Date),
            ("vip", FieldKind::Boolean),
            ("tier", FieldKind::Select),
            ("interests", FieldKind::MultiSelect),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
    }

    fn parse(doc: Value) -> Result<Group, RuleError> {
        Group::parse(&doc, &attrs())
    }

    fn code(doc: Value) -> &'static str {
        parse(doc).expect_err("must be refused").code()
    }

    fn rule() -> Value {
        json!({"field": "email", "op": "contains", "value": "@acme.com"})
    }

    fn all(rules: Vec<Value>) -> Value {
        json!({"match": "all", "rules": rules})
    }

    fn group(rules: Vec<Value>) -> Value {
        json!({"group": {"match": "any", "rules": rules}})
    }

    fn rules(n: usize) -> Vec<Value> {
        (0..n).map(|_| rule()).collect()
    }

    const ID: &str = "7b0f2c3e-0000-4000-8000-000000000001";

    #[test]
    fn a_valid_document_round_trips() {
        let doc = json!({
            "match": "all",
            "rules": [
                {"field": "email", "op": "contains", "value": "@acme.com"},
                {"field": "attr:plan", "op": "eq", "value": "pro"},
                {"field": "attr:seats", "op": "gte", "value": 5},
                {"field": "attr:joined", "op": "in_last_days", "value": 30},
                {"field": "attr:vip", "op": "is_true"},
                {"field": "created_at", "op": "before", "value": "2026-01-01T00:00:00Z"},
                {"field": "list", "op": "member_of", "value": ID, "status": "subscribed"},
                {"field": "tag", "op": "not_has", "value": ID},
                {"field": "suppressed", "op": "is_false"},
                {"group": {"match": "any", "rules": [
                    {"field": "timezone", "op": "is_not_set"},
                    {"group": {"match": "all", "rules": [
                        {"field": "email_domain", "op": "eq", "value": "acme.com"}
                    ]}}
                ]}}
            ]
        });
        let parsed = parse(doc.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), doc);
        let RuleNode::Rule(second) = &parsed.rules[1] else {
            panic!("second node is a rule")
        };
        assert_eq!(
            second.field,
            Field::Attr {
                key: "plan".into(),
                kind: FieldKind::Text
            }
        );
    }

    #[test]
    fn an_empty_top_level_rules_array_is_refused() {
        assert_eq!(code(all(vec![])), "empty-rules");
    }

    #[test]
    fn an_empty_nested_rules_array_is_refused() {
        // A builder emits this every time the last rule in a group is deleted.
        assert_eq!(code(all(vec![rule(), group(vec![])])), "empty-rules");
        let deeper = group(vec![rule(), group(vec![])]);
        assert_eq!(code(all(vec![deeper])), "empty-rules");
    }

    #[test]
    fn an_unknown_field_is_refused_not_an_always_false_rule() {
        for field in [
            "emial",
            "attr:nope",
            "attr:",
            "ATTR:plan",
            "EMAIL",
            "",
            "contacts.id",
            // Attribute keys match exactly: a different case is a miss, not a fuzzy hit.
            "attr:Plan",
            "attr:plan ",
        ] {
            let err = parse(all(vec![json!({"field": field, "op": "eq", "value": "x"})]))
                .expect_err(field);
            assert_eq!(err, RuleError::UnknownField(field.to_owned()), "{field}");
            assert_eq!(err.code(), "unknown-field", "{field}");
        }
    }

    #[test]
    fn a_node_is_a_rule_or_a_group_never_both() {
        let both = json!({"field": "email", "op": "eq", "value": "x",
                          "group": {"match": "all", "rules": [rule()]}});
        assert_eq!(code(all(vec![both])), "malformed-rules");
        // Neither: a node with nothing in it.
        assert_eq!(code(all(vec![json!({})])), "malformed-rules");
    }

    #[test]
    fn unknown_keys_are_refused_on_every_shape() {
        let mut r = rule();
        r["extra"] = json!(1);
        assert_eq!(code(all(vec![r])), "malformed-rules");
        let mut g = group(vec![rule()]);
        g["group"]["extra"] = json!(1);
        assert_eq!(code(all(vec![g])), "malformed-rules");
        let mut g = group(vec![rule()]);
        g["extra"] = json!(1);
        assert_eq!(code(all(vec![g])), "malformed-rules");
        let mut top = all(vec![rule()]);
        top["extra"] = json!(1);
        assert_eq!(code(top), "malformed-rules");
        assert_eq!(
            code(json!({"match": "some", "rules": [rule()]})),
            "malformed-rules"
        );
    }

    #[test]
    fn depth_three_is_allowed_and_four_is_refused() {
        let l3 = group(vec![rule()]);
        let l2 = group(vec![rule(), l3.clone()]);
        assert!(parse(all(vec![l2])).is_ok(), "top, group, group is depth 3");
        let l4 = group(vec![rule(), group(vec![rule(), l3])]);
        assert_eq!(code(all(vec![l4])), "rules-too-deep");
    }

    #[test]
    fn fifty_rules_are_allowed_and_fifty_one_are_refused() {
        // Three groups keep each group under its own limit of 20.
        let build = |n: usize| {
            all(vec![
                group(rules(17)),
                group(rules(17)),
                group(rules(n - 34)),
            ])
        };
        assert!(parse(build(50)).is_ok());
        assert_eq!(code(build(51)), "too-many-rules");
    }

    #[test]
    fn twenty_rules_per_group_are_allowed_and_twenty_one_are_refused() {
        assert!(parse(all(rules(20))).is_ok());
        assert_eq!(code(all(rules(21))), "group-too-large");
        assert_eq!(code(all(vec![group(rules(21))])), "group-too-large");
    }

    #[test]
    fn an_operator_outside_the_fields_kind_is_refused() {
        for (field, op, value) in [
            ("email", "gt", json!(1)),
            ("attr:plan", "before", json!("2026-01-01")),
            ("attr:seats", "contains", json!("1")),
            ("attr:joined", "eq", json!("2026-01-01")),
            ("attr:vip", "eq", json!(true)),
            ("attr:tier", "contains", json!("a")),
            ("attr:interests", "eq", json!("a")),
            ("created_at", "starts_with", json!("2")),
            ("list", "has", json!(ID)),
            ("tag", "member_of", json!(ID)),
            ("suppressed", "is_set", json!(null)),
            ("email", "frobnicate", json!("x")),
        ] {
            let err = parse(all(vec![json!({"field": field, "op": op, "value": value})]))
                .expect_err(field);
            assert_eq!(err.code(), "invalid-operator", "{field} {op}");
        }
    }

    #[test]
    fn the_operator_set_follows_the_definitions_kind_not_the_key() {
        // The same key, defined as a number, takes number operators and refuses text ones.
        let mut numeric = attrs();
        numeric.insert("plan".to_owned(), FieldKind::Number);
        let doc = all(vec![json!({"field": "attr:plan", "op": "gt", "value": 1})]);
        assert!(Group::parse(&doc, &numeric).is_ok());
        assert!(parse(doc).is_err());
    }

    #[test]
    fn is_set_is_refused_on_the_fields_that_can_never_be_unset() {
        // All four are `NOT NULL` on `contacts`, so `is_set` would compile to a predicate matching
        // every contact in the workspace. Refused here, so no stored segment can hold one.
        for field in ["email", "email_domain", "created_at", "updated_at"] {
            for op in ["is_set", "is_not_set"] {
                let err =
                    parse(all(vec![json!({"field": field, "op": op})])).expect_err("must refuse");
                assert_eq!(err.code(), "invalid-operator", "{field} {op}");
            }
        }
        // The nullable and empty-string-bearing columns keep both operators, and so does every
        // attribute: an attribute key is absent until something writes it.
        for field in [
            "timezone",
            "first_name",
            "last_name",
            "attr:plan",
            "attr:seats",
        ] {
            for op in ["is_set", "is_not_set"] {
                assert!(
                    parse(all(vec![json!({"field": field, "op": op})])).is_ok(),
                    "{field} {op}"
                );
            }
        }
        // The other operators on those four fields are untouched.
        let ok = json!({"field": "created_at", "op": "in_last_days", "value": 30});
        assert!(parse(all(vec![ok])).is_ok());
    }

    #[test]
    fn reserved_engagement_fields_parse_then_return_not_yet_available() {
        for f in ["opened", "clicked", "bounced", "sent"] {
            // Any operator: the field is refused before the operator is looked at.
            for op in ["eq", "in_last_days", "nonsense"] {
                let err = parse(all(vec![
                    rule(),
                    group(vec![json!({"field": f, "op": op})]),
                ]))
                .expect_err(f);
                assert_eq!(err, RuleError::NotYetAvailable(f.to_owned()));
            }
        }
        let api: ApiError = RuleError::NotYetAvailable("opened".into()).into();
        let ApiError::UnprocessableDetail { code, detail } = api else {
            panic!("not a 422 with detail")
        };
        assert_eq!(code, "not-yet-available");
        assert!(detail.contains("M2"), "{detail}");
    }

    #[test]
    fn values_must_fit_the_operator() {
        for (field, op, value) in [
            ("email", "eq", json!(1)),
            ("email", "contains", json!("")),
            ("email", "eq", json!(null)),
            ("attr:seats", "gt", json!("5")),
            ("attr:joined", "before", json!("yesterday")),
            ("attr:joined", "before", json!("2026-01-01T00:00:00Z")),
            ("created_at", "in_last_days", json!(-1)),
            ("created_at", "in_last_days", json!(36_501)),
            ("created_at", "in_last_days", json!(1.5)),
            ("tag", "has", json!("not-a-uuid")),
            ("list", "member_of", json!(7)),
            // `timezone`, not `email`: `is_set` on a `NOT NULL` column is refused a step earlier.
            ("timezone", "is_set", json!("x")),
        ] {
            let err = parse(all(vec![json!({"field": field, "op": op, "value": value})]))
                .expect_err(field);
            assert_eq!(err.code(), "invalid-value", "{field} {op} {value}");
        }
        // Absent value on an operator that needs one.
        assert_eq!(
            code(all(vec![json!({"field": "email", "op": "eq"})])),
            "invalid-value"
        );
    }

    #[test]
    fn status_is_for_list_membership_only_and_must_be_a_real_status() {
        let ok = json!({"field": "list", "op": "not_member_of", "value": ID, "status": "pending"});
        assert!(parse(all(vec![ok])).is_ok());
        for bad in [
            json!({"field": "list", "op": "member_of", "value": ID, "status": "gone"}),
            json!({"field": "tag", "op": "has", "value": ID, "status": "subscribed"}),
        ] {
            assert_eq!(code(all(vec![bad])), "invalid-value");
        }
    }

    #[test]
    fn every_refusal_has_its_own_pinned_problem_type() {
        // The strings are the public contract (`urn:nm:problem:<code>`), so each is spelled out.
        let errors = [
            (RuleError::Malformed, "malformed-rules"),
            (RuleError::EmptyRules, "empty-rules"),
            (RuleError::TooDeep, "rules-too-deep"),
            (RuleError::TooManyRules, "too-many-rules"),
            (RuleError::GroupTooLarge, "group-too-large"),
            (RuleError::UnknownField(String::new()), "unknown-field"),
            (
                RuleError::NotYetAvailable(String::new()),
                "not-yet-available",
            ),
            (
                RuleError::InvalidOperator {
                    field: String::new(),
                    op: String::new(),
                },
                "invalid-operator",
            ),
            (
                RuleError::InvalidValue {
                    field: String::new(),
                    why: "",
                },
                "invalid-value",
            ),
        ];
        for (err, code) in &errors {
            assert_eq!(err.code(), *code);
        }
        let distinct: std::collections::HashSet<_> = errors.iter().map(|(_, c)| c).collect();
        assert_eq!(distinct.len(), errors.len());
    }

    #[test]
    fn a_multi_select_takes_has_any_and_has_none_over_a_non_empty_list_of_strings() {
        for op in ["has_any", "has_none"] {
            let ok = json!({"field": "attr:interests", "op": op, "value": ["a", "b"]});
            let parsed = parse(all(vec![ok.clone()])).unwrap();
            assert_eq!(serde_json::to_value(&parsed).unwrap(), all(vec![ok]));
            for bad in [
                json!([]),
                json!("a"),
                json!(["a", 1]),
                json!([null]),
                json!(null),
            ] {
                let err = parse(all(vec![
                    json!({"field": "attr:interests", "op": op, "value": bad}),
                ]))
                .expect_err("bad value");
                assert_eq!(err.code(), "invalid-value", "{op} {bad}");
            }
            let missing = parse(all(vec![json!({"field": "attr:interests", "op": op})]));
            assert_eq!(missing.expect_err("no value").code(), "invalid-value");
        }
        for op in ["is_set", "is_not_set"] {
            let ok = json!({"field": "attr:interests", "op": op});
            assert!(parse(all(vec![ok])).is_ok());
        }
        // The list operators belong to multi_select only.
        for field in ["email", "attr:tier", "attr:plan", "tag"] {
            let err = parse(all(vec![
                json!({"field": field, "op": "has_any", "value": ["a"]}),
            ]))
            .expect_err(field);
            assert_eq!(err.code(), "invalid-operator", "{field}");
        }
    }

    #[test]
    fn an_explicit_null_is_refused_so_a_document_round_trips_exactly() {
        // Absent and null are different documents; folding one into the other would let the rule
        // builder corrupt a segment by loading and saving it.
        // `timezone`, not `email`: `email` is `NOT NULL`, so `is_set` on it is refused a step
        // earlier, as `invalid-operator`, and would not reach the value check at all.
        let null_value = json!({"field": "timezone", "op": "is_set", "value": null});
        assert_eq!(code(all(vec![null_value])), "invalid-value");
        let null_value = json!({"field": "email", "op": "eq", "value": null});
        assert_eq!(code(all(vec![null_value])), "invalid-value");
        let null_status = json!({"field": "list", "op": "member_of", "value": ID, "status": null});
        assert_eq!(code(all(vec![null_status])), "malformed-rules");
        let absent = all(vec![json!({"field": "timezone", "op": "is_set"})]);
        let parsed = parse(absent.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), absent);
    }
}
