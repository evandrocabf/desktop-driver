//! The macOS accessibility adapter.
//!
//! Unlike AT-SPI, `AXUIElementRef` is an opaque process-local `CFTypeRef`: it
//! cannot be written to disk and read back by the next `desktop` invocation.
//! So [`RawNode::native`] stays `None` here and every `--element N` is resolved
//! by re-walking the tree along its recorded path — which is exactly why the
//! core models identity as a path rather than a handle.

use std::time::{Duration, Instant};

use desktop_core::{
    errors::{DesktopError, Permission, Result},
    models::{
        app::{AppKey, Application, Window},
        backend::Platform,
        element::{ElementAction, RawNode, States},
        geometry::CoordinateSpace,
        ids::WindowId,
        path::{ElementPath, StaleReason, hash_name},
        role,
        selector::Target,
        snapshot::WalkBudget,
    },
    ports::{AccessibilityPort, ResolvedTree},
};

use crate::{
    ax::{self, Element},
    ax_constants::{action, attribute},
    process,
};

pub struct Accessibility;

impl Accessibility {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn ensure_trusted() -> Result<()> {
        if ax::is_trusted() {
            return Ok(());
        }
        Err(DesktopError::PermissionRequired {
            permission: Permission::Accessibility,
            platform: Platform::Macos,
            remedy: crate::probe::accessibility_remedy(),
        })
    }

    fn applications() -> Vec<(AppKey, Element)> {
        process::running_applications()
            .into_iter()
            .map(|app| {
                let element = Element::for_application(app.pid.get());
                (app, element)
            })
            .collect()
    }

    fn find_app(target: &Target) -> Result<(AppKey, Element)> {
        let apps = Self::applications();
        match target {
            Target::App(needle) => apps
                .into_iter()
                .find(|(key, _)| key.matches(needle))
                .ok_or_else(|| DesktopError::TargetNotFound {
                    target: target.describe(),
                }),
            Target::Focused => {
                let frontmost = process::frontmost_pid();
                apps.into_iter()
                    .find(|(key, _)| Some(key.pid) == frontmost)
                    .ok_or_else(|| DesktopError::TargetNotFound {
                        target: target.describe(),
                    })
            }
            Target::Window(id) => {
                let pid = process::windows()
                    .into_iter()
                    .find(|window| window.id == *id)
                    .map(|window| window.pid)
                    .ok_or_else(|| DesktopError::TargetNotFound {
                        target: target.describe(),
                    })?;
                apps.into_iter()
                    .find(|(key, _)| key.pid == pid)
                    .ok_or_else(|| DesktopError::TargetNotFound {
                        target: target.describe(),
                    })
            }
        }
    }

    fn find_saved_app(key: &AppKey) -> Result<Element> {
        Self::applications()
            .into_iter()
            .find(|(candidate, _)| saved_app_matches(candidate, key))
            .map(|(_, element)| element)
            .ok_or_else(|| DesktopError::TargetNotFound {
                target: format!("application {} ({})", key.name, key.pid),
            })
    }

    fn windows_for(app_key: &AppKey, app: &Element) -> Result<Vec<(WindowId, usize, Element)>> {
        let records: Vec<_> = process::windows()
            .into_iter()
            .filter(|record| record.pid == app_key.pid)
            .collect();
        let mut used = Vec::<WindowId>::new();
        app.windows()
            .into_iter()
            .enumerate()
            .map(|(index, window)| {
                let title = window
                    .string(attribute::TITLE)
                    .filter(|text| !text.is_empty());
                let bounds = window.bounds();
                let record = records
                    .iter()
                    .find(|record| {
                        !used.contains(&record.id)
                            && record.title == title
                            && record.bounds == bounds
                    })
                    .or_else(|| {
                        records.iter().find(|record| {
                            !used.contains(&record.id) && title.is_some() && record.title == title
                        })
                    })
                    .or_else(|| records.get(index).filter(|r| !used.contains(&r.id)))
                    .or_else(|| records.iter().find(|r| !used.contains(&r.id)));
                let id = record.map(|record| record.id).ok_or_else(|| {
                    DesktopError::backend(format!(
                        "cannot correlate AX window {index} of {} with Core Graphics",
                        app_key.name
                    ))
                })?;
                used.push(id);
                Ok((id, index, window))
            })
            .collect()
    }

