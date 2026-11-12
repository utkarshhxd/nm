// SPDX-License-Identifier: AGPL-3.0-only

//! Compiles a parsed rule tree into a parameterised SQL predicate over `contacts` (spec M1b
//! sections 4.2 to 4.5). The caller supplies the query up to and including its `workspace_id`
//! restriction; this file appends only the rules.
//!
//! Four properties hold everywhere below, and every one of them has a test in
//! `tests/segments_compile.rs` that was shown to go red when it was removed.
//!
//! 1. **Nothing caller-supplied becomes SQL text.** A field resolves to a *fixed* fragment chosen
//!    by a match on the whitelist in section 4.2. The only caller-supplied part of any fragment is
//!    the attribute key, and it is bound, never written. Keys bind as `&str`: `->>` and `->` are
//!    overloaded on `text` and `integer`, and any other inferred type silently selects the
//!    array-index overload, which would make the rule mean something else.
//! 2. **Negation is `NOT EXISTS`, never a negated join, and survives NULL.** A contact with no
//!    related rows matches `not_has` / `not_member_of`; a contact whose column or attribute is
//!    NULL or absent matches every negative operator, because `col not ilike …` is NULL — and so
//!    not true — for a NULL column.
//! 3. **LIKE metacharacters are escaped**, with an explicit `ESCAPE` clause. Unescaped, a value of
//!    `50%` matches every row: the "matches everything" shape of section 4.1, arriving through a
//!    value instead of a document.
//! 4. **Casts follow the definition's kind, not the value's shape.** `(attributes ->> $n)::numeric`
//!    for a number field. That is safe only because `validate_attributes` enforces the type on
//!    write; the coupling is load-bearing and is recorded in spec section 4.3.

use serde_json::Value;
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{
    contacts::escape_like,
    custom_fields::FieldKind,
    segments::rules::{Field, Group, Match, Op, Rule, RuleNode},
};

/// The four live-suppression arms of `mailable_list_members`, correlated to `contacts` rather than
/// to a membership row: workspace scope and instance scope, address scope and domain scope. FOUR
/// separate clauses, not two with `or` inside — migration 0015 measured that an OR-qualified
/// anti-join cannot use the partial indexes and degrades to a sequential scan. Every token here is
/// fixed: nothing caller-supplied reaches this list.
const SUPPRESSION_ARMS: [&str; 4] = [
    "(select 1 from suppressions s where s.lifted_at is null \
      and s.workspace_id = contacts.workspace_id \
      and s.scope = 'address' and s.email = contacts.email)",
    "(select 1 from suppressions s where s.lifted_at is null \
      and s.workspace_id is null \
      and s.scope = 'address' and s.email = contacts.email)",
    "(select 1 from suppressions s where s.lifted_at is null \
      and s.workspace_id = contacts.workspace_id \
      and s.scope = 'domain' and s.domain = contacts.email_domain)",
    "(select 1 from suppressions s where s.lifted_at is null \
      and s.workspace_id is null \
      and s.scope = 'domain' and s.domain = contacts.email_domain)",
];

/// Appends `group` as one parenthesised predicate to a query that has already constrained
/// `contacts.workspace_id`.
///
/// A group always contributes at least one condition: `Group::parse` refuses an empty `rules`
/// array at every level, which is what stops a document compiling to "every contact in the
/// workspace". If one ever did arrive empty this emits `()`, a syntax error Postgres refuses —
/// loudly wrong rather than quietly matching everyone.
pub fn push_rules(q: &mut QueryBuilder<Postgres>, group: &Group) {
    let join = match group.match_ {
        Match::All => " and ",
        Match::Any => " or ",
    };
    q.push("(");
    for (i, node) in group.rules.iter().enumerate() {
        if i > 0 {
            q.push(join);
        }
        match node {
            RuleNode::Group(g) => push_rules(q, g),
            RuleNode::Rule(r) => push_rule(q, r),
        }
    }
    q.push(")");
}

