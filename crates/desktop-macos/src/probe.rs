//! Reporting macOS capabilities and permissions.

use desktop_core::{
    errors::{Permission, PermissionState},
    models::{
        backend::{Backend, BackendInfo, DesktopEnvironment, DisplayServer, Platform},
        capability::{Capability, CapabilitySet, CapabilityState, UnsupportedReason},
    },
    ports::{Diagnostic, PlatformProbe},
};

pub struct MacosProbe {
    info: BackendInfo,
}

impl MacosProbe {
    #[must_use]
    pub fn new() -> Self {
        Self { info: info() }
    }
}

impl Default for MacosProbe {
    fn default() -> Self {
        Self::new()
    }
}

#[must_use]
pub fn info() -> BackendInfo {
    BackendInfo {
        platform: Platform::Macos,
        display_server: DisplayServer::Quartz,
        desktop_environment: DesktopEnvironment::Aqua,
        accessibility: Backend::AxUiElement,
        windows: Backend::CoreGraphics,
        screenshot: Backend::ScreenCaptureKit,
        input: Backend::CoreGraphics,
    }
}

impl PlatformProbe for MacosProbe {
    fn info(&self) -> BackendInfo {
        self.info.clone()
    }

    fn capabilities(&self) -> CapabilitySet {
        capabilities()
    }

    fn permissions(&self) -> Vec<PermissionState> {
        let accessibility = crate::ax::is_trusted();
        let screen_recording = has_screen_recording();
        let post_events = has_post_event_access();
        vec![
            PermissionState {
                permission: Permission::Accessibility,
                granted: accessibility,
                remedy: (!accessibility).then(accessibility_remedy),
            },
            PermissionState {
                permission: Permission::ScreenRecording,
                granted: screen_recording,
                remedy: (!screen_recording).then(screen_recording_remedy),
            },
            PermissionState {
                permission: Permission::PostEvents,
                granted: post_events,
                remedy: (!post_events).then(post_event_remedy),
            },
        ]
    }

    fn diagnostics(&self) -> Vec<Diagnostic> {
        diagnostics()
    }

    /// Shows the Accessibility prompt if it has never been shown.
    ///
    /// macOS displays it once per TCC entry and silently declines afterwards,
    /// so the remedy text remains the real fallback.
    fn request_permissions(&self) -> Vec<PermissionState> {
        let _ = crate::ax::is_trusted_with_prompt(true);
        self.permissions()
    }
}

/// macOS supports every capability, subject to three permissions.
///
/// Posting synthetic events is a *separate* grant from reading the
/// accessibility tree, and macOS answers it with its own preflight. Without
/// checking it, a process trusted for reading reports mouse and keyboard as
/// supported and then posts events that go nowhere — no error, no movement.
#[must_use]
pub fn capabilities() -> CapabilitySet {
    let mut set = CapabilitySet::new();

    let accessibility_state = if crate::ax::is_trusted() {
        CapabilityState::Supported
    } else {
        CapabilityState::unsupported(UnsupportedReason::PermissionMissing {
            permission: "accessibility".to_owned(),
        })
    };
    for capability in [
        Capability::Accessibility,
        Capability::ElementActions,
        Capability::ElementText,
        Capability::Windows,
        Capability::Focus,
    ] {
        set.set(capability, accessibility_state.clone());
    }

    let capture_state = if has_screen_recording() {
        CapabilityState::Supported
    } else {
        CapabilityState::unsupported(UnsupportedReason::PermissionMissing {
            permission: "screen_recording".to_owned(),
        })
    };
    set.set(Capability::Screenshots, capture_state.clone());
    set.set(Capability::WindowScreenshots, capture_state);

    let post_state = if accessibility_state.is_available() && has_post_event_access() {
        CapabilityState::Supported
    } else if accessibility_state.is_available() {
        CapabilityState::unsupported(UnsupportedReason::PermissionMissing {
            permission: "post_events".to_owned(),
        })
    } else {
        accessibility_state.clone()
    };
    for capability in [Capability::Mouse, Capability::Keyboard, Capability::Scroll] {
        set.set(capability, post_state.clone());
    }

    set
}

/// Whether Screen Recording has been granted.
#[must_use]
pub fn has_screen_recording() -> bool {
    objc2_core_graphics::CGPreflightScreenCaptureAccess()
}

