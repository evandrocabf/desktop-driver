//! Turning a raw accessibility tree into a compact snapshot.
//!
//! A real application's tree is mostly layout scaffolding: GTK4 nests unnamed
//! `generic` containers several deep, and a browser can report tens of
//! thousands of nodes. Handing that to an agent burns tokens and buries the
//! five things it can actually click.
//!
//! Pruning happens in one pass with the rules applied in a fixed order, so the
//! output is a deterministic function of the input — which is what makes the
//! whole thing testable from fixtures.

use crate::models::{
    app::{AppKey, WindowKey},
    element::{Element, RawNode},
    geometry::CoordinateSpace,
    ids::ElementId,
    path::{ElementPath, PathStep},
    snapshot::{Snapshot, WalkBudget},
};

/// Inputs that describe where a tree came from.
#[derive(Clone, Debug)]
pub struct SnapshotContext {
    pub app: AppKey,
    pub window: WindowKey,
    pub window_title: Option<String>,
    pub coordinate_space: CoordinateSpace,
    pub budget: WalkBudget,
    /// When false, elements that are present but not currently on screen are
    /// dropped. `desktop snapshot --all` sets it true.
    pub include_offscreen: bool,
}

impl SnapshotContext {
    #[must_use]
    pub fn new(app: AppKey, window: WindowKey, space: CoordinateSpace) -> Self {
        Self {
            app,
            window,
            window_title: None,
            coordinate_space: space,
            budget: WalkBudget::default(),
            include_offscreen: false,
        }
    }

    #[must_use]
    pub fn with_title(mut self, title: Option<&str>) -> Self {
        self.window_title = title.map(str::to_owned);
        self
    }

    #[must_use]
    pub fn with_budget(mut self, budget: WalkBudget) -> Self {
        self.budget = budget;
        self
    }

    #[must_use]
    pub fn including_offscreen(mut self, include: bool) -> Self {
        self.include_offscreen = include;
        self
    }
}

/// Prunes and numbers `root`.
///
/// When the tree carries no positions at all, every element's bounds are
/// dropped rather than passed on: the coordinates would be the origin by
/// default rather than by measurement, and emitting them invites
/// `click --x --y` at the corner of the screen for every element on the page.
/// `null` says the position is unknown, which is the truth and which a caller
/// can branch on.
pub fn snapshot(root: &RawNode, context: &SnapshotContext) -> Snapshot {
    let mut walker = Walker {
        context,
        elements: Vec::new(),
        visited: 0,
        next_id: 1,
        truncated: false,
    };
    walker.walk(root, &mut Vec::new(), 0, None);

    let mut elements = walker.elements;
    if !reports_positions(root) {
        for element in &mut elements {
            element.bounds = None;
        }
    }

    Snapshot {
        app: Some(context.app.name.clone()),
        window: context.window_title.clone(),
        coordinate_space: context.coordinate_space,
        elements,
        truncated: walker.truncated,
        visited_nodes: walker.visited,
    }
}

/// Whether a tree carries real positions at all.
///
/// GTK4 answers `GetExtents` with a correct size and a position of `(0, 0)` for
/// every node, on X11 as well as Wayland — observed across all 111 nodes of
/// gnome-calculator while its window sat at (44, 92). One element at the origin
/// is ordinary; an entire tree there means the toolkit is reporting no position
/// at all, and the zeros are a default rather than a measurement.
fn reports_positions(root: &RawNode) -> bool {
    fn any_offset(node: &RawNode) -> bool {
        if node.bounds.is_some_and(|b| b.x != 0 || b.y != 0) {
            return true;
        }
        node.children.iter().any(any_offset)
    }
    any_offset(root)
}

struct Walker<'ctx> {
    context: &'ctx SnapshotContext,
    elements: Vec<Element>,
    visited: usize,
    next_id: u32,
    truncated: bool,
}