/// The whitelist of section 4.2, as a match. Every arm names a fixed fragment; `Field::Attr` is
/// the one arm carrying caller text, and it hands that text to `Lhs::Attr`, which binds it.
fn push_rule(q: &mut QueryBuilder<Postgres>, r: &Rule) {
    match &r.field {
        Field::Email => scalar_rule(q, Lhs::Col("contacts.email"), FieldKind::Text, r),
        Field::FirstName => scalar_rule(q, Lhs::Col("contacts.first_name"), FieldKind::Text, r),
        Field::LastName => scalar_rule(q, Lhs::Col("contacts.last_name"), FieldKind::Text, r),
        Field::Timezone => scalar_rule(q, Lhs::Col("contacts.timezone"), FieldKind::Text, r),
        Field::EmailDomain => scalar_rule(q, Lhs::Col("contacts.email_domain"), FieldKind::Text, r),
        // Already `timestamptz`, so their cast is a no-op; the operators are the datetime ones.
        Field::CreatedAt => scalar_rule(q, Lhs::Col("contacts.created_at"), FieldKind::DateTime, r),
        Field::UpdatedAt => scalar_rule(q, Lhs::Col("contacts.updated_at"), FieldKind::DateTime, r),
        // A multi_select is a JSON array: `->>` text comparison on it is meaningless, so it has
        // its own compiler through `jsonb_exists_any`.
        Field::Attr {
            key,
            kind: FieldKind::MultiSelect,
        } => multi_select_rule(q, key, r),
        Field::Attr { key, kind } => scalar_rule(q, Lhs::Attr(key), *kind, r),
        Field::List => list_rule(q, r),
        Field::Tag => tag_rule(q, r),
        Field::Suppressed => suppressed_rule(q, r),
    }
}

/// A rule's left-hand side. `Col` is a fixed identifier from the match above; `Attr` holds the one
/// caller-supplied string in the whole compiler, and only ever emits it through `push_bind`.
#[derive(Clone, Copy)]
enum Lhs<'a> {
    Col(&'static str),
    Attr(&'a str),
}

impl Lhs<'_> {
    /// The expression as `text`. An attribute key is bound each time it appears, so a rule that
    /// mentions the expression twice (the NULL half of a negation) binds twice — Postgres does not
    /// mind, and it keeps the key a parameter everywhere.
    fn push(self, q: &mut QueryBuilder<Postgres>) {
        match self {
            Lhs::Col(c) => {
                q.push(c);
            }
            // `&str`, so `->>` takes its `text` overload and not the array-index one.
            Lhs::Attr(key) => {
                q.push("(contacts.attributes ->> ").push_bind(key).push(")");
            }
        }
    }

    /// The expression cast to the *definition's* kind, never the value's shape.
    fn push_as(self, q: &mut QueryBuilder<Postgres>, kind: FieldKind) {
        self.push(q);
        if let Some(cast) = cast_of(kind) {
            q.push("::").push(cast);
        }
    }
}

/// The SQL type an attribute's text is read back as. Text and select are already text;
/// multi_select never reaches here.
fn cast_of(kind: FieldKind) -> Option<&'static str> {
    match kind {
        FieldKind::Text | FieldKind::Select | FieldKind::MultiSelect => None,
        FieldKind::Number => Some("numeric"),
        FieldKind::Date => Some("date"),
        FieldKind::DateTime => Some("timestamptz"),
        FieldKind::Boolean => Some("boolean"),
    }
}