/// What to do when synthetic events are being discarded.
#[must_use]
pub fn post_event_remedy() -> String {
    format!(
        "Permission to post keyboard and mouse events is required.\n\n\
         System Settings →\n\
         Privacy & Security →\n\
         Accessibility\n\n\
         Grant it to the application that launched `desktop` — currently {}. \
         This is a different grant from reading the accessibility tree: with \
         only the first, every snapshot works and every click is discarded \
         without an error. Some macOS versions list it under Input Monitoring \
         instead.",
        launching_application()
    )
}

/// Whether this process may post synthetic events.
///
/// Distinct from [`AXIsProcessTrusted`](crate::ax::is_trusted): reading the
/// tree and driving the pointer are different grants, and a process can hold
/// the first without the second. That combination is the quiet one — every
/// read works, every click does nothing.
#[must_use]
pub fn has_post_event_access() -> bool {
    objc2_core_graphics::CGPreflightPostEventAccess()
}

/// The Accessibility remedy text.
///
/// It names the *launching application* rather than `desktop`, because TCC
/// attributes a CLI's requests to whatever started it. Telling someone to grant
/// access to "desktop" sends them looking for an entry that will never appear.
#[must_use]
pub fn accessibility_remedy() -> String {
    format!(
        "Accessibility permission is required.\n\n\
         System Settings →\n\
         Privacy & Security →\n\
         Accessibility\n\n\
         Grant it to the application that launched `desktop` — currently {}. \
         A command-line tool inherits its terminal's permission rather than \
         having one of its own.",
        launching_application()
    )
}

/// The Screen Recording remedy text.
#[must_use]
pub fn screen_recording_remedy() -> String {
    format!(
        "Screen Recording permission is required.\n\n\
         System Settings →\n\
         Privacy & Security →\n\
         Screen & System Audio Recording\n\n\
         Grant it to the application that launched `desktop` — currently {}. \
         macOS re-checks this grant periodically, so a previously working \
         setup can start failing without any change on your part.",
        launching_application()
    )
}

/// A best-effort name for whatever launched this process.
///
/// `TERM_PROGRAM` is set by every mainstream terminal; when it is absent the
/// message stays honest rather than guessing.
fn launching_application() -> String {
    std::env::var("TERM_PROGRAM")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "your terminal application".to_owned())
}

#[must_use]
pub fn diagnostics() -> Vec<Diagnostic> {
    let mut out = Vec::new();

    if !crate::ax::is_trusted() {
        out.push(Diagnostic::error(
            "Accessibility permission is not granted, so no UI tree can be read.",
            accessibility_remedy(),
        ));
    }
    if !has_screen_recording() {
        out.push(Diagnostic::warning(
            "Screen Recording permission is not granted. Screenshots will fail, and \
             window titles from the window list come back empty.",
            screen_recording_remedy(),
        ));
    }

    out.push(Diagnostic::info(
        "TCC identifies a binary by its code signature. An unsigned or ad-hoc-signed \
         build gets a new identity on every `cargo build`, which silently drops the \
         grant — sign with a stable identity, or run the same installed binary.",
    ));

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_accessibility_remedy_names_the_launching_app_not_the_binary() {
        // Telling someone to grant access to "desktop" sends them looking for
        // an entry that will never appear in System Settings.
        let text = accessibility_remedy();
        assert!(text.contains("System Settings"), "got {text}");
        assert!(text.contains("Accessibility"), "got {text}");
        assert!(
            text.contains("launched `desktop`"),
            "the remedy must explain the inheritance: {text}"
        );
    }

    #[test]
    fn the_screen_recording_remedy_uses_the_current_settings_pane_name() {
        // macOS 26 renamed the pane; the old name sends users to the wrong place.
        let text = screen_recording_remedy();
        assert!(
            text.contains("Screen & System Audio Recording"),
            "got {text}"
        );
        assert!(text.contains("periodically"), "got {text}");
    }

    #[test]
    fn macos_reports_quartz_rather_than_borrowing_a_linux_display_server_name() {
        let info = info();
        assert_eq!(info.platform, Platform::Macos);
        assert_eq!(info.display_server, DisplayServer::Quartz);
        assert_eq!(info.accessibility, Backend::AxUiElement);
        assert_eq!(info.screenshot, Backend::ScreenCaptureKit);
    }

    #[test]
    fn info_serializes_to_the_documented_shape() {
        let json = serde_json::to_value(info()).expect("serializes");
        assert_eq!(json["platform"], "macos");
        assert_eq!(json["display_server"], "quartz");
        assert_eq!(json["screenshot"], "screen-capture-kit");
    }
}