    /// Picks the window a target designates.
    ///
    /// A focused target uses AXFocusedWindow, an application target uses its
    /// main window, and both fall back to the first top-level window.
    fn find_window(
        app_key: &AppKey,
        app: &Element,
        target: &Target,
    ) -> Result<(WindowId, usize, Element)> {
        let windows = Self::windows_for(app_key, app)?;
        if windows.is_empty() {
            return Err(DesktopError::TargetNotFound {
                target: target.describe(),
            });
        }
        match target {
            Target::Window(id) => windows
                .into_iter()
                .find(|(candidate, _, _)| candidate == id)
                .ok_or_else(|| DesktopError::TargetNotFound {
                    target: target.describe(),
                }),
            Target::Focused | Target::App(_) => {
                let preferred = if matches!(target, Target::Focused) {
                    app.element(attribute::FOCUSED_WINDOW)
                        .or_else(|| app.element(attribute::MAIN_WINDOW))
                } else {
                    app.element(attribute::MAIN_WINDOW)
                };
                if let Some(preferred) = preferred {
                    let title = preferred.string(attribute::TITLE);
                    let bounds = preferred.bounds();
                    if let Some((id, index, _)) = windows.iter().find(|(_, _, window)| {
                        window.string(attribute::TITLE) == title && window.bounds() == bounds
                    }) {
                        return Ok((*id, *index, preferred));
                    }
                }
                windows
                    .into_iter()
                    .next()
                    .ok_or_else(|| DesktopError::TargetNotFound {
                        target: target.describe(),
                    })
            }
        }
    }

    fn find_saved_window(
        app_key: &AppKey,
        app: &Element,
        key: &desktop_core::models::app::WindowKey,
    ) -> Result<Element> {
        let mut windows = Self::windows_for(app_key, app)?;
        let title_matches = windows
            .iter()
            .filter(|(_, _, window)| window.string(attribute::TITLE) == key.title)
            .count();
        if title_matches == 1 {
            let position = windows
                .iter()
                .position(|(_, _, window)| window.string(attribute::TITLE) == key.title)
                .expect("one title match was counted");
            return Ok(windows.swap_remove(position).2);
        }
        windows
            .into_iter()
            .find(|(_, index, window)| {
                *index == usize::from(key.index)
                    && (title_matches == 0
                        || key.title.is_none()
                        || window.string(attribute::TITLE) == key.title)
            })
            .map(|(_, _, window)| window)
            .ok_or_else(|| DesktopError::TargetNotFound {
                target: format!("window {:?} of {}", key.title, app_key.name),
            })
    }

