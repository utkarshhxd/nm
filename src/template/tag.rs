// SPDX-License-Identifier: AGPL-3.0-only

//! The merge-tag language (spec §5.3): a closed grammar, parsed once at save into an AST. The AST
//! is the only thing stored; the operator's source text is never parsed again at send time.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::custom_fields::Definition;

/// How deep blocks may nest. Enforced at compile, so no renderer ever walks a deeper tree.
pub const MAX_DEPTH: usize = 32;

/// The contact columns a tag may name, alongside the workspace's custom fields.
const SYSTEM_FIELDS: [&str; 4] = ["email", "first_name", "last_name", "timezone"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Node {
    Text(String),
    Field {
        key: String,
        filter: Option<Filter>,
        fallback: Option<String>,
    },
    Present {
        key: String,
        body: Vec<Node>,
    },
    Absent {
        key: String,
        body: Vec<Node>,
    },
    Equals {
        key: String,
        value: String,
        body: Vec<Node>,
    },
}

/// Frozen at these four. Adding one is a spec change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Filter {
    Upper,
    Lower,
    Title,
    Date(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagError {
    UnknownField(String),
    ArchivedField(String),
    UnknownFilter(String),
    NonLiteralFallback,
    Unbalanced(String),
    TooDeep,
    Malformed(String),
}

/// The names a tag may use: system fields plus this workspace's custom fields. Archived fields are
/// kept apart so a new tag naming one is told why, rather than "unknown".
pub struct Registry {
    live: HashSet<String>,
    archived: HashSet<String>,
}

impl Registry {
    pub fn from_definitions(defs: &[Definition]) -> Self {
        let mut live: HashSet<String> = SYSTEM_FIELDS.iter().map(|s| s.to_string()).collect();
        let mut archived = HashSet::new();
        for d in defs {
            if d.archived_at.is_some() {
                archived.insert(d.key.clone());
            } else {
                live.insert(d.key.clone());
            }
        }
        // A system name can never be archived away.
        archived.retain(|k| !live.contains(k));
        Self { live, archived }
    }

    fn check(&self, key: &str) -> Result<(), TagError> {
        if self.live.contains(key) {
            Ok(())
        } else if self.archived.contains(key) {
            Err(TagError::ArchivedField(key.to_string()))
        } else {
            Err(TagError::UnknownField(key.to_string()))
        }
    }
}

/// A block waiting for its `{{/ key }}`.
struct Open {
    key: String,
    kind: Kind,
    body: Vec<Node>,
}

enum Kind {
    Present,
    Absent,
    Equals(String),
}

/// One pass over `source`. Text between tags is copied verbatim and tags become nodes; nothing
/// the AST later produces is ever fed back in here.
pub fn compile(source: &str, registry: &Registry) -> Result<Vec<Node>, TagError> {
    let mut root: Vec<Node> = Vec::new();
    // The depth cap is this stack's length, so it cannot drift from the parse.
    let mut stack: Vec<Open> = Vec::new();
    let mut rest = source;

    while let Some(start) = rest.find("{{") {
        if start > 0 {
            push(&mut stack, &mut root, Node::Text(rest[..start].to_string()));
        }
        let after = &rest[start + 2..];
        // shortcut: the first `}}` ends a tag, so a fallback literal cannot contain `}}`.
        let end = after
            .find("}}")
            .ok_or_else(|| TagError::Malformed("unterminated tag".into()))?;
        let inner = after[..end].trim();
        rest = &after[end + 2..];

        match inner.as_bytes().first() {
            Some(b'#') | Some(b'^') => {
                let body = inner[1..].trim();
                let (key, kind) = if inner.starts_with('#') {
                    open_tag(body)?
                } else {
                    (key_only(body)?, Kind::Absent)
                };
                registry.check(&key)?;
                if stack.len() >= MAX_DEPTH {
                    return Err(TagError::TooDeep);
                }
                stack.push(Open {
                    key,
                    kind,
                    body: Vec::new(),
                });
            }
            Some(b'/') => {
                let key = key_only(inner[1..].trim())?;
                match stack.pop() {
                    Some(open) if open.key == key => {
                        let body = open.body;
                        let node = match open.kind {
                            Kind::Present => Node::Present { key, body },
                            Kind::Absent => Node::Absent { key, body },
                            Kind::Equals(value) => Node::Equals { key, value, body },
                        };
                        push(&mut stack, &mut root, node);
                    }
                    _ => return Err(TagError::Unbalanced(key)),
                }
            }
            _ => {
                let node = field_tag(inner, registry)?;
                push(&mut stack, &mut root, node);
            }
        }
    }
    if !rest.is_empty() {
        push(&mut stack, &mut root, Node::Text(rest.to_string()));
    }
    match stack.pop() {
        Some(open) => Err(TagError::Unbalanced(open.key)),
        None => Ok(root),
    }
}

fn push(stack: &mut [Open], root: &mut Vec<Node>, node: Node) {
    match stack.last_mut() {
        Some(open) => open.body.push(node),
        None => root.push(node),
    }
}

/// `^[a-z][a-z0-9_]{0,62}$`, the same shape `custom_fields::validate_key` enforces.
fn is_key(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=63).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

fn key_only(s: &str) -> Result<String, TagError> {
    if is_key(s) {
        Ok(s.to_string())
    } else {
        Err(TagError::Malformed(format!("not a field name: {s:?}")))
    }
}

/// `"text"` with no quote inside: the only thing a fallback or an equality may hold.
fn literal(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('"')).then(|| inner.to_string())
}

/// The inside of `{{# … }}`: `key` or `key = "literal"`.
fn open_tag(s: &str) -> Result<(String, Kind), TagError> {
    match s.split_once('=') {
        None => Ok((key_only(s)?, Kind::Present)),
        Some((key, value)) => {
            let value = literal(value.trim())
                .ok_or_else(|| TagError::Malformed("an equality needs a quoted literal".into()))?;
            Ok((key_only(key.trim())?, Kind::Equals(value)))
        }
    }
}

/// The inside of `{{ … }}`: `key`, `key | "fallback"` or `key | filter`.
fn field_tag(s: &str, registry: &Registry) -> Result<Node, TagError> {
    let (key, arg) = match s.split_once('|') {
        None => (s, None),
        Some((k, a)) => (k.trim(), Some(a.trim())),
    };
    let key = key_only(key)?;
    registry.check(&key)?;
    let (mut filter, mut fallback) = (None, None);
    if let Some(arg) = arg {
        if arg.starts_with('"') {
            fallback = Some(literal(arg).ok_or(TagError::NonLiteralFallback)?);
        } else {
            filter = Some(match arg {
                "upper" => Filter::Upper,
                "lower" => Filter::Lower,
                "title" => Filter::Title,
                _ => match arg.strip_prefix("date:") {
                    Some(fmt) if !fmt.is_empty() => Filter::Date(fmt.to_string()),
                    // Naming another field here is an attempt at an expression, not a typo.
                    _ if registry.live.contains(arg) || registry.archived.contains(arg) => {
                        return Err(TagError::NonLiteralFallback);
                    }
                    _ => return Err(TagError::UnknownFilter(arg.to_string())),
                },
            });
        }
    }
    Ok(Node::Field {
        key,
        filter,
        fallback,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::custom_fields::FieldKind;

    fn def(key: &str, archived: bool) -> Definition {
        Definition {
            id: uuid::Uuid::nil(),
            key: key.into(),
            label: key.into(),
            kind: FieldKind::Text,
            options: vec![],
            archived_at: archived.then(chrono::Utc::now),
        }
    }

    fn registry() -> Registry {
        Registry::from_definitions(&[def("country", false), def("old_plan", true)])
    }

    fn field(key: &str) -> Node {
        Node::Field {
            key: key.into(),
            filter: None,
            fallback: None,
        }
    }

    /// A stand-in for R3's renderer: one walk over the AST, values written out and never read
    /// again. The property under test is that nothing feeds a value back into `compile`.
    fn eval(nodes: &[Node], vals: &HashMap<&str, &str>, out: &mut String) {
        for n in nodes {
            match n {
                Node::Text(t) => out.push_str(t),
                Node::Field {
                    key,
                    filter,
                    fallback,
                } => {
                    let v = vals.get(key.as_str()).copied().filter(|v| !v.is_empty());
                    match (v, fallback) {
                        (Some(v), _) => match filter {
                            Some(Filter::Upper) => out.push_str(&v.to_uppercase()),
                            Some(Filter::Lower) => out.push_str(&v.to_lowercase()),
                            _ => out.push_str(v),
                        },
                        (None, Some(f)) => out.push_str(f),
                        (None, None) => {}
                    }
                }
                Node::Present { key, body } => {
                    if vals.get(key.as_str()).is_some_and(|v| !v.is_empty()) {
                        eval(body, vals, out);
                    }
                }
                Node::Absent { key, body } => {
                    if vals.get(key.as_str()).is_none_or(|v| v.is_empty()) {
                        eval(body, vals, out);
                    }
                }
                Node::Equals { key, value, body } => {
                    if vals.get(key.as_str()) == Some(&value.as_str()) {
                        eval(body, vals, out);
                    }
                }
            }
        }
    }

    #[test]
    fn a_known_field_compiles() {
        let r = registry();
        assert_eq!(
            compile(
                "Hi {{ first_name | \"there\" }}, {{ country | upper }}!",
                &r
            )
            .unwrap(),
            vec![
                Node::Text("Hi ".into()),
                Node::Field {
                    key: "first_name".into(),
                    filter: None,
                    fallback: Some("there".into())
                },
                Node::Text(", ".into()),
                Node::Field {
                    key: "country".into(),
                    filter: Some(Filter::Upper),
                    fallback: None
                },
                Node::Text("!".into()),
            ]
        );
        assert_eq!(
            compile("{{email|date:%Y-%m-%d}}", &r).unwrap(),
            vec![Node::Field {
                key: "email".into(),
                filter: Some(Filter::Date("%Y-%m-%d".into())),
                fallback: None
            }]
        );
        assert_eq!(
            compile("no tags, a { and a }", &r).unwrap(),
            vec![Node::Text("no tags, a { and a }".into())]
        );
    }

    #[test]
    fn an_unknown_field_is_a_compile_error() {
        let r = registry();
        assert_eq!(
            compile("{{ nope }}", &r),
            Err(TagError::UnknownField("nope".into()))
        );
        // Blocks are checked too: a conditional is not a way around the registry.
        assert_eq!(
            compile("{{# nope }}x{{/ nope }}", &r),
            Err(TagError::UnknownField("nope".into()))
        );
    }

    #[test]
    fn an_archived_field_is_a_compile_error() {
        let r = registry();
        assert_eq!(
            compile("{{ old_plan }}", &r),
            Err(TagError::ArchivedField("old_plan".into()))
        );
        assert_eq!(
            compile("{{# old_plan }}x{{/ old_plan }}", &r),
            Err(TagError::ArchivedField("old_plan".into()))
        );
    }

    #[test]
    fn a_fallback_must_be_a_quoted_literal() {
        let r = registry();
        // Another field is an expression, not a literal.
        assert_eq!(
            compile("{{ first_name | last_name }}", &r),
            Err(TagError::NonLiteralFallback)
        );
        // A quote that does not close, or closes early.
        assert_eq!(
            compile("{{ first_name | \"there }}", &r),
            Err(TagError::NonLiteralFallback)
        );
        assert_eq!(
            compile("{{ first_name | \"a\" + \"b\" }}", &r),
            Err(TagError::NonLiteralFallback)
        );
        // Single quotes are not quotes.
        assert_eq!(
            compile("{{ first_name | 'there' }}", &r),
            Err(TagError::UnknownFilter("'there'".into()))
        );
        // An equality takes a quoted literal too.
        assert!(matches!(
            compile("{{# country = US }}x{{/ country }}", &r),
            Err(TagError::Malformed(_))
        ));
    }

    #[test]
    fn an_unfrozen_filter_is_a_compile_error() {
        let r = registry();
        for f in ["env", "include", "render", "date:", "UPPER", "upper:x"] {
            assert_eq!(
                compile(&format!("{{{{ first_name | {f} }}}}"), &r),
                Err(TagError::UnknownFilter(f.into())),
                "{f}"
            );
        }
    }

    #[test]
    fn present_absent_and_equals_blocks_compile() {
        let nodes = compile(
            "{{# first_name }}Hi {{ first_name }}{{/ first_name }}\
             {{^ first_name }}Hi{{/ first_name }}\
             {{# country = \"US\" }}US{{/ country }}",
            &registry(),
        )
        .unwrap();
        assert_eq!(
            nodes,
            vec![
                Node::Present {
                    key: "first_name".into(),
                    body: vec![Node::Text("Hi ".into()), field("first_name")]
                },
                Node::Absent {
                    key: "first_name".into(),
                    body: vec![Node::Text("Hi".into())]
                },
                Node::Equals {
                    key: "country".into(),
                    value: "US".into(),
                    body: vec![Node::Text("US".into())]
                },
            ]
        );
    }

    #[test]
    fn an_unbalanced_block_is_a_compile_error() {
        let r = registry();
        assert_eq!(
            compile("{{# first_name }}x", &r),
            Err(TagError::Unbalanced("first_name".into()))
        );
        assert_eq!(
            compile("x{{/ first_name }}", &r),
            Err(TagError::Unbalanced("first_name".into()))
        );
        assert_eq!(
            compile(
                "{{# first_name }}{{# country }}{{/ first_name }}{{/ country }}",
                &r
            ),
            Err(TagError::Unbalanced("first_name".into()))
        );
        assert!(matches!(
            compile("{{ first_name", &r),
            Err(TagError::Malformed(_))
        ));
    }

    #[test]
    fn nesting_past_max_depth_is_a_compile_error() {
        let r = registry();
        let nest = |n: usize| {
            let mut s = "{{# first_name }}".repeat(n);
            s.push('x');
            s.push_str(&"{{/ first_name }}".repeat(n));
            s
        };
        assert!(compile(&nest(MAX_DEPTH), &r).is_ok());
        assert_eq!(compile(&nest(MAX_DEPTH + 1), &r), Err(TagError::TooDeep));
        assert_eq!(compile(&nest(10_000), &r), Err(TagError::TooDeep));
        // Unclosed too: the cap must not wait for the closers to arrive.
        assert_eq!(
            compile(&"{{^ first_name }}".repeat(10_000), &r),
            Err(TagError::TooDeep)
        );
    }

    #[test]
    fn a_substituted_value_is_never_re_parsed() {
        let nodes = compile(
            "Hi {{ first_name }}, we have {{ email }} on file.",
            &registry(),
        )
        .unwrap();
        let vals = HashMap::from([("first_name", "{{ email }}"), ("email", "a@example.com")]);
        let mut out = String::new();
        eval(&nodes, &vals, &mut out);
        assert_eq!(out, "Hi {{ email }}, we have a@example.com on file.");
        // A value that is itself a block opener or an invalid tag is equally inert.
        let vals = HashMap::from([
            ("first_name", "{{# country }}{{ nope }}"),
            ("email", "a@example.com"),
        ]);
        let mut out = String::new();
        eval(&nodes, &vals, &mut out);
        assert_eq!(
            out,
            "Hi {{# country }}{{ nope }}, we have a@example.com on file."
        );
    }
}
