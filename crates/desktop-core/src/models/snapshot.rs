//! The compact semantic snapshot an agent reads.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::models::{element::Element, geometry::CoordinateSpace, ids::ElementId};

/// Limits on tree traversal. Exceeding one sets [`Snapshot::truncated`] rather
/// than quietly returning a partial tree, because an agent that believes it has
/// seen the whole window will conclude a missing button does not exist.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct WalkBudget {
    pub max_nodes: usize,
    pub max_depth: usize,
}

impl WalkBudget {
    pub const DEFAULT_MAX_NODES: usize = 4_000;
    pub const DEFAULT_MAX_DEPTH: usize = 40;
}

impl Default for WalkBudget {
    fn default() -> Self {
        Self {
            max_nodes: Self::DEFAULT_MAX_NODES,
            max_depth: Self::DEFAULT_MAX_DEPTH,
        }
    }
}

/// A pruned, numbered view of one window's accessibility tree.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Snapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// Which origin every `bounds` in this snapshot is measured from. On GNOME
    /// Wayland this is a window, not the screen.
    pub coordinate_space: CoordinateSpace,
    pub elements: Vec<Element>,
    /// `true` when the walk hit its budget. The elements present are still
    /// valid; there are simply more of them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Nodes the platform reported, before pruning. The ratio against
    /// `elements.len()` is the token saving.
    pub visited_nodes: usize,
    /// The display this was taken on, so it is never read against another.
    ///
    /// Element ids mean nothing outside the tree they were numbered in. The
    /// store is one file per user, shared by the user's own desktop and by
    /// every agent session, so without this a snapshot survives
    /// `session start` and `session stop` and gets searched against a display
    /// it never described. `None` where the platform has only one display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

impl Snapshot {
    #[must_use]
    pub fn find(&self, id: ElementId) -> Option<&Element> {
        self.elements.iter().find(|element| element.id == id)
    }

    /// Renders the compact human form:
    ///
    /// ```text
    /// Application: Visual Studio Code
    /// Window: main.rs — desktop-driver
    ///
    /// [1] menu "File"
    /// [5] button "Run"
    /// ```
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(app) = &self.app {
            let _ = writeln!(out, "Application: {app}");
        }
        if let Some(window) = &self.window {
            let _ = writeln!(out, "Window: {window}");
        }
        if self.app.is_some() || self.window.is_some() {
            out.push('\n');
        }

        if self.elements.is_empty() {
            out.push_str("(no interactive elements found)\n");
            return out;
        }

        for element in &self.elements {
            let _ = write!(out, "[{}] {}", element.id, element.role.as_str());
            if let Some(name) = &element.name {
                let _ = write!(out, " {:?}", name);
            }
            if element.redacted {
                out.push_str(" <redacted>");
            } else if let Some(value) = &element.value {
                let _ = write!(out, " = {:?}", value);
            }
            for flag in state_flags(element) {
                let _ = write!(out, " {flag}");
            }
            out.push('\n');
        }

        if self.truncated {
            let _ = writeln!(
                out,
                "\n(truncated: walk budget reached after {} nodes; re-run with --max-nodes to see more)",
                self.visited_nodes
            );
        }
        out
    }
}

fn state_flags(element: &Element) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if !element.enabled {
        flags.push("disabled");
    }
    if element.focused {
        flags.push("focused");
    }
    if element.selected {
        flags.push("selected");
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{geometry::Bounds, role::Role};

    fn element(id: u32, role: Role, name: &str) -> Element {
        Element {
            id: ElementId::new(id),
            role,
            name: Some(name.to_owned()),
            description: None,
            value: None,
            enabled: true,
            focused: false,
            selected: false,
            redacted: false,
            bounds: Some(Bounds::new(0, 0, 10, 10)),
            actions: Vec::new(),
            path: None,
        }
    }

    fn snapshot(elements: Vec<Element>) -> Snapshot {
        Snapshot {
            app: Some("Visual Studio Code".to_owned()),
            window: Some("main.rs — desktop-driver".to_owned()),
            coordinate_space: CoordinateSpace::primary_screen(),
            elements,
            truncated: false,
            visited_nodes: 0,
            display: None,
        }
    }

    #[test]
    fn rendering_matches_the_documented_compact_format() {
        let snap = snapshot(vec![
            element(1, Role::Menu, "File"),
            element(2, Role::Menu, "Edit"),
            element(5, Role::Button, "Run"),
        ]);
        let rendered = snap.render();
        assert_eq!(
            rendered,
            "Application: Visual Studio Code\n\
             Window: main.rs — desktop-driver\n\
             \n\
             [1] menu \"File\"\n\
             [2] menu \"Edit\"\n\
             [5] button \"Run\"\n"
        );
    }

    #[test]
    fn a_selected_tab_is_annotated() {
        let mut tab = element(3, Role::Tab, "main.rs");
        tab.selected = true;
        let rendered = snapshot(vec![tab]).render();
        assert!(
            rendered.contains(r#"[3] tab "main.rs" selected"#),
            "got {rendered}"
        );
    }

    #[test]
    fn a_disabled_element_is_annotated() {
        let mut button = element(4, Role::Button, "Save");
        button.enabled = false;
        let rendered = snapshot(vec![button]).render();
        assert!(
            rendered.contains(r#"[4] button "Save" disabled"#),
            "got {rendered}"
        );
    }

    #[test]
    fn a_value_is_shown_but_a_redacted_one_is_replaced_with_a_marker() {
        let mut field = element(6, Role::TextBox, "Editor");
        field.value = Some("hello".to_owned());
        assert!(snapshot(vec![field]).render().contains(r#"= "hello""#));

        let mut secret = element(7, Role::PasswordField, "Password");
        secret.value = None;
        secret.redacted = true;
        let rendered = snapshot(vec![secret]).render();
        assert!(rendered.contains("<redacted>"), "got {rendered}");
        assert!(!rendered.contains("hunter"), "got {rendered}");
    }

    #[test]
    fn an_empty_snapshot_says_so_rather_than_rendering_nothing() {
        let rendered = snapshot(Vec::new()).render();
        assert!(
            rendered.contains("(no interactive elements found)"),
            "got {rendered}"
        );
    }

    #[test]
    fn truncation_is_announced_so_a_missing_element_is_not_read_as_absent() {
        let mut snap = snapshot(vec![element(1, Role::Button, "Save")]);
        snap.truncated = true;
        snap.visited_nodes = 4_000;
        let rendered = snap.render();
        assert!(rendered.contains("truncated"), "got {rendered}");
        assert!(rendered.contains("4000"), "got {rendered}");
    }

    #[test]
    fn find_locates_an_element_by_its_snapshot_id() {
        let snap = snapshot(vec![
            element(1, Role::Menu, "File"),
            element(9, Role::Button, "Run"),
        ]);
        assert_eq!(
            snap.find(ElementId::new(9)).map(|e| e.name.as_deref()),
            Some(Some("Run"))
        );
        assert!(snap.find(ElementId::new(2)).is_none());
    }

    #[test]
    fn default_budget_is_generous_enough_for_real_applications() {
        let budget = WalkBudget::default();
        assert_eq!(budget.max_nodes, 4_000);
        assert_eq!(budget.max_depth, 40);
    }
}
