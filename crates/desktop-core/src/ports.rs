//! The seams platform adapters implement.
//!
//! Four narrow ports rather than one wide trait, because on Linux these are
//! four different subsystems that fail independently: AT-SPI can be perfectly
//! healthy while screen capture is refused and input is unimplemented.
//!
//! All ports are synchronous. `ashpd` and `atspi` are both async-only, so the
//! Linux adapter owns a private current-thread runtime and blocks inside
//! itself. Async stops at the adapter boundary and never reaches the CLI.

use crate::{
    errors::{PermissionState, Result},
    models::{
        app::{AppKey, Application, Window},
        backend::BackendInfo,
        capability::CapabilitySet,
        chord::Chord,
        dependency::SystemDependency,
        element::{ElementAction, RawNode},
        geometry::{CoordinateSpace, Point, ScrollDelta},
        ids::WindowId,
        image::Image,
        path::ElementPath,
        selector::Target,
        snapshot::WalkBudget,
    },
};

/// Reading the semantic UI tree.
pub trait AccessibilityPort: Send + Sync {
    fn list_apps(&self) -> Result<Vec<Application>>;

    fn list_windows(&self, app: Option<&AppKey>) -> Result<Vec<Window>>;

    /// The window a target designates, plus its raw tree.
    fn tree(&self, target: &Target, budget: WalkBudget) -> Result<ResolvedTree>;

    /// Re-walks the live tree to find the element a path describes.
    fn resolve(&self, path: &ElementPath) -> Result<RawNode>;

    /// Activates an element through the accessibility API. Preferred over
    /// pointer synthesis: deterministic, and it needs no portal session.
    fn perform(&self, path: &ElementPath, action: ElementAction) -> Result<()>;

    /// Replaces an element's text through the accessibility API.
    ///
    /// The point of this is what it *doesn't* do: no keystrokes, no focus
    /// change, no pointer movement. On a desktop shared with a human that is
    /// the difference between addressing a field and racing the user for the
    /// keyboard — synthetic typing lands wherever focus happens to be at that
    /// instant, which is not necessarily where the agent looked.
    fn set_text(&self, path: &ElementPath, text: &str) -> Result<()>;

    fn focus(&self, target: &Target) -> Result<()>;
}

/// A window's tree together with everything needed to interpret its geometry.
#[derive(Clone, Debug)]
pub struct ResolvedTree {
    pub app: AppKey,
    pub window: Window,
    pub root: RawNode,
    /// The space `root`'s bounds are expressed in. Window-relative under
    /// Wayland, screen-relative elsewhere.
    pub space: CoordinateSpace,
}

/// What to capture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CaptureTarget {
    Screen,
    Window(WindowId),
    /// The frontmost ordinary window owned by the named application.
    App(String),
}

pub trait CapturePort: Send + Sync {
    /// Resolves an application name/identifier without requiring the
    /// accessibility grant. Policy uses this before an app-scoped capture so
    /// aliases cannot bypass `--allow-app` or `--deny-app`.
    fn resolve_app(&self, _needle: &str) -> Result<Option<AppKey>> {
        Ok(None)
    }

    /// Resolves the owner of an opaque window id for app-scoped policy.
    fn resolve_window_app(&self, _id: WindowId) -> Result<Option<AppKey>> {
        Ok(None)
    }

    fn capture(&self, target: &CaptureTarget) -> Result<Image>;
}

/// Gap an adapter leaves between synthesised characters.
///
/// Not politeness. Several toolkits rebuild their input state on a keystroke
/// and lose whatever arrives during the rebuild, so a string delivered as fast
/// as the wire allows comes out with characters missing and no error anywhere —
/// observed with gnome-calculator, which turned `7+3` into `7`. Shared by every
/// adapter because the failure is a property of the applications being driven,
/// not of the platform driving them.
pub const KEYSTROKE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(12);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

pub trait InputPort: Send + Sync {
    fn move_mouse(&self, point: Point, space: &CoordinateSpace) -> Result<()>;

    fn click(
        &self,
        point: Point,
        space: &CoordinateSpace,
        button: MouseButton,
        count: u8,
    ) -> Result<()>;

    fn type_text(&self, text: &str) -> Result<()>;

    fn key(&self, chord: &Chord) -> Result<()>;

    fn scroll(&self, delta: ScrollDelta, space: &CoordinateSpace) -> Result<()>;
}

/// Describing the environment. Kept separate from the three doing-ports so a
/// backend can report its own limitations even when it can do nothing else.
pub trait PlatformProbe: Send + Sync {
    fn info(&self) -> BackendInfo;
    fn capabilities(&self) -> CapabilitySet;
    fn permissions(&self) -> Vec<PermissionState>;
    /// Environment-specific advice for `desktop doctor`, most-important first.
    fn diagnostics(&self) -> Vec<Diagnostic> {
        Vec::new()
    }

    /// External packages this platform needs, present or not.
    ///
    /// Reported even when everything is installed, so `desktop doctor --json`
    /// is a complete answer to "what does this tool depend on" rather than
    /// only a list of today's problems.
    fn dependencies(&self) -> Vec<SystemDependency> {
        Vec::new()
    }

    /// The command that would install whatever is missing.
    fn install_command(&self) -> Option<String> {
        None
    }

    /// Triggers whatever one-time prompt the platform shows for a missing
    /// grant, and reports the state afterwards.
    ///
    /// Default is a no-op: on Linux the grant is requested implicitly by the
    /// first portal call, so there is nothing to ask for up front.
    fn request_permissions(&self) -> Vec<PermissionState> {
        self.permissions()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// One finding from `desktop doctor`: what is wrong and how to fix it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub summary: String,
    pub remedy: Option<String>,
}

impl Diagnostic {
    #[must_use]
    pub fn error(summary: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            summary: summary.into(),
            remedy: Some(remedy.into()),
        }
    }

    #[must_use]
    pub fn warning(summary: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            summary: summary.into(),
            remedy: Some(remedy.into()),
        }
    }

    #[must_use]
    pub fn info(summary: impl Into<String>) -> Self {
        Self {
            severity: Severity::Info,
            summary: summary.into(),
            remedy: None,
        }
    }
}

/// The four ports a platform supplies.
pub struct Ports {
    pub accessibility: Box<dyn AccessibilityPort>,
    pub capture: Box<dyn CapturePort>,
    pub input: Box<dyn InputPort>,
    pub probe: Box<dyn PlatformProbe>,
}