    /// Depth-first walk building the normalized tree.
    ///
    /// A control's label is `AXTitle`, but many report only `AXDescription`,
    /// and a menu item's text can live in `AXValue`, so all three are consulted.
    ///
    /// An absent boolean means "not reported" rather than false: treating a
    /// missing `AXEnabled` as disabled would mark most of an application
    /// unusable. `protected` is left false because macOS marks a secret field
    /// by subrole, which the role mapping has already turned into
    /// `Role::PasswordField`.
    ///
    /// The native handle is deliberately unset: an `AXUIElementRef` cannot
    /// cross a process boundary, so re-resolution always walks the path.
    fn walk(element: &Element, budget: WalkBudget, depth: usize, visited: &mut usize) -> RawNode {
        *visited += 1;

        let role_name = element.string(attribute::ROLE).unwrap_or_default();
        let subrole = element.string(attribute::SUBROLE);
        let role = role::from_ax(&role_name, subrole.as_deref());

        let name = accessible_name(element);

        let actions: Vec<ElementAction> = element
            .action_names()
            .iter()
            .filter_map(|name| ElementAction::from_platform(name))
            .fold(Vec::new(), |mut acc, action| {
                if !acc.contains(&action) {
                    acc.push(action);
                }
                acc
            });

        let value = element.value_string(attribute::VALUE);
        let focused = element.boolean(attribute::FOCUSED);
        let mut node = RawNode::new(role.clone());
        node.name = name;
        node.description = element
            .string(attribute::HELP)
            .filter(|text| !text.is_empty());
        node.value = value.clone();
        node.bounds = element.bounds();
        node.actions = actions;
        node.states = States {
            enabled: element.boolean(attribute::ENABLED).unwrap_or(true),
            focused: focused.unwrap_or(false),
            // AX exposes `AXFocused` only on elements that participate in the
            // focus system. Reuse the value already read above rather than
            // adding another IPC round-trip for every node in a large tree.
            focusable: focused.is_some(),
            selected: element.boolean(attribute::SELECTED).unwrap_or(false),
            checked: checked_from_value(&role, value.as_deref()),
            expanded: element
                .boolean(attribute::EXPANDED)
                .or_else(|| element.boolean(attribute::DISCLOSING))
                .unwrap_or(false),
            visible: !element.boolean(attribute::HIDDEN).unwrap_or(false),
            showing: !element.boolean(attribute::HIDDEN).unwrap_or(false),
            protected: false,
        };
        node.native = None;

        if depth < budget.max_depth && *visited < budget.max_nodes {
            let remaining = budget.max_nodes.saturating_sub(*visited);
            for child in element.children_limited(remaining) {
                if *visited >= budget.max_nodes {
                    break;
                }
                node.children
                    .push(Self::walk(&child, budget, depth + 1, visited));
            }
        }
        node
    }

    fn identity(element: &Element) -> (desktop_core::models::role::Role, Option<u64>) {
        let role_name = element.string(attribute::ROLE).unwrap_or_default();
        let subrole = element.string(attribute::SUBROLE);
        let role = role::from_ax(&role_name, subrole.as_deref());
        let name = accessible_name(element);
        (role, name.as_deref().map(hash_name))
    }

    /// Resolves and returns the exact live AX handle that was validated.
    ///
    /// This mirrors `desktop_core::path::resolve`, but keeps the live object so
    /// a sibling reorder cannot validate one element and act on another.
    fn resolve_element(target: &ElementPath) -> Result<Element> {
        let app = Self::find_saved_app(&target.app)?;
        let mut current = Self::find_saved_window(&target.app, &app, &target.window)?;

        for (depth, step) in target.steps.iter().enumerate() {
            let mut children = current.children();
            if children.is_empty() {
                return Err(stale(StaleReason::PathTruncated { depth }));
            }
            let matching_indices: Vec<usize> = children
                .iter()
                .enumerate()
                .filter(|(_, child)| {
                    let (role, name_hash) = Self::identity(child);
                    role == step.role && name_hash == step.name_hash
                })
                .map(|(index, _)| index)
                .collect();
            let index = match matching_indices.as_slice() {
                [only] => *only,
                [] => {
                    let index = usize::from(step.index);
                    let Some(child) = children.get(index) else {
                        return Err(stale(StaleReason::PathTruncated { depth }));
                    };
                    let (found, _) = Self::identity(child);
                    if found != step.role {
                        return Err(stale(StaleReason::RoleChanged {
                            depth,
                            expected: step.role.clone(),
                            found,
                        }));
                    }
                    index
                }
                many => {
                    let index = usize::from(step.index);
                    if many.contains(&index) {
                        index
                    } else {
                        return Err(stale(StaleReason::Ambiguous {
                            depth,
                            matches: many.len(),
                        }));
                    }
                }
            };
            current = children.swap_remove(index);
        }
        Ok(current)
    }
}

fn non_empty(text: Option<String>) -> Option<String> {
    text.filter(|text| !text.trim().is_empty())
}