/// Every operator over a scalar left-hand side: a `contacts` column or one attribute key.
fn scalar_rule(q: &mut QueryBuilder<Postgres>, lhs: Lhs<'_>, kind: FieldKind, r: &Rule) {
    match r.op {
        Op::Eq => {
            lhs.push_as(q, kind);
            q.push(" = ");
            push_operand(q, kind, &r.value);
        }
        // Negative, so it must survive NULL: a contact with no value at all is not equal to `x`.
        Op::Neq => {
            q.push("(");
            lhs.push(q);
            q.push(" is null or ");
            lhs.push_as(q, kind);
            q.push(" <> ");
            push_operand(q, kind, &r.value);
            q.push(")");
        }
        Op::Contains => push_like(q, lhs, r, "'%' || ", " || '%'", false),
        Op::NotContains => push_like(q, lhs, r, "'%' || ", " || '%'", true),
        Op::StartsWith => push_like(q, lhs, r, "", " || '%'", false),
        Op::EndsWith => push_like(q, lhs, r, "'%' || ", "", false),
        Op::Gt | Op::Gte | Op::Lt | Op::Lte | Op::Before | Op::After => {
            lhs.push_as(q, kind);
            q.push(match r.op {
                Op::Gt | Op::After => " > ",
                Op::Gte => " >= ",
                Op::Lt | Op::Before => " < ",
                _ => " <= ",
            });
            push_operand(q, kind, &r.value);
        }
        // "On that calendar day": an instant is compared by its date, so `on` means the same
        // thing for a `date` field and a `datetime` one.
        Op::On => {
            lhs.push_as(q, kind);
            q.push(if kind == FieldKind::Date {
                " = "
            } else {
                "::date = "
            });
            push_operand(q, kind, &r.value);
            if kind != FieldKind::Date {
                q.push("::date");
            }
        }
        Op::InLastDays => {
            lhs.push_as(q, kind);
            q.push(" >= ");
            // `make_interval`, not a formatted interval literal: the day count stays a bound
            // parameter. `Group::parse` caps it at 36500, past which Postgres errors.
            if kind == FieldKind::Date {
                q.push("(now() - make_interval(days => ")
                    .push_bind(days(&r.value))
                    .push("))::date");
            } else {
                q.push("now() - make_interval(days => ")
                    .push_bind(days(&r.value))
                    .push(")");
            }
        }
        // Text and select treat an empty string as no value: `first_name` and `last_name` are
        // `not null default ''`, so a bare NULL test would make the operator useless on them.
        Op::IsSet | Op::IsNotSet => {
            let text = matches!(kind, FieldKind::Text | FieldKind::Select);
            if text {
                q.push("coalesce(");
            }
            lhs.push(q);
            q.push(match (text, r.op) {
                (true, Op::IsSet) => ", '') <> ''",
                (true, _) => ", '') = ''",
                (false, Op::IsSet) => " is not null",
                (false, _) => " is null",
            });
        }
        // `is true` / `is not true`, not `= true` / `= false`: a boolean expression over an absent
        // key is NULL, and `is not true` is where section 4.5's reviewer check lands for a
        // boolean — a contact with no value matches `is_false`. `is_set` is how you ask the other
        // question.
        Op::IsTrue | Op::IsFalse => {
            lhs.push_as(q, kind);
            q.push(if r.op == Op::IsTrue {
                " is true"
            } else {
                " is not true"
            });
        }
        op => {
            unreachable!("`{op:?}` is not an operator of a scalar field; `Group::parse` refuses it")
        }
    }
}

/// `ilike`, with the value's `%`, `_` and `\` escaped and an explicit `ESCAPE` clause. `negated`
/// adds the NULL half: `timezone not ilike '%x%'` is NULL, and therefore not true, for every
/// contact whose timezone is NULL.
fn push_like(
    q: &mut QueryBuilder<Postgres>,
    lhs: Lhs<'_>,
    r: &Rule,
    prefix: &'static str,
    suffix: &'static str,
    negated: bool,
) {
    if negated {
        q.push("(");
        lhs.push(q);
        q.push(" is null or ");
    }
    lhs.push(q);
    q.push(if negated { " not ilike " } else { " ilike " });
    q.push(prefix)
        .push_bind(escape_like(text(&r.value)))
        .push(suffix)
        // The escape character the escaping above used. Postgres defaults to backslash, but the
        // clause is written out so the pairing cannot be broken by a server setting.
        .push(" escape '\\'");
    if negated {
        q.push(")");
    }
}

/// The right-hand side, cast to the definition's kind. A number is bound as its JSON text and cast
/// to `numeric`, not bound as a float: `numeric = float8` would compare through a binary float and
/// `5.1` would stop being `5.1`.
fn push_operand(q: &mut QueryBuilder<Postgres>, kind: FieldKind, value: &Option<Value>) {
    match kind {
        FieldKind::Number => {
            let Some(Value::Number(n)) = value else {
                unreachable!("`Group::parse` requires a number for this operator")
            };
            q.push_bind(n.to_string()).push("::numeric");
        }
        FieldKind::Date => {
            q.push_bind(text(value)).push("::date");
        }
        FieldKind::DateTime => {
            q.push_bind(text(value)).push("::timestamptz");
        }
        _ => {
            q.push_bind(text(value));
        }
    }
}

