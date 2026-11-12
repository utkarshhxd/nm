// SPDX-License-Identifier: AGPL-3.0-only

//! Segments: named, saved predicates over contacts. The rule document is JSON; `rules` parses and
//! validates it into a tree that cannot represent an empty group, an unknown field or a node that
//! is both a rule and a group, and `compile` turns such a tree into parameterised SQL.

pub mod compile;
pub mod rules;