/// The best human-facing label macOS exposes for an element.
///
/// Native controls often put their label in `AXTitleUIElement` and web/native
/// text fields commonly expose only a placeholder. Keeping this in one helper
/// also makes snapshot identity use exactly the name the user saw.
fn accessible_name(element: &Element) -> Option<String> {
    non_empty(element.string(attribute::TITLE))
        .or_else(|| {
            element
                .element(attribute::TITLE_UI_ELEMENT)
                .and_then(|label| {
                    non_empty(label.string(attribute::TITLE))
                        .or_else(|| non_empty(label.value_string(attribute::VALUE)))
                        .or_else(|| non_empty(label.string(attribute::DESCRIPTION)))
                })
        })
        .or_else(|| non_empty(element.string(attribute::DESCRIPTION)))
        .or_else(|| non_empty(element.string(attribute::PLACEHOLDER_VALUE)))
}

fn checked_from_value(role: &desktop_core::models::role::Role, value: Option<&str>) -> bool {
    if !matches!(
        role,
        desktop_core::models::role::Role::CheckBox
            | desktop_core::models::role::Role::RadioButton
            | desktop_core::models::role::Role::Switch
            | desktop_core::models::role::Role::ToggleButton
    ) {
        return false;
    }
    value.is_some_and(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "off" | "unchecked"
        )
    })
}

fn saved_app_matches(candidate: &AppKey, recorded: &AppKey) -> bool {
    candidate.pid == recorded.pid
        && candidate.name == recorded.name
        && recorded
            .identifier
            .as_ref()
            .is_none_or(|identifier| candidate.identifier.as_ref() == Some(identifier))
}

fn stale(reason: StaleReason) -> DesktopError {
    DesktopError::ElementStale {
        element: desktop_core::models::ids::ElementId::new(0),
        reason,
    }
}

impl AccessibilityPort for Accessibility {
    fn list_apps(&self) -> Result<Vec<Application>> {
        Self::ensure_trusted()?;
        let frontmost = process::frontmost_pid();
        Ok(process::running_applications()
            .into_iter()
            .map(|key| {
                let element = Element::for_application(key.pid.get());
                Application {
                    pid: key.pid,
                    name: key.name.clone(),
                    identifier: key.identifier.clone(),
                    active: Some(key.pid) == frontmost,
                    window_count: u32::try_from(element.windows().len()).unwrap_or(0),
                }
            })
            .collect())
    }

    /// The windows of one application, or of all of them.
    ///
    /// Bounds are real screen coordinates here, unlike under Wayland.
    ///
    /// Every window is reported as accessible because the list comes from the
    /// accessibility API itself, so anything in it necessarily has a tree. The
    /// field exists for Linux, where the window manager sees windows AT-SPI
    /// does not.
    fn list_windows(&self, app: Option<&AppKey>) -> Result<Vec<Window>> {
        Self::ensure_trusted()?;
        let mut out = Vec::new();
        for (key, element) in Self::applications() {
            if let Some(filter) = app
                && key.pid != filter.pid
            {
                continue;
            }
            for (id, index, window) in Self::windows_for(&key, &element)? {
                out.push(Window {
                    id,
                    title: window.string(attribute::TITLE),
                    app: key.clone(),
                    bounds: window.bounds(),
                    focused: window.boolean(attribute::FOCUSED).unwrap_or(false),
                    minimized: window.boolean(attribute::MINIMIZED).unwrap_or(false),
                    accessible: true,
                    index: u16::try_from(index).unwrap_or(u16::MAX),
                });
            }
        }
        Ok(out)
    }

