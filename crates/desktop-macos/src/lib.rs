//! macOS platform adapters for `desktop-driver`.
//!
//! Three system APIs, one per concern: `AXUIElement` for the semantic tree,
//! `CGWindowListCopyWindowInfo` for applications and windows, ScreenCaptureKit
//! for capture and `CGEvent` for input.
//!
//! Three permissions gate it, as separate TCC checks: **Accessibility** (tree
//! and element actions), **Screen Recording** (capture and window titles), and
//! **Post Events** (synthetic pointer and keyboard input). `desktop doctor`
//! explains each one.
//!
//! Minimum supported macOS is **14 (Sonoma)** — `SCScreenshotManager` requires
//! it, and `CGWindowListCreateImage` is obsoleted in the macOS 15 SDK.
//!
//! The AX and CGEvent APIs are C APIs over raw pointers. The `unsafe` needed to
//! call them is confined to the `ax`, `capture`, `input` and `process`
//! modules, each block carrying its own safety comment; nothing above those
//! modules is unsafe. The crate denies `unsafe_code` and grants it back one
//! module at a time, so the confinement is a compile error to breach rather
//! than a convention — a crate-wide `allow` would silence the deny entirely.
//!
//! The whole crate is macOS-only, and it is the contents that are gated rather
//! than the dependency, so the Linux side type-checks without an SDK.
#![deny(unsafe_code)]
#![cfg(target_os = "macos")]

mod a11y;
#[allow(unsafe_code)]
mod ax;
mod ax_constants;
#[allow(unsafe_code)]
mod capture;
#[allow(unsafe_code)]
mod input;
mod probe;
#[allow(unsafe_code)]
mod process;

use desktop_core::{errors::Result, models::backend::BackendInfo, ports::Ports};

pub use probe::{capabilities, diagnostics, info as backend_info};

/// Builds the port set for this Mac.
pub fn build_ports() -> Result<Ports> {
    Ok(Ports {
        accessibility: Box::new(a11y::Accessibility::new()),
        capture: Box::new(capture::ScreenCaptureKit::new()),
        input: Box::new(input::CoreGraphicsInput::new()),
        probe: Box::new(probe::MacosProbe::new()),
    })
}

/// Ports that can only describe the environment.
///
/// `info`, `capabilities` and `doctor` must work when a permission is missing,
/// since those are the commands that explain the missing permission.
#[must_use]
pub fn describe_only_ports() -> Ports {
    Ports {
        accessibility: Box::new(unsupported::Accessibility),
        capture: Box::new(unsupported::Capture),
        input: Box::new(unsupported::Input),
        probe: Box::new(probe::MacosProbe::new()),
    }
}

/// The environment description, available without any permission.
#[must_use]
pub fn detect() -> BackendInfo {
    probe::info()
}

/// Ports that refuse everything, used by [`describe_only_ports`].
mod unsupported {
    use desktop_core::{
        errors::{DesktopError, Permission, Result},
        models::{
            backend::Platform,
            chord::Chord,
            geometry::{CoordinateSpace, Point, ScrollDelta},
            image::Image,
        },
        ports::{
            AccessibilityPort, CapturePort, CaptureTarget, InputPort, MouseButton, ResolvedTree,
        },
    };

    fn needs_accessibility() -> DesktopError {
        DesktopError::PermissionRequired {
            permission: Permission::Accessibility,
            platform: Platform::Macos,
            remedy: crate::probe::accessibility_remedy(),
        }
    }

    pub struct Accessibility;

    impl AccessibilityPort for Accessibility {
        fn list_apps(&self) -> Result<Vec<desktop_core::models::app::Application>> {
            Err(needs_accessibility())
        }
        fn list_windows(
            &self,
            _app: Option<&desktop_core::models::app::AppKey>,
        ) -> Result<Vec<desktop_core::models::app::Window>> {
            Err(needs_accessibility())
        }
        fn tree(
            &self,
            _target: &desktop_core::models::selector::Target,
            _budget: desktop_core::models::snapshot::WalkBudget,
        ) -> Result<ResolvedTree> {
            Err(needs_accessibility())
        }
        fn resolve(
            &self,
            _path: &desktop_core::models::path::ElementPath,
        ) -> Result<desktop_core::models::element::RawNode> {
            Err(needs_accessibility())
        }
        fn perform(
            &self,
            _path: &desktop_core::models::path::ElementPath,
            _action: desktop_core::models::element::ElementAction,
        ) -> Result<()> {
            Err(needs_accessibility())
        }
        fn set_text(
            &self,
            _path: &desktop_core::models::path::ElementPath,
            _text: &str,
        ) -> Result<()> {
            Err(needs_accessibility())
        }
        fn focus(&self, _target: &desktop_core::models::selector::Target) -> Result<()> {
            Err(needs_accessibility())
        }
    }

    pub struct Capture;

    impl CapturePort for Capture {
        fn capture(&self, _target: &CaptureTarget) -> Result<Image> {
            Err(DesktopError::PermissionRequired {
                permission: Permission::ScreenRecording,
                platform: Platform::Macos,
                remedy: crate::probe::screen_recording_remedy(),
            })
        }
    }

    pub struct Input;

    impl InputPort for Input {
        fn move_mouse(&self, _point: Point, _space: &CoordinateSpace) -> Result<()> {
            Err(needs_accessibility())
        }
        fn click(
            &self,
            _point: Point,
            _space: &CoordinateSpace,
            _button: MouseButton,
            _count: u8,
        ) -> Result<()> {
            Err(needs_accessibility())
        }
        fn type_text(&self, _text: &str) -> Result<()> {
            Err(needs_accessibility())
        }
        fn key(&self, _chord: &Chord) -> Result<()> {
            Err(needs_accessibility())
        }
        fn scroll(&self, _delta: ScrollDelta, _space: &CoordinateSpace) -> Result<()> {
            Err(needs_accessibility())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn building_capture_ports_does_not_require_accessibility_permission() {
        // Individual operations enforce their own TCC grant. Construction
        // must stay permission-independent so Screen Recording can work on a
        // machine where Accessibility is intentionally denied.
        assert!(super::build_ports().is_ok());
    }
}
