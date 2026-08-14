//! Content-addressed element paths.
//!
//! `desktop snapshot` and `desktop click --element 42` run in two different
//! processes. A `AXUIElementRef` is an opaque process-local `CFTypeRef`, so an
//! element id cannot be a pointer. Instead a snapshot records *how to find the
//! element again* — role, name and ordinal at every level from the window root
//! — and acting on it re-walks the live tree.
//!
//! The payoff is that a stale reference is *detected* rather than silently
//! clicking wherever the element used to be.

use serde::{Deserialize, Serialize};

use crate::models::{
    app::{AppKey, WindowKey},
    element::RawNode,
    role::Role,
};

/// FNV-1a over the name, so paths stay compact and comparable.
///
/// Hand-rolled rather than `DefaultHasher` because this value is written to
/// disk and read back by a later process: it must not change between Rust
/// releases.
#[must_use]
pub fn hash_name(name: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// One level of descent from the window root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct PathStep {
    pub role: Role,
    /// Hash of the accessible name, when the element had one. Names are the
    /// most stable identifying property in practice — far more stable than
    /// sibling position, which shifts whenever a list grows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_hash: Option<u64>,
    /// Ordinal among siblings, used to disambiguate and as a fallback.
    pub index: u16,
}

impl PathStep {
    #[must_use]
    pub fn new(role: Role, name: Option<&str>, index: u16) -> Self {
        Self {
            role,
            name_hash: name.map(hash_name),
            index,
        }
    }

    fn matches(&self, node: &RawNode) -> bool {
        node.role == self.role && node.name.as_deref().map(hash_name) == self.name_hash
    }
}

/// A full route to an element: which app, which window, then the descent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ElementPath {
    pub app: AppKey,
    pub window: WindowKey,
    pub steps: Vec<PathStep>,
    /// A serializable platform handle when one exists — an AT-SPI
    /// `(bus_name, object_path)` pair, for instance. Tried first as a fast
    /// path; the steps remain authoritative. macOS leaves this `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<String>,
}

impl ElementPath {
    #[must_use]
    pub fn new(app: AppKey, window: WindowKey, steps: Vec<PathStep>) -> Self {
        Self {
            app,
            window,
            steps,
            native: None,
        }
    }

    #[must_use]
    pub fn with_native(mut self, native: Option<String>) -> Self {
        self.native = native;
        self
    }

    #[must_use]
    pub fn depth(&self) -> usize {
        self.steps.len()
    }
}

/// Why a recorded path no longer designates the element it was recorded for.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum StaleReason {
    /// The tree got shallower — the branch this path descended is gone.
    PathTruncated { depth: usize },
    /// Something is at that position, but it is a different kind of thing.
    RoleChanged {
        depth: usize,
        expected: Role,
        found: Role,
    },
    /// Several siblings match equally well and the recorded ordinal does not
    /// pick one. Guessing here would click the wrong row of a list.
    Ambiguous { depth: usize, matches: usize },
}

/// Re-walks `root` following `steps`.
///
/// At each level an exact role+name match wins. Where several siblings match,
/// the recorded ordinal breaks the tie — but only if it actually points at one
/// of them. Where nothing matches by name, the ordinal is tried alone and
/// accepted only if the role still agrees, which tolerates a renamed label
/// without tolerating a restructured tree.
pub fn resolve<'tree>(
    root: &'tree RawNode,
    steps: &[PathStep],
) -> Result<&'tree RawNode, StaleReason> {
    let mut node = root;

    for (depth, step) in steps.iter().enumerate() {
        if node.children.is_empty() {
            return Err(StaleReason::PathTruncated { depth });
        }

        let matches: Vec<usize> = node
            .children
            .iter()
            .enumerate()
            .filter(|(_, child)| step.matches(child))
            .map(|(index, _)| index)
            .collect();

        let chosen = match matches.as_slice() {
            [only] => *only,
            [] => {
                let index = usize::from(step.index);
                let Some(child) = node.children.get(index) else {
                    return Err(StaleReason::PathTruncated { depth });
                };
                if child.role != step.role {
                    return Err(StaleReason::RoleChanged {
                        depth,
                        expected: step.role.clone(),
                        found: child.role.clone(),
                    });
                }
                index
            }
            many => {
                let index = usize::from(step.index);
                if many.contains(&index) {
                    index
                } else {
                    return Err(StaleReason::Ambiguous {
                        depth,
                        matches: many.len(),
                    });
                }
            }
        };

        node = &node.children[chosen];
    }

    Ok(node)
}

/// Records the path to every node in `root`, in the same depth-first order the
/// snapshot numbers them.
#[must_use]
pub fn record_paths(root: &RawNode) -> Vec<(Vec<PathStep>, &RawNode)> {
    let mut out = Vec::new();
    let mut stack = Vec::new();
    collect(root, &mut stack, &mut out);
    out
}

