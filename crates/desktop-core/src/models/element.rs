//! The normalized element and the raw tree it is derived from.

use serde::{Deserialize, Serialize};

use crate::models::{geometry::Bounds, ids::ElementId, path::ElementPath, role::Role};

/// An action an element advertises. Performing one is preferred over
/// synthesizing pointer input: it is deterministic, needs no portal session,
/// and cannot miss because a window moved between snapshot and click.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ElementAction {
    Press,
    Toggle,
    Expand,
    Collapse,
    Select,
    ShowMenu,
    Focus,
    Increment,
    Decrement,
}

impl ElementAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Press => "press",
            Self::Toggle => "toggle",
            Self::Expand => "expand",
            Self::Collapse => "collapse",
            Self::Select => "select",
            Self::ShowMenu => "show_menu",
            Self::Focus => "focus",
            Self::Increment => "increment",
            Self::Decrement => "decrement",
        }
    }

    /// Maps a platform action name onto the normalized vocabulary. AT-SPI uses
    /// lowercase verbs (`click`, `press`); macOS uses `AX`-prefixed names.
    ///
    /// GTK4 namespaces its actions, so `default.activate` — what a button
    /// reports as its primary action — is recognised alongside the bare names.
    #[must_use]
    pub fn from_platform(name: &str) -> Option<Self> {
        match name.trim() {
            "AXPress" => Some(Self::Press),
            "AXShowMenu" => Some(Self::ShowMenu),
            "AXIncrement" => Some(Self::Increment),
            "AXDecrement" => Some(Self::Decrement),
            "AXConfirm" => Some(Self::Press),
            "AXPick" => Some(Self::Select),
            "AXRaise" => Some(Self::Focus),
            other => match other.to_ascii_lowercase().as_str() {
                "click" | "press" | "activate" | "jump" | "open" | "default.activate" => {
                    Some(Self::Press)
                }
                "toggle" => Some(Self::Toggle),
                "expand" | "expand or contract" => Some(Self::Expand),
                "collapse" => Some(Self::Collapse),
                "select" => Some(Self::Select),
                "show menu" | "menu" => Some(Self::ShowMenu),
                "grabfocus" | "grab focus" | "focus" => Some(Self::Focus),
                "increment" => Some(Self::Increment),
                "decrement" => Some(Self::Decrement),
                _ => None,
            },
        }
    }
}

/// Interaction states, normalized across platforms.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct States {
    pub enabled: bool,
    pub focused: bool,
    pub focusable: bool,
    pub selected: bool,
    pub checked: bool,
    pub expanded: bool,
    pub visible: bool,
    pub showing: bool,
    /// The platform marked this element's content as protected. Independent of
    /// [`Role::is_secure`]: either one triggers redaction.
    pub protected: bool,
}

impl States {
    /// A sensible default for platforms that do not report a given state: an
    /// element is assumed usable and on-screen unless told otherwise.
    #[must_use]
    pub const fn usable() -> Self {
        Self {
            enabled: true,
            focused: false,
            focusable: false,
            selected: false,
            checked: false,
            expanded: false,
            visible: true,
            showing: true,
            protected: false,
        }
    }
}

/// A node as reported by a platform adapter, before normalization.
#[derive(Clone, Debug, PartialEq)]
pub struct RawNode {
    pub role: Role,
    pub name: Option<String>,
    pub description: Option<String>,
    pub value: Option<String>,
    pub states: States,
    pub bounds: Option<Bounds>,
    pub actions: Vec<ElementAction>,
    pub children: Vec<RawNode>,
    /// A serializable platform handle, when the platform has one. AT-SPI object
    /// paths qualify; `AXUIElementRef` does not, so macOS leaves this `None`
    /// and always re-resolves by path.
    pub native: Option<String>,
}

impl RawNode {
    #[must_use]
    pub fn new(role: Role) -> Self {
        Self {
            role,
            name: None,
            description: None,
            value: None,
            states: States::usable(),
            bounds: None,
            actions: Vec::new(),
            children: Vec::new(),
            native: None,
        }
    }

    #[must_use]
    pub fn with_name(mut self, name: &str) -> Self {
        self.name = Some(name.to_owned());
        self
    }

    #[must_use]
    pub fn with_value(mut self, value: &str) -> Self {
        self.value = Some(value.to_owned());
        self
    }

    #[must_use]
    pub fn with_bounds(mut self, bounds: Bounds) -> Self {
        self.bounds = Some(bounds);
        self
    }

    #[must_use]
    pub fn with_actions(mut self, actions: &[ElementAction]) -> Self {
        self.actions = actions.to_vec();
        self
    }

