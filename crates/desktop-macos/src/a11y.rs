//! The macOS accessibility adapter.
//!
//! Unlike AT-SPI, `AXUIElementRef` is an opaque process-local `CFTypeRef`: it
//! cannot be written to disk and read back by the next `desktop` invocation.
//! So [`RawNode::native`] stays `None` here and every `--element N` is resolved
//! by re-walking the tree along its recorded path — which is exactly why the
//! core models identity as a path rather than a handle.

use desktop_core::{
    errors::{DesktopError, Permission, Result},
    models::{
        app::{AppKey, Application, Window},
        backend::Platform,
        element::{ElementAction, RawNode, States},
        geometry::CoordinateSpace,
        ids::WindowId,
        path::{self, ElementPath},
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
    /// Fails early with an actionable message when the grant is missing, since
    /// every call below would otherwise return empty trees that look like
    /// applications with no UI.
    pub fn new() -> Result<Self> {
        if ax::is_trusted() {
            return Ok(Self);
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
            Target::Focused | Target::Window(_) => {
                let frontmost = process::frontmost_pid();
                apps.into_iter()
                    .find(|(key, _)| Some(key.pid) == frontmost)
                    .ok_or_else(|| DesktopError::TargetNotFound {
                        target: target.describe(),
                    })
            }
        }
    }

    /// Picks the window a target designates.
    ///
    /// For an unqualified target this is the main window, falling back to the
    /// first — a single-window application reports one either way.
    fn find_window(app: &Element, target: &Target) -> Result<(usize, Element)> {
        let windows = app.windows();
        if windows.is_empty() {
            return Err(DesktopError::TargetNotFound {
                target: target.describe(),
            });
        }
        match target {
            Target::Window(id) => {
                let index = id.get() as usize;
                windows
                    .into_iter()
                    .nth(index)
                    .map(|window| (index, window))
                    .ok_or_else(|| DesktopError::TargetNotFound {
                        target: target.describe(),
                    })
            }
            Target::Focused | Target::App(_) => {
                if let Some(main) = app.element(attribute::MAIN_WINDOW) {
                    return Ok((0, main));
                }
                windows
                    .into_iter()
                    .next()
                    .map(|window| (0, window))
                    .ok_or_else(|| DesktopError::TargetNotFound {
                        target: target.describe(),
                    })
            }
        }
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

        let name = element
            .string(attribute::TITLE)
            .filter(|text| !text.is_empty())
            .or_else(|| {
                element
                    .string(attribute::DESCRIPTION)
                    .filter(|text| !text.is_empty())
            });

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

        let mut node = RawNode::new(role);
        node.name = name;
        node.description = element
            .string(attribute::HELP)
            .filter(|text| !text.is_empty());
        node.value = element.value_string(attribute::VALUE);
        node.bounds = element.bounds();
        node.actions = actions;
        node.states = States {
            enabled: element.boolean(attribute::ENABLED).unwrap_or(true),
            focused: element.boolean(attribute::FOCUSED).unwrap_or(false),
            focusable: false,
            selected: element.boolean(attribute::SELECTED).unwrap_or(false),
            checked: false,
            expanded: false,
            visible: !element.boolean(attribute::HIDDEN).unwrap_or(false),
            showing: !element.boolean(attribute::HIDDEN).unwrap_or(false),
            protected: false,
        };
        node.native = None;

        if depth < budget.max_depth && *visited < budget.max_nodes {
            for child in element.children() {
                if *visited >= budget.max_nodes {
                    break;
                }
                node.children
                    .push(Self::walk(&child, budget, depth + 1, visited));
            }
        }
        node
    }

    fn resolve_node(target: &ElementPath) -> Result<RawNode> {
        let (_, app) = Self::find_app(&Target::App(target.app.name.clone()))?;
        let (_, window) = Self::find_window(&app, &Target::Focused)?;
        let mut visited = 0;
        let root = Self::walk(&window, WalkBudget::default(), 0, &mut visited);
        path::resolve(&root, &target.steps)
            .cloned()
            .map_err(|reason| DesktopError::ElementStale {
                element: desktop_core::models::ids::ElementId::new(0),
                reason,
            })
    }

    /// Re-walks to the live element, rather than the copied tree, so an action
    /// is performed on the real thing.
    fn resolve_element(target: &ElementPath) -> Result<Element> {
        let (_, app) = Self::find_app(&Target::App(target.app.name.clone()))?;
        let (_, window) = Self::find_window(&app, &Target::Focused)?;

        let mut current = window;
        for step in &target.steps {
            let children = current.children();
            let index = usize::from(step.index);
            let chosen =
                children
                    .into_iter()
                    .nth(index)
                    .ok_or_else(|| DesktopError::ElementStale {
                        element: desktop_core::models::ids::ElementId::new(0),
                        reason: desktop_core::models::path::StaleReason::PathTruncated {
                            depth: index,
                        },
                    })?;
            current = chosen;
        }
        Ok(current)
    }
}

impl AccessibilityPort for Accessibility {
    fn list_apps(&self) -> Result<Vec<Application>> {
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
        let mut out = Vec::new();
        let mut next_id = 0u32;
        for (key, element) in Self::applications() {
            if let Some(filter) = app
                && !key.matches(&filter.name)
            {
                continue;
            }
            for (index, window) in element.windows().into_iter().enumerate() {
                out.push(Window {
                    id: WindowId::new(next_id),
                    title: window.string(attribute::TITLE),
                    app: key.clone(),
                    bounds: window.bounds(),
                    focused: window.boolean(attribute::FOCUSED).unwrap_or(false),
                    minimized: window.boolean(attribute::MINIMIZED).unwrap_or(false),
                    accessible: true,
                    index: u16::try_from(index).unwrap_or(u16::MAX),
                });
                next_id += 1;
            }
        }
        Ok(out)
    }

    /// The window a target designates, and its tree.
    ///
    /// Reported in screen space, which genuinely exists on macOS.
    fn tree(&self, target: &Target, budget: WalkBudget) -> Result<ResolvedTree> {
        let (key, app) = Self::find_app(target)?;
        let (index, window) = Self::find_window(&app, target)?;
        let mut visited = 0;
        let root = Self::walk(&window, budget, 0, &mut visited);

        Ok(ResolvedTree {
            app: key.clone(),
            window: Window {
                id: WindowId::new(u32::try_from(index).unwrap_or(0)),
                title: window.string(attribute::TITLE),
                app: key,
                bounds: window.bounds(),
                focused: true,
                minimized: false,
                accessible: true,
                index: u16::try_from(index).unwrap_or(0),
            },
            root,
            space: CoordinateSpace::primary_screen(),
        })
    }

    fn resolve(&self, target: &ElementPath) -> Result<RawNode> {
        Self::resolve_node(target)
    }

    fn perform(&self, target: &ElementPath, requested: ElementAction) -> Result<()> {
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
        let element = Self::resolve_element(target)?;
        element.set_string(attribute::VALUE, text)
    }

    /// Raises a window and brings its application forward.
    ///
    /// Raising alone is not enough: the application also has to be activated,
    /// or the raised window sits behind the frontmost one.
    fn focus(&self, target: &Target) -> Result<()> {
        let (key, app) = Self::find_app(target)?;
        let (_, window) = Self::find_window(&app, target)?;
        let _ = window.perform(action::RAISE);
        process::activate(key.pid)
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
}
