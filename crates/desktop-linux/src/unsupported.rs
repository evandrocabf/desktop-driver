//! Ports for capabilities this build does not provide in the current session.
//!
//! These exist so that an unimplemented path produces a structured, specific
//! refusal instead of a plausible-looking result. Under KDE or a wlroots
//! compositor the mechanisms genuinely exist upstream, but pretending we drive
//! them — or quietly falling through to XTEST, which XWayland would happily
//! accept and then deliver nowhere — is the failure mode this whole design is
//! organised against.

use desktop_core::{
    errors::{DesktopError, Result},
    models::{
        backend::{Backend, BackendInfo},
        capability::Capability,
        chord::Chord,
        geometry::{CoordinateSpace, Point, ScrollDelta},
        image::Image,
    },
    ports::{CapturePort, CaptureTarget, InputPort, MouseButton},
};

/// Used by the describe-only port set, so `info` / `capabilities` / `doctor`
/// work even when the accessibility bus is unreachable — which is exactly when
/// a user needs them.
pub struct UnsupportedAccessibility {
    info: BackendInfo,
}

impl UnsupportedAccessibility {
    #[must_use]
    pub const fn new(info: BackendInfo) -> Self {
        Self { info }
    }

    fn refuse(&self) -> DesktopError {
        DesktopError::unsupported(Capability::Accessibility, Backend::None, &self.info)
    }
}

impl desktop_core::ports::AccessibilityPort for UnsupportedAccessibility {
    fn list_apps(&self) -> Result<Vec<desktop_core::models::app::Application>> {
        Err(self.refuse())
    }

    fn list_windows(
        &self,
        _app: Option<&desktop_core::models::app::AppKey>,
    ) -> Result<Vec<desktop_core::models::app::Window>> {
        Err(self.refuse())
    }

    fn tree(
        &self,
        _target: &desktop_core::models::selector::Target,
        _budget: desktop_core::models::snapshot::WalkBudget,
    ) -> Result<desktop_core::ports::ResolvedTree> {
        Err(self.refuse())
    }

    fn resolve(
        &self,
        _path: &desktop_core::models::path::ElementPath,
    ) -> Result<desktop_core::models::element::RawNode> {
        Err(self.refuse())
    }

    fn perform(
        &self,
        _path: &desktop_core::models::path::ElementPath,
        _action: desktop_core::models::element::ElementAction,
    ) -> Result<()> {
        Err(self.refuse())
    }

    fn set_text(&self, _path: &desktop_core::models::path::ElementPath, _text: &str) -> Result<()> {
        Err(self.refuse())
    }

    fn focus(&self, _target: &desktop_core::models::selector::Target) -> Result<()> {
        Err(self.refuse())
    }
}

pub struct UnsupportedCapture {
    info: BackendInfo,
}

impl UnsupportedCapture {
    #[must_use]
    pub const fn new(info: BackendInfo) -> Self {
        Self { info }
    }

    fn refuse(&self, capability: Capability) -> DesktopError {
        DesktopError::unsupported(capability, Backend::None, &self.info)
    }
}

impl CapturePort for UnsupportedCapture {
    fn capture(&self, target: &CaptureTarget) -> Result<Image> {
        Err(self.refuse(match target {
            CaptureTarget::Screen => Capability::Screenshots,
            CaptureTarget::Window(_) | CaptureTarget::App(_) => Capability::WindowScreenshots,
        }))
    }
}

pub struct UnsupportedInput {
    info: BackendInfo,
}

impl UnsupportedInput {
    #[must_use]
    pub const fn new(info: BackendInfo) -> Self {
        Self { info }
    }

    fn refuse(&self, capability: Capability) -> DesktopError {
        DesktopError::unsupported(capability, Backend::None, &self.info)
    }
}

impl InputPort for UnsupportedInput {
    fn move_mouse(&self, _point: Point, _space: &CoordinateSpace) -> Result<()> {
        Err(self.refuse(Capability::Mouse))
    }

    fn click(
        &self,
        _point: Point,
        _space: &CoordinateSpace,
        _button: MouseButton,
        _count: u8,
    ) -> Result<()> {
        Err(self.refuse(Capability::Mouse))
    }

    fn type_text(&self, _text: &str) -> Result<()> {
        Err(self.refuse(Capability::Keyboard))
    }

    fn key(&self, _chord: &Chord) -> Result<()> {
        Err(self.refuse(Capability::Keyboard))
    }

    fn scroll(&self, _delta: ScrollDelta, _space: &CoordinateSpace) -> Result<()> {
        Err(self.refuse(Capability::Scroll))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_core::models::backend::{DesktopEnvironment, DisplayServer, Platform};

    fn kde_wayland() -> BackendInfo {
        BackendInfo {
            platform: Platform::Linux,
            display_server: DisplayServer::Wayland,
            desktop_environment: DesktopEnvironment::Kde,
            accessibility: Backend::AtSpi,
            windows: Backend::AtSpi,
            screenshot: Backend::None,
            input: Backend::None,
        }
    }

    #[test]
    fn refusals_name_the_capability_and_the_environment_that_lacks_it() {
        let input = UnsupportedInput::new(kde_wayland());
        let error = input
            .click(
                Point::new(0, 0),
                &CoordinateSpace::primary_screen(),
                MouseButton::Left,
                1,
            )
            .expect_err("must refuse");

        let json = serde_json::to_value(&error).expect("serializes");
        assert_eq!(json["error"], "unsupported_capability");
        assert_eq!(json["capability"], "mouse");
        assert_eq!(json["display_server"], "wayland");
        assert_eq!(json["desktop_environment"], "kde");
    }

    #[test]
    fn screen_and_window_capture_refusals_are_distinguishable() {
        let capture = UnsupportedCapture::new(kde_wayland());
        let screen = capture
            .capture(&CaptureTarget::Screen)
            .expect_err("must refuse");
        let window = capture
            .capture(&CaptureTarget::Window(
                desktop_core::models::ids::WindowId::new(1),
            ))
            .expect_err("must refuse");

        assert_eq!(
            serde_json::to_value(&screen).expect("serializes")["capability"],
            "screenshots"
        );
        assert_eq!(
            serde_json::to_value(&window).expect("serializes")["capability"],
            "window_screenshots"
        );
    }

    #[test]
    fn every_input_operation_refuses_rather_than_silently_doing_nothing() {
        let input = UnsupportedInput::new(kde_wayland());
        assert!(input.type_text("x").is_err());
        assert!(input.key(&Chord::parse("ctrl+c").expect("parses")).is_err());
        assert!(
            input
                .scroll(
                    ScrollDelta::new(0, -100),
                    &CoordinateSpace::primary_screen()
                )
                .is_err()
        );
        assert!(
            input
                .move_mouse(Point::new(1, 1), &CoordinateSpace::primary_screen())
                .is_err()
        );
    }
}