    /// The window a target designates, and its tree.
    ///
    /// Reported in screen space, which genuinely exists on macOS.
    fn tree(&self, target: &Target, budget: WalkBudget) -> Result<ResolvedTree> {
        Self::ensure_trusted()?;
        let (key, app) = Self::find_app(target)?;
        let (id, index, window) = Self::find_window(&key, &app, target)?;
        let mut visited = 0;
        let root = Self::walk(&window, budget, 0, &mut visited);

        Ok(ResolvedTree {
            app: key.clone(),
            window: Window {
                id,
                title: window.string(attribute::TITLE),
                app: key,
                bounds: window.bounds(),
                focused: window.boolean(attribute::FOCUSED).unwrap_or(false),
                minimized: window.boolean(attribute::MINIMIZED).unwrap_or(false),
                accessible: true,
                index: u16::try_from(index).unwrap_or(0),
            },
            root,
            space: CoordinateSpace::primary_screen(),
        })
    }

    fn resolve(&self, target: &ElementPath) -> Result<RawNode> {
        Self::ensure_trusted()?;
        let element = Self::resolve_element(target)?;
        let mut visited = 0;
        Ok(Self::walk(&element, WalkBudget::default(), 0, &mut visited))
    }

    fn perform(&self, target: &ElementPath, requested: ElementAction) -> Result<()> {
        Self::ensure_trusted()?;
        let element = Self::resolve_element(target)?;
        let name = element
            .action_names()
            .into_iter()
            .find(|candidate| ElementAction::from_platform(candidate) == Some(requested))
            .ok_or_else(|| {
                DesktopError::invalid_argument(format!(
                    "element does not offer the {} action",
                    requested.as_str()
                ))
            })?;
        element.perform(&name)
    }

    /// Replaces an element's text by writing `AXValue`.
    ///
    /// That puts the text where the agent looked without a keystroke, a focus
    /// change or a pointer move, so it does not fight whoever else is using the
    /// machine.
    fn set_text(&self, target: &ElementPath, text: &str) -> Result<()> {
        Self::ensure_trusted()?;
        let element = Self::resolve_element(target)?;
        element.set_string(attribute::VALUE, text)?;
        if element.string(attribute::VALUE).as_deref() != Some(text) {
            return Err(DesktopError::backend(
                "the application accepted AXValue but read-back did not match",
            ));
        }
        Ok(())
    }

    /// Raises a window and brings its application forward.
    ///
    /// Raising alone is not enough: the application also has to be activated,
    /// or the raised window sits behind the frontmost one.
    fn focus(&self, target: &Target) -> Result<()> {
        Self::ensure_trusted()?;
        let (key, app) = Self::find_app(target)?;
        let (_, _, window) = Self::find_window(&key, &app, target)?;
        window.perform(action::RAISE)?;
        process::activate(key.pid)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if process::frontmost_pid() == Some(key.pid)
                && window.boolean(attribute::FOCUSED).unwrap_or(true)
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(DesktopError::backend(
            "the application did not become frontmost after AXRaise",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_elements_carry_no_serializable_handle_so_resolution_walks_the_path() {
        // An AXUIElementRef is process-local; recording one would produce a
        // dangling reference in the next invocation.
        let node = RawNode::new(desktop_core::models::role::Role::Button);
        assert!(node.native.is_none());
    }

    #[test]
    fn screen_coordinates_are_real_on_macos_unlike_wayland() {
        assert!(!CoordinateSpace::primary_screen().is_window_relative());
    }

    #[test]
    fn a_recorded_bundle_identity_cannot_disappear_during_resolution() {
        let recorded = AppKey::new(desktop_core::models::ids::ProcessId::new(7), "Fixture")
            .with_identifier("dev.desktop-driver.fixture");
        let candidate = AppKey::new(desktop_core::models::ids::ProcessId::new(7), "Fixture");
        assert!(!saved_app_matches(&candidate, &recorded));
    }

    #[test]
    fn native_toggle_values_are_normalized_into_checked_state() {
        use desktop_core::models::role::Role;

        assert!(checked_from_value(&Role::CheckBox, Some("1")));
        assert!(checked_from_value(&Role::Switch, Some("true")));
        assert!(checked_from_value(&Role::CheckBox, Some("2")));
        assert!(!checked_from_value(&Role::CheckBox, Some("0")));
        assert!(!checked_from_value(&Role::Button, Some("1")));
    }
}
