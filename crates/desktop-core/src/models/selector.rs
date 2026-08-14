//! Semantic selectors and the targets commands operate on.

use serde::{Deserialize, Serialize};

use crate::models::{
    element::Element,
    ids::{ElementId, WindowId},
    role::Role,
};

/// What to inspect or act within. `Focused` is the default so the common case
/// needs no flags at all.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    #[default]
    Focused,
    App(String),
    Window(WindowId),
}

impl Target {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Focused => "the focused window".to_owned(),
            Self::App(name) => format!("application {name:?}"),
            Self::Window(id) => format!("window {id}"),
        }
    }
}

/// A semantic query over a snapshot.
///
/// `name` is an exact, case-insensitive match; `text` is a case-insensitive
/// substring searched across name, value and description. Exactness matters:
/// an agent asking for the button named "Save" should not be handed
/// "Save As…".
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Selector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl Selector {
    #[must_use]
    pub fn by_role(role: Role) -> Self {
        Self {
            role: Some(role),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn by_name(name: &str) -> Self {
        Self {
            name: Some(name.to_owned()),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn by_text(text: &str) -> Self {
        Self {
            text: Some(text.to_owned()),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_role(mut self, role: Role) -> Self {
        self.role = Some(role);
        self
    }

    #[must_use]
    pub fn with_name(mut self, name: &str) -> Self {
        self.name = Some(name.to_owned());
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.role.is_none() && self.name.is_none() && self.text.is_none()
    }

    /// All criteria must hold. An empty selector matches nothing rather than
    /// everything, so a command built from missing flags fails loudly.
    ///
    /// A redacted value stays unsearchable: a substring query must not become a
    /// back door to a secret the snapshot refused to print.
    #[must_use]
    pub fn matches(&self, element: &Element) -> bool {
        if self.is_empty() {
            return false;
        }
        if let Some(role) = &self.role
            && element.role != *role
        {
            return false;
        }
        if let Some(name) = &self.name {
            let matched = element
                .name
                .as_deref()
                .is_some_and(|actual| actual.trim().eq_ignore_ascii_case(name.trim()));
            if !matched {
                return false;
            }
        }
        if let Some(text) = &self.text {
            let needle = text.trim().to_lowercase();
            let haystacks = [
                element.name.as_deref(),
                element.value.as_deref(),
                element.description.as_deref(),
            ];
            let matched = haystacks
                .iter()
                .flatten()
                .any(|field| !element.redacted && field.to_lowercase().contains(&needle));
            if !matched {
                return false;
            }
        }
        true
    }

    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(role) = &self.role {
            parts.push(format!("role={}", role.as_str()));
        }
        if let Some(name) = &self.name {
            parts.push(format!("name={name:?}"));
        }
        if let Some(text) = &self.text {
            parts.push(format!("text={text:?}"));
        }
        if parts.is_empty() {
            "<empty selector>".to_owned()
        } else {
            parts.join(" ")
        }
    }
}

/// How a click names its destination. Element and selector are preferred;
/// coordinates remain available as the documented fallback.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClickTarget {
    Element(ElementId),
    Selector(Selector),
    Point { x: i32, y: i32 },
}

/// Whether to act through the accessibility API or by moving the pointer.
///
/// `Auto` prefers the accessibility action when the element advertises one:
/// it cannot miss, needs no portal session on Wayland, and does not disturb
/// the user's cursor.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ActivationMode {
    #[default]
    Auto,
    Action,
    Pointer,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::geometry::Bounds;

    fn element(role: Role, name: Option<&str>, value: Option<&str>) -> Element {
        Element {
            id: ElementId::new(1),
            role,
            name: name.map(str::to_owned),
            description: None,
            value: value.map(str::to_owned),
            enabled: true,
            focused: false,
            selected: false,
            redacted: false,
            bounds: Some(Bounds::new(0, 0, 10, 10)),
            actions: Vec::new(),
            path: None,
        }
    }

    #[test]
    fn an_empty_selector_matches_nothing_so_missing_flags_fail_loudly() {
        let selector = Selector::default();
        assert!(selector.is_empty());
        assert!(!selector.matches(&element(Role::Button, Some("Save"), None)));
    }

    #[test]
    fn role_and_name_must_both_hold() {
        let selector = Selector::by_role(Role::Button).with_name("Save");
        assert!(selector.matches(&element(Role::Button, Some("Save"), None)));
        assert!(!selector.matches(&element(Role::Button, Some("Run"), None)));
        assert!(!selector.matches(&element(Role::MenuItem, Some("Save"), None)));
    }

    #[test]
    fn name_matching_is_exact_so_save_does_not_match_save_as() {
        let selector = Selector::by_name("Save");
        assert!(selector.matches(&element(Role::Button, Some("Save"), None)));
        assert!(!selector.matches(&element(Role::Button, Some("Save As…"), None)));
    }

    #[test]
    fn name_matching_ignores_case_and_surrounding_whitespace() {
        let selector = Selector::by_name("save");
        assert!(selector.matches(&element(Role::Button, Some("  Save  "), None)));
    }

    #[test]
    fn text_matching_is_a_substring_across_name_value_and_description() {
        let selector = Selector::by_text("complete");
        assert!(selector.matches(&element(Role::Label, Some("Build complete"), None)));
        assert!(selector.matches(&element(Role::Label, None, Some("Build complete"))));
        assert!(!selector.matches(&element(Role::Label, Some("Build failed"), None)));
    }

    #[test]
    fn text_matching_never_reaches_into_a_redacted_value() {
        // Otherwise `desktop find --text "hunter2"` becomes an oracle for a
        // password field's contents.
        let mut secret = element(Role::PasswordField, Some("Password"), None);
        secret.redacted = true;
        secret.value = Some("hunter2".to_owned());
        assert!(!Selector::by_text("hunter2").matches(&secret));
    }

    #[test]
    fn describe_renders_every_criterion_for_use_in_error_messages() {
        let selector = Selector::by_role(Role::Button).with_name("Save");
        let described = selector.describe();
        assert!(described.contains("role=button"), "got {described}");
        assert!(described.contains(r#"name="Save""#), "got {described}");
        assert_eq!(Selector::default().describe(), "<empty selector>");
    }

    #[test]
    fn target_defaults_to_the_focused_window() {
        assert_eq!(Target::default(), Target::Focused);
    }

    #[test]
    fn activation_defaults_to_preferring_the_accessibility_action() {
        assert_eq!(ActivationMode::default(), ActivationMode::Auto);
    }
}
