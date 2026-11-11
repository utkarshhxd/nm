// SPDX-License-Identifier: AGPL-3.0-only

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A member's role, stored as lowercase text. Variants run from least to most privileged:
/// `authorize` compares roles with `<`, so the order here is load-bearing.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, sqlx::Type, ToSchema,
)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Editor,
    Admin,
    Owner,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Editor => "editor",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }
}

/// Whether `actor` may put someone in `target`. `current` is their role now, `None` for a new
/// member. An owner may do anything; an admin may set viewer, editor or admin, but only on someone
/// who is not an owner. Everyone below admin manages no one.
pub fn can_assign(actor: Role, current: Option<Role>, target: Role) -> bool {
    match actor {
        Role::Owner => true,
        Role::Admin => target < Role::Owner && current.is_none_or(|c| c < Role::Owner),
        Role::Editor | Role::Viewer => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_ordered() {
        assert!(
            Role::Owner > Role::Admin && Role::Admin > Role::Editor && Role::Editor > Role::Viewer
        );
    }

    #[test]
    fn only_owners_touch_ownership() {
        assert!(!can_assign(Role::Admin, Some(Role::Editor), Role::Owner));
        assert!(!can_assign(Role::Admin, Some(Role::Owner), Role::Admin));
        assert!(can_assign(Role::Admin, Some(Role::Viewer), Role::Admin));
        assert!(can_assign(Role::Owner, Some(Role::Editor), Role::Owner));
    }
}