impl Walker<'_> {
    /// Visits one node and its children.
    ///
    /// An invisible subtree is invisible all the way down, so it is pruned
    /// whole rather than per-node.
    ///
    /// The window root is never emitted: it is already named in the snapshot
    /// header, and including it would shift every element id by one for no
    /// information gain.
    ///
    /// A kept, named node becomes the context its descendants are judged
    /// against, which is how a label repeating its button's text is dropped.
    fn walk(
        &mut self,
        node: &RawNode,
        steps: &mut Vec<PathStep>,
        depth: usize,
        enclosing_name: Option<&str>,
    ) {
        if self.visited >= self.context.budget.max_nodes {
            self.truncated = true;
            return;
        }
        if depth > self.context.budget.max_depth {
            self.truncated = true;
            return;
        }
        self.visited += 1;

        if !self.context.include_offscreen && is_hidden(node) {
            return;
        }

        let kept = depth > 0 && self.should_keep(node) && !restates(node, enclosing_name);
        if kept {
            self.emit(node, steps);
        }

        let inherited = if kept {
            node.name.as_deref().or(enclosing_name)
        } else {
            enclosing_name
        };

        for (index, child) in node.children.iter().enumerate() {
            steps.push(PathStep::new(
                child.role.clone(),
                child.name.as_deref(),
                u16::try_from(index).unwrap_or(u16::MAX),
            ));
            self.walk(child, steps, depth + 1, inherited);
            steps.pop();
        }
    }

    /// Retention rules, in priority order.
    ///
    /// An element advertising an action is actionable regardless of what its
    /// role is called, which is what rescues custom widgets.
    ///
    /// Text is kept on its content alone, *before* geometry is consulted: GTK4
    /// reports height 0 for gnome-calculator's result labels, and pruning those
    /// hid the very output an agent needs to verify what it just did. Geometry
    /// decides whether something can be *clicked*, not whether it exists.
    ///
    /// After that, a zero-area node with nothing to say is layout scaffolding,
    /// and a pure container survives only if it was given a name — which
    /// usually means its author meant it as a landmark.
    fn should_keep(&self, node: &RawNode) -> bool {
        if !node.actions.is_empty() {
            return true;
        }
        if node.role.is_interactive() {
            return true;
        }
        if node.role.is_textual() {
            return has_content(node);
        }
        if node.bounds.is_some_and(|b| b.is_empty()) {
            return false;
        }
        if node.role.is_structural() {
            return false;
        }
        has_content(node)
    }

    /// Records one element.
    ///
    /// The single choke point every snapshot passes through, and where a secret
    /// never becomes an `Option::Some` in the first place.
    fn emit(&mut self, node: &RawNode, steps: &[PathStep]) {
        let secure = node.is_secure();
        let id = ElementId::new(self.next_id);
        self.next_id += 1;

        let path = ElementPath::new(
            self.context.app.clone(),
            self.context.window.clone(),
            steps.to_vec(),
        )
        .with_native(node.native.clone());

        self.elements.push(Element {
            id,
            role: node.role.clone(),
            name: node.name.clone(),
            description: node.description.clone(),
            value: if secure { None } else { node.value.clone() },
            enabled: node.states.enabled,
            focused: node.states.focused,
            selected: node.states.selected,
            redacted: secure,
            bounds: node.bounds,
            actions: node.actions.clone(),
            path: Some(path),
        });
    }
}

/// Whether this node merely restates the accessible name of the nearest kept
/// ancestor.
///
/// GTK4 gives every button a child label carrying the same text, so a plain
/// walk reports each control twice. The duplicate carries no information an
/// agent can act on — it is not separately clickable — and doubles the token
/// cost of every snapshot.
/// Only an exact restatement counts: a label that adds anything is kept.
fn restates(node: &RawNode, enclosing_name: Option<&str>) -> bool {
    if !node.role.is_textual() {
        return false;
    }
    let (Some(name), Some(enclosing)) = (node.name.as_deref(), enclosing_name) else {
        return false;
    };
    node.children.is_empty() && name.trim().eq_ignore_ascii_case(enclosing.trim())
}