    #[must_use]
    pub fn with_children(mut self, children: Vec<Self>) -> Self {
        self.children = children;
        self
    }

    #[must_use]
    pub fn with_states(mut self, states: States) -> Self {
        self.states = states;
        self
    }

    #[must_use]
    pub fn with_native(mut self, native: &str) -> Self {
        self.native = Some(native.to_owned());
        self
    }

    /// True when this node's value must be withheld, by role or by platform
    /// state. Checked in exactly one place, where every snapshot path converges.
    #[must_use]
    pub const fn is_secure(&self) -> bool {
        self.role.is_secure() || self.states.protected
    }
}

/// A normalized element as it appears in snapshots and JSON output.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Element {
    pub id: ElementId,
    pub role: Role,
    pub name: Option<String>,
    pub description: Option<String>,
    pub value: Option<String>,
    pub enabled: bool,
    pub focused: bool,
    pub selected: bool,
    /// `true` when [`Element::value`] was withheld because the element holds a
    /// secret. Distinguishes "no value" from "value not shown".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
    pub bounds: Option<Bounds>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ElementAction>,
    /// How to find this element again in a later process. Omitted from the
    /// human-facing rendering but essential to `--element N`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<ElementPath>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gtk4_namespaced_action_names_map_to_press() {
        // Probed on gnome-calculator: GTK4 reports `default.activate`.
        assert_eq!(
            ElementAction::from_platform("default.activate"),
            Some(ElementAction::Press)
        );
    }

    #[test]
    fn action_matching_is_case_insensitive_for_platform_variation() {
        assert_eq!(
            ElementAction::from_platform("Click"),
            Some(ElementAction::Press)
        );
    }

    #[test]
    fn platform_action_names_from_both_platforms_map_to_press() {
        assert_eq!(
            ElementAction::from_platform("AXPress"),
            Some(ElementAction::Press)
        );
        assert_eq!(
            ElementAction::from_platform("click"),
            Some(ElementAction::Press)
        );
        assert_eq!(
            ElementAction::from_platform("press"),
            Some(ElementAction::Press)
        );
        assert_eq!(
            ElementAction::from_platform("activate"),
            Some(ElementAction::Press)
        );
    }

    #[test]
    fn unknown_platform_actions_are_dropped_rather_than_guessed() {
        assert_eq!(ElementAction::from_platform("AXSomethingNew"), None);
        assert_eq!(ElementAction::from_platform(""), None);
    }

    #[test]
    fn a_node_is_secure_by_role_or_by_platform_protected_state() {
        let by_role = RawNode::new(Role::PasswordField);
        assert!(by_role.is_secure());

        let mut states = States::usable();
        states.protected = true;
        let by_state = RawNode::new(Role::TextBox).with_states(states);
        assert!(by_state.is_secure());

        assert!(!RawNode::new(Role::TextBox).is_secure());
    }

    #[test]
    fn redacted_flag_is_omitted_from_json_when_false_to_keep_snapshots_small() {
        let element = Element {
            id: ElementId::new(1),
            role: Role::Button,
            name: Some("Save".to_owned()),
            description: None,
            value: None,
            enabled: true,
            focused: false,
            selected: false,
            redacted: false,
            bounds: Some(Bounds::new(1100, 700, 80, 32)),
            actions: Vec::new(),
            path: None,
        };
        let json = serde_json::to_string(&element).expect("serializes");
        assert!(
            !json.contains("redacted"),
            "unexpected redacted key in {json}"
        );
        assert!(json.contains(r#""role":"button""#), "got {json}");
        assert!(json.contains(r#""name":"Save""#), "got {json}");
    }

    #[test]
    fn element_json_matches_the_documented_public_shape() {
        let element = Element {
            id: ElementId::new(42),
            role: Role::Button,
            name: Some("Save".to_owned()),
            description: None,
            value: None,
            enabled: true,
            focused: false,
            selected: false,
            redacted: false,
            bounds: Some(Bounds::new(1100, 700, 80, 32)),
            actions: Vec::new(),
            path: None,
        };
        let value = serde_json::to_value(&element).expect("serializes");
        assert_eq!(value["id"], 42);
        assert_eq!(value["role"], "button");
        assert_eq!(value["name"], "Save");
        assert!(value["description"].is_null());
        assert!(value["value"].is_null());
        assert_eq!(value["enabled"], true);
        assert_eq!(value["focused"], false);
        assert_eq!(value["selected"], false);
        assert_eq!(value["bounds"]["x"], 1100);
        assert_eq!(value["bounds"]["y"], 700);
        assert_eq!(value["bounds"]["width"], 80);
        assert_eq!(value["bounds"]["height"], 32);
    }
}