/// `has_any` / `has_none` over the array's elements, through `jsonb_exists_any` (the `?|`
/// operator, spelled as the function so the `?` cannot be mistaken for a placeholder).
fn multi_select_rule(q: &mut QueryBuilder<Postgres>, key: &str, r: &Rule) {
    match r.op {
        Op::IsSet | Op::IsNotSet => {
            q.push("(contacts.attributes ->> ")
                .push_bind(key)
                .push(if r.op == Op::IsSet {
                    ") is not null"
                } else {
                    ") is null"
                });
        }
        Op::HasAny => {
            push_exists_any(q, key, r);
        }
        // Negative, so the absent key matters: `jsonb_exists_any(null, …)` is NULL and `not NULL`
        // is not true, which would drop every contact who has never been given the field.
        Op::HasNone => {
            q.push("((contacts.attributes -> ")
                .push_bind(key)
                .push(") is null or not ");
            push_exists_any(q, key, r);
            q.push(")");
        }
        op => unreachable!("`{op:?}` is not an operator of a multi_select field"),
    }
}

fn push_exists_any(q: &mut QueryBuilder<Postgres>, key: &str, r: &Rule) {
    let Some(Value::Array(items)) = &r.value else {
        unreachable!("`Group::parse` requires a non-empty list of strings for this operator")
    };
    let wanted: Vec<String> = items
        .iter()
        .map(|v| text(&Some(v.clone())).to_owned())
        .collect();
    q.push("jsonb_exists_any(contacts.attributes -> ")
        .push_bind(key)
        .push(", ")
        .push_bind(wanted)
        .push(")");
}

/// `exists` / `not exists`, correlated on `workspace_id` as well as the contact, so a list of
/// another workspace matches nothing rather than leaking one.
fn list_rule(q: &mut QueryBuilder<Postgres>, r: &Rule) {
    q.push(match r.op {
        Op::MemberOf => "exists ",
        Op::NotMemberOf => "not exists ",
        op => unreachable!("`{op:?}` is not an operator of the list field"),
    });
    q.push(
        "(select 1 from list_memberships m where m.workspace_id = contacts.workspace_id \
         and m.contact_id = contacts.id and m.list_id = ",
    )
    .push_bind(uuid(&r.value));
    if let Some(status) = r.status {
        q.push(" and m.status = ").push_bind(status);
    }
    q.push(")");
}

/// `NOT EXISTS`, never a negated join: a contact with no tags at all must match `not_has`.
fn tag_rule(q: &mut QueryBuilder<Postgres>, r: &Rule) {
    q.push(match r.op {
        Op::Has => "exists ",
        Op::NotHas => "not exists ",
        op => unreachable!("`{op:?}` is not an operator of the tag field"),
    });
    q.push(
        "(select 1 from contact_tags t where t.workspace_id = contacts.workspace_id \
         and t.contact_id = contacts.id and t.tag_id = ",
    )
    .push_bind(uuid(&r.value))
    .push(")");
}

/// `is_false` is the four arms separately negated, so a contact with no suppression row at all
/// matches it.
fn suppressed_rule(q: &mut QueryBuilder<Postgres>, r: &Rule) {
    let (keyword, join) = match r.op {
        Op::IsTrue => ("exists ", " or "),
        Op::IsFalse => ("not exists ", " and "),
        op => unreachable!("`{op:?}` is not an operator of the suppressed field"),
    };
    q.push("(");
    for (i, arm) in SUPPRESSION_ARMS.iter().enumerate() {
        if i > 0 {
            q.push(join);
        }
        q.push(keyword).push(*arm);
    }
    q.push(")");
}

// The three extractors below restate what `Group::parse` already checked. A `Rule` can only be
// built by parsing, so none of them can fire for a document that reached this file; each names the
// invariant it depends on rather than inventing an always-false or always-true rule.

fn text(value: &Option<Value>) -> &str {
    let Some(Value::String(s)) = value else {
        unreachable!("`Group::parse` requires a string for this operator")
    };
    s
}

fn uuid(value: &Option<Value>) -> Uuid {
    Uuid::parse_str(text(value)).expect("`Group::parse` requires a UUID for this operator")
}

fn days(value: &Option<Value>) -> i32 {
    let n = value
        .as_ref()
        .and_then(Value::as_u64)
        .expect("`Group::parse` requires a whole number of days");
    i32::try_from(n).expect("`Group::parse` caps the day count at 36500")
}