fn collect<'tree>(
    node: &'tree RawNode,
    stack: &mut Vec<PathStep>,
    out: &mut Vec<(Vec<PathStep>, &'tree RawNode)>,
) {
    out.push((stack.clone(), node));
    for (index, child) in node.children.iter().enumerate() {
        stack.push(PathStep::new(
            child.role.clone(),
            child.name.as_deref(),
            u16::try_from(index).unwrap_or(u16::MAX),
        ));
        collect(child, stack, out);
        stack.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> RawNode {
        RawNode::new(Role::Window)
            .with_name("main.rs")
            .with_children(vec![
                RawNode::new(Role::Toolbar).with_children(vec![
                    RawNode::new(Role::Button).with_name("Save"),
                    RawNode::new(Role::Button).with_name("Run"),
                ]),
                RawNode::new(Role::ListBox).with_children(vec![
                    RawNode::new(Role::ListItem).with_name("alpha"),
                    RawNode::new(Role::ListItem).with_name("beta"),
                    RawNode::new(Role::ListItem).with_name("gamma"),
                ]),
            ])
    }

    fn path_to_run() -> Vec<PathStep> {
        vec![
            PathStep::new(Role::Toolbar, None, 0),
            PathStep::new(Role::Button, Some("Run"), 1),
        ]
    }

    #[test]
    fn hash_is_stable_across_processes_so_paths_survive_being_written_to_disk() {
        // Locked to a literal: changing the hash silently invalidates every
        // snapshot file a user already has on disk.
        assert_eq!(hash_name(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(hash_name("Save"), 3_744_847_701_619_372_040);
    }

    #[test]
    fn hash_distinguishes_different_names() {
        assert_ne!(hash_name("Save"), hash_name("Run"));
    }

    #[test]
    fn resolving_an_unchanged_tree_finds_the_recorded_element() {
        let root = tree();
        let found = resolve(&root, &path_to_run()).expect("resolves");
        assert_eq!(found.name.as_deref(), Some("Run"));
    }

    #[test]
    fn an_empty_path_resolves_to_the_window_root_itself() {
        let root = tree();
        let found = resolve(&root, &[]).expect("resolves");
        assert_eq!(found.name.as_deref(), Some("main.rs"));
    }

    #[test]
    fn a_name_match_wins_over_the_recorded_ordinal_when_siblings_are_reordered() {
        let mut root = tree();
        root.children[0].children.reverse(); // Run is now at index 0.
        let found = resolve(&root, &path_to_run()).expect("resolves");
        assert_eq!(found.name.as_deref(), Some("Run"));
    }

    #[test]
    fn a_renamed_element_is_still_found_by_ordinal_when_its_role_is_unchanged() {
        let mut root = tree();
        root.children[0].children[1].name = Some("Execute".to_owned());
        let found = resolve(&root, &path_to_run()).expect("resolves");
        assert_eq!(found.name.as_deref(), Some("Execute"));
    }

    #[test]
    fn a_replaced_element_of_a_different_role_is_reported_stale_not_clicked() {
        let mut root = tree();
        root.children[0].children[1] = RawNode::new(Role::TextBox).with_name("Filter");
        let error = resolve(&root, &path_to_run()).expect_err("must not resolve");
        assert_eq!(
            error,
            StaleReason::RoleChanged {
                depth: 1,
                expected: Role::Button,
                found: Role::TextBox,
            }
        );
    }

    #[test]
    fn a_vanished_branch_is_reported_as_truncated() {
        let mut root = tree();
        root.children[0].children.clear();
        let error = resolve(&root, &path_to_run()).expect_err("must not resolve");
        assert_eq!(error, StaleReason::PathTruncated { depth: 1 });
    }

    #[test]
    fn identical_siblings_are_disambiguated_by_the_recorded_ordinal() {
        let root = RawNode::new(Role::Window).with_children(vec![
            RawNode::new(Role::ListBox).with_children(vec![
                RawNode::new(Role::ListItem).with_name("row"),
                RawNode::new(Role::ListItem).with_name("row"),
                RawNode::new(Role::ListItem).with_name("row"),
            ]),
        ]);
        let steps = vec![
            PathStep::new(Role::ListBox, None, 0),
            PathStep::new(Role::ListItem, Some("row"), 2),
        ];
        assert!(resolve(&root, &steps).is_ok());
    }

    #[test]
    fn identical_siblings_with_an_out_of_range_ordinal_are_ambiguous_not_a_guess() {
        // Clicking "probably that one" in a list of identical rows is exactly
        // the failure this design exists to prevent.
        let root = RawNode::new(Role::Window).with_children(vec![
            RawNode::new(Role::ListBox).with_children(vec![
                RawNode::new(Role::ListItem).with_name("row"),
                RawNode::new(Role::ListItem).with_name("row"),
            ]),
        ]);
        let steps = vec![
            PathStep::new(Role::ListBox, None, 0),
            PathStep::new(Role::ListItem, Some("row"), 7),
        ];
        let error = resolve(&root, &steps).expect_err("must not resolve");
        assert_eq!(
            error,
            StaleReason::Ambiguous {
                depth: 1,
                matches: 2
            }
        );
    }

    #[test]
    fn recorded_paths_round_trip_back_to_the_nodes_they_describe() {
        let root = tree();
        for (steps, node) in record_paths(&root) {
            let resolved = resolve(&root, &steps).expect("every recorded path resolves");
            assert_eq!(resolved.name, node.name);
            assert_eq!(resolved.role, node.role);
        }
    }

    #[test]
    fn recorded_paths_are_emitted_in_depth_first_order() {
        let root = tree();
        let names: Vec<Option<&str>> = record_paths(&root)
            .iter()
            .map(|(_, node)| node.name.as_deref())
            .collect();
        assert_eq!(
            names,
            vec![
                Some("main.rs"),
                None,
                Some("Save"),
                Some("Run"),
                None,
                Some("alpha"),
                Some("beta"),
                Some("gamma"),
            ]
        );
    }

    #[test]
    fn stale_reason_serializes_with_a_machine_readable_tag() {
        let json = serde_json::to_value(StaleReason::RoleChanged {
            depth: 1,
            expected: Role::Button,
            found: Role::TextBox,
        })
        .expect("serializes");
        assert_eq!(json["reason"], "role_changed");
        assert_eq!(json["expected"], "button");
        assert_eq!(json["found"], "textbox");
    }
}