/// Whether a subtree is genuinely not part of the rendered UI.
///
/// Only `visible` is consulted. `showing` means "currently rendered on screen",
/// which a background window legitimately is not — Firefox clears it on every
/// toolbar the moment its window stops being frontmost. Pruning on it would
/// mean the tool could only ever see the window on top, which is precisely the
/// case an agent sharing a desktop with a person cannot rely on.
fn is_hidden(node: &RawNode) -> bool {
    !node.states.visible
}

fn has_content(node: &RawNode) -> bool {
    node.name.as_ref().is_some_and(|n| !n.trim().is_empty())
        || node.value.as_ref().is_some_and(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        element::{ElementAction, States},
        geometry::Bounds,
        ids::ProcessId,
        role::Role,
    };

    fn context() -> SnapshotContext {
        SnapshotContext::new(
            AppKey::new(ProcessId::new(1), "Fixture"),
            WindowKey::new(Some("Main"), 0),
            CoordinateSpace::primary_screen(),
        )
        .with_title(Some("Main"))
    }

    fn sized(node: RawNode) -> RawNode {
        node.with_bounds(Bounds::new(0, 0, 10, 10))
    }

    #[test]
    fn interactive_elements_are_kept_and_numbered_in_document_order() {
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::Menu)).with_name("File"),
            sized(RawNode::new(Role::Menu)).with_name("Edit"),
            sized(RawNode::new(Role::Button)).with_name("Run"),
        ]);
        let snap = snapshot(&root, &context());
        let ids: Vec<u32> = snap.elements.iter().map(|e| e.id.get()).collect();
        let names: Vec<Option<&str>> = snap.elements.iter().map(|e| e.name.as_deref()).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(names, vec![Some("File"), Some("Edit"), Some("Run")]);
    }

    #[test]
    fn unnamed_layout_containers_are_pruned_but_their_children_survive() {
        // This is the GTK4 shape probed on GNOME 49: nested unnamed `generic`
        // nodes wrapping the real content.
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::Group))
                .with_children(vec![sized(RawNode::new(Role::Group)).with_children(vec![
                    sized(RawNode::new(Role::Button)).with_name("Save"),
                ])]),
        ]);
        let snap = snapshot(&root, &context());
        assert_eq!(snap.elements.len(), 1);
        assert_eq!(snap.elements[0].name.as_deref(), Some("Save"));
        // The saving is visible in the counts.
        assert_eq!(snap.visited_nodes, 4);
    }

    #[test]
    fn zero_area_nodes_with_nothing_to_say_are_dropped_as_layout_scaffolding() {
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            RawNode::new(Role::Panel).with_bounds(Bounds::new(0, 0, 0, 0)),
            sized(RawNode::new(Role::Label)).with_name("real"),
        ]);
        let snap = snapshot(&root, &context());
        let names: Vec<Option<&str>> = snap.elements.iter().map(|e| e.name.as_deref()).collect();
        assert_eq!(names, vec![Some("real")]);
    }

    #[test]
    fn text_with_broken_geometry_is_kept_because_content_is_what_makes_it_useful() {
        // gnome-calculator on GTK4 reports height 0 for its result labels.
        // Pruning those hid the answer the agent had just computed — the one
        // thing it needed to read back.
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            RawNode::new(Role::Label)
                .with_name("10")
                .with_bounds(Bounds::new(9, 280, 0, 0)),
        ]);
        let snap = snapshot(&root, &context());
        assert_eq!(snap.elements.len(), 1);
        assert_eq!(snap.elements[0].name.as_deref(), Some("10"));
    }

    #[test]
    fn unnamed_text_with_broken_geometry_is_still_dropped() {
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            RawNode::new(Role::Label).with_bounds(Bounds::new(0, 0, 0, 0)),
        ]);
        assert!(snapshot(&root, &context()).elements.is_empty());
    }

    #[test]
    fn a_background_window_is_still_fully_inspectable() {
        // Probed on Firefox: every toolbar reports visible=true, showing=false
        // while the window sits behind another. An agent must still be able to
        // read and act on it.
        let mut backgrounded = States::usable();
        backgrounded.showing = false;
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::Toolbar))
                .with_states(backgrounded)
                .with_children(vec![
                    sized(RawNode::new(Role::Button))
                        .with_name("Reload")
                        .with_states(backgrounded)
                        .with_actions(&[ElementAction::Press]),
                ]),
        ]);
        let snap = snapshot(&root, &context());
        assert_eq!(
            snap.elements
                .iter()
                .filter_map(|e| e.name.as_deref())
                .collect::<Vec<_>>(),
            vec!["Reload"]
        );
    }

    #[test]
    fn hidden_subtrees_are_pruned_whole() {
        let mut hidden = States::usable();
        hidden.visible = false;
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::Panel))
                .with_states(hidden)
                .with_children(vec![sized(RawNode::new(Role::Button)).with_name("Buried")]),
            sized(RawNode::new(Role::Button)).with_name("Visible"),
        ]);
        let snap = snapshot(&root, &context());
        let names: Vec<Option<&str>> = snap.elements.iter().map(|e| e.name.as_deref()).collect();
        assert_eq!(names, vec![Some("Visible")]);
    }

    #[test]
    fn offscreen_elements_can_be_requested_explicitly() {
        let mut hidden = States::usable();
        hidden.visible = false;
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::Button))
                .with_name("Buried")
                .with_states(hidden),
        ]);
        let snap = snapshot(&root, &context().including_offscreen(true));
        assert_eq!(snap.elements.len(), 1);
    }

    #[test]
    fn a_custom_widget_with_an_action_is_kept_even_with_an_unknown_role() {
        // Roles we have never heard of are common; an advertised action is the
        // reliable signal that something is actionable.
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::Other("AXBrandNewThing".to_owned())))
                .with_actions(&[ElementAction::Press]),
        ]);
        let snap = snapshot(&root, &context());
        assert_eq!(snap.elements.len(), 1);
        assert_eq!(snap.elements[0].actions, vec![ElementAction::Press]);
    }

    #[test]
    fn password_values_are_withheld_and_flagged_regardless_of_any_policy() {
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::PasswordField))
                .with_name("Password")
                .with_value("hunter2"),
        ]);
        let snap = snapshot(&root, &context());
        assert_eq!(snap.elements.len(), 1);
        assert_eq!(snap.elements[0].value, None);
        assert!(snap.elements[0].redacted);

        let json = serde_json::to_string(&snap).expect("serializes");
        assert!(!json.contains("hunter2"), "secret leaked into {json}");
        assert!(
            !snap.render().contains("hunter2"),
            "secret leaked into render"
        );
    }

    #[test]
    fn a_protected_state_redacts_even_when_the_role_looks_ordinary() {
        let mut protected = States::usable();
        protected.protected = true;
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::TextBox))
                .with_name("Token")
                .with_value("s3cret")
                .with_states(protected),
        ]);
        let snap = snapshot(&root, &context());
        assert!(snap.elements[0].redacted);
        assert_eq!(snap.elements[0].value, None);
        assert!(
            !serde_json::to_string(&snap)
                .expect("serializes")
                .contains("s3cret")
        );
    }

    #[test]
    fn the_node_budget_stops_the_walk_and_announces_truncation() {
        let children: Vec<RawNode> = (0..50)
            .map(|i| sized(RawNode::new(Role::Button)).with_name(&format!("b{i}")))
            .collect();
        let root = sized(RawNode::new(Role::Window)).with_children(children);
        let context = context().with_budget(WalkBudget {
            max_nodes: 10,
            max_depth: 40,
        });
        let snap = snapshot(&root, &context);
        assert!(snap.truncated);
        assert!(snap.elements.len() < 50);
    }

    #[test]
    fn the_depth_budget_stops_runaway_recursion_and_announces_truncation() {
        let mut node = sized(RawNode::new(Role::Button)).with_name("deep");
        for _ in 0..30 {
            node = sized(RawNode::new(Role::Group)).with_children(vec![node]);
        }
        let context = context().with_budget(WalkBudget {
            max_nodes: 4_000,
            max_depth: 5,
        });
        let snap = snapshot(&node, &context);
        assert!(snap.truncated);
        assert!(snap.elements.is_empty());
    }

    #[test]
    fn every_emitted_element_carries_a_path_that_resolves_back_to_its_node() {
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::Toolbar)).with_children(vec![
                sized(RawNode::new(Role::Button)).with_name("Save"),
                sized(RawNode::new(Role::Button)).with_name("Run"),
            ]),
        ]);
        let snap = snapshot(&root, &context());
        assert!(!snap.elements.is_empty());
        for element in &snap.elements {
            let path = element.path.as_ref().expect("every element carries a path");
            let resolved = crate::models::path::resolve(&root, &path.steps).expect("path resolves");
            assert_eq!(resolved.name, element.name);
            assert_eq!(resolved.role, element.role);
        }
    }

    #[test]
    fn the_window_root_is_not_itself_an_element_since_the_header_already_names_it() {
        let root = sized(RawNode::new(Role::Window))
            .with_name("Main")
            .with_children(vec![sized(RawNode::new(Role::Button)).with_name("Save")]);
        let snap = snapshot(&root, &context());
        assert_eq!(snap.elements.len(), 1);
        assert_eq!(snap.elements[0].id.get(), 1);
        assert_eq!(snap.elements[0].name.as_deref(), Some("Save"));
    }

    #[test]
    fn a_label_restating_its_buttons_text_is_dropped_as_a_duplicate() {
        // The GTK4 shape probed on gnome-calculator: every button contains a
        // label with identical text, which doubled every snapshot.
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::Button))
                .with_name("7")
                .with_actions(&[ElementAction::Press])
                .with_children(vec![sized(RawNode::new(Role::Label)).with_name("7")]),
        ]);
        let snap = snapshot(&root, &context());
        let rendered: Vec<(String, Option<&str>)> = snap
            .elements
            .iter()
            .map(|e| (e.role.as_str().into_owned(), e.name.as_deref()))
            .collect();
        assert_eq!(rendered, vec![("button".to_owned(), Some("7"))]);
    }

    #[test]
    fn a_label_that_adds_information_is_kept_alongside_its_container() {
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::Button))
                .with_name("Save")
                .with_actions(&[ElementAction::Press])
                .with_children(vec![
                    sized(RawNode::new(Role::Label)).with_name("Save as draft"),
                ]),
        ]);
        let snap = snapshot(&root, &context());
        assert_eq!(snap.elements.len(), 2);
        assert_eq!(snap.elements[1].name.as_deref(), Some("Save as draft"));
    }

    #[test]
    fn duplicate_detection_ignores_case_and_padding_but_not_content() {
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::Button))
                .with_name("Save")
                .with_actions(&[ElementAction::Press])
                .with_children(vec![sized(RawNode::new(Role::Label)).with_name("  save  ")]),
        ]);
        assert_eq!(snapshot(&root, &context()).elements.len(), 1);
    }

    #[test]
    fn an_interactive_child_is_never_dropped_as_a_duplicate() {
        // Two nested clickable things with the same name are genuinely two
        // targets; collapsing them would make one unreachable.
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::Button))
                .with_name("x")
                .with_actions(&[ElementAction::Press])
                .with_children(vec![
                    sized(RawNode::new(Role::ToggleButton))
                        .with_name("x")
                        .with_actions(&[ElementAction::Toggle]),
                ]),
        ]);
        assert_eq!(snapshot(&root, &context()).elements.len(), 2);
    }

    #[test]
    fn snapshot_output_is_deterministic_for_the_same_input() {
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::Button)).with_name("A"),
            sized(RawNode::new(Role::Button)).with_name("B"),
        ]);
        let first = snapshot(&root, &context());
        let second = snapshot(&root, &context());
        assert_eq!(first, second);
    }

    #[test]
    fn a_tree_with_no_positions_reports_unknown_bounds_rather_than_the_origin() {
        // GTK4 answers GetExtents with a real size and a position of (0,0) for
        // every node, on X11 as well as Wayland. Passing those through would
        // aim `click --x --y` at the corner of the screen for every element.
        let root = RawNode::new(Role::Window)
            .with_bounds(Bounds::new(0, 0, 370, 616))
            .with_children(vec![
                RawNode::new(Role::Button)
                    .with_name("7")
                    .with_bounds(Bounds::new(0, 0, 64, 44))
                    .with_actions(&[ElementAction::Press]),
                RawNode::new(Role::Button)
                    .with_name("=")
                    .with_bounds(Bounds::new(0, 0, 64, 92))
                    .with_actions(&[ElementAction::Press]),
            ]);
        let snap = snapshot(&root, &context());
        assert_eq!(snap.elements.len(), 2);
        for element in &snap.elements {
            assert_eq!(
                element.bounds, None,
                "{:?} kept a position the toolkit never measured",
                element.name
            );
        }
    }

    #[test]
    fn a_tree_that_does_report_positions_keeps_every_one_of_them() {
        let root = RawNode::new(Role::Window)
            .with_bounds(Bounds::new(0, 0, 800, 600))
            .with_children(vec![
                // At the origin, and genuinely so: a sibling proves the
                // toolkit is measuring.
                RawNode::new(Role::Button)
                    .with_name("A")
                    .with_bounds(Bounds::new(0, 0, 80, 32))
                    .with_actions(&[ElementAction::Press]),
                RawNode::new(Role::Button)
                    .with_name("B")
                    .with_bounds(Bounds::new(120, 240, 80, 32))
                    .with_actions(&[ElementAction::Press]),
            ]);
        let snap = snapshot(&root, &context());
        assert_eq!(snap.elements[0].bounds, Some(Bounds::new(0, 0, 80, 32)));
        assert_eq!(snap.elements[1].bounds, Some(Bounds::new(120, 240, 80, 32)));
    }

    #[test]
    fn a_position_anywhere_in_the_tree_is_enough_to_trust_the_rest() {
        // The signal is whole-tree: one measured offset means the toolkit
        // reports positions, so the zeros elsewhere are real.
        let root = RawNode::new(Role::Window)
            .with_bounds(Bounds::new(0, 0, 800, 600))
            .with_children(vec![
                RawNode::new(Role::Panel)
                    .with_bounds(Bounds::new(0, 0, 400, 300))
                    .with_children(vec![
                        RawNode::new(Role::Button)
                            .with_name("deep")
                            .with_bounds(Bounds::new(4, 8, 80, 32))
                            .with_actions(&[ElementAction::Press]),
                    ]),
            ]);
        let snap = snapshot(&root, &context());
        assert_eq!(
            snap.elements.last().and_then(|e| e.bounds),
            Some(Bounds::new(4, 8, 80, 32))
        );
    }

    #[test]
    fn labels_without_content_are_dropped_but_labels_with_text_are_kept() {
        let root = sized(RawNode::new(Role::Window)).with_children(vec![
            sized(RawNode::new(Role::Label)),
            sized(RawNode::new(Role::Label)).with_name("   "),
            sized(RawNode::new(Role::StatusBar)).with_name("Ln 18, Col 4"),
        ]);
        let snap = snapshot(&root, &context());
        let names: Vec<Option<&str>> = snap.elements.iter().map(|e| e.name.as_deref()).collect();
        assert_eq!(names, vec![Some("Ln 18, Col 4")]);
    }
}
