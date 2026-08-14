//! Structured, machine-branchable errors.
//!
//! Every failure is a variant an agent can match on, carrying the fields it
//! would need to decide what to do next. "Something went wrong" is not an
//! outcome this crate can produce.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::models::{
    backend::{Backend, DesktopEnvironment, DisplayServer, Platform},
    capability::Capability,
    ids::ElementId,
    path::StaleReason,
};

/// An OS-level permission the tool needs and does not have.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Accessibility,
    ScreenRecording,
    /// Posting synthetic pointer and keyboard events. Separate from
    /// [`Accessibility`](Self::Accessibility): a process can be trusted to read
    /// the tree and still have every event it posts discarded.
    PostEvents,
    /// The user has not yet granted the portal session that Wayland input needs.
    RemoteDesktopPortal,
    ScreenCastPortal,
}

impl Permission {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accessibility => "accessibility",
            Self::ScreenRecording => "screen_recording",
            Self::PostEvents => "post_events",
            Self::RemoteDesktopPortal => "remote_desktop_portal",
            Self::ScreenCastPortal => "screencast_portal",
        }
    }
}

/// Whether a permission is currently held.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct PermissionState {
    pub permission: Permission,
    pub granted: bool,
    /// What the user must do, when it is not granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

/// The semantic exit code family. Callers branch on these rather than parsing
/// output, so they are part of the public interface and must not be renumbered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitCategory {
    Success,
    SetupOrConfigurationFailure,
    PolicyDenied,
    InteractionRequired,
    BackendFailure,
    TargetFailure,
    Timeout,
    InternalInvariantFailure,
}

impl ExitCategory {
    #[must_use]
    pub const fn status(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::SetupOrConfigurationFailure => 2,
            Self::PolicyDenied => 3,
            Self::InteractionRequired => 4,
            Self::BackendFailure => 5,
            Self::TargetFailure => 6,
            Self::Timeout => 7,
            Self::InternalInvariantFailure => 70,
        }
    }

    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::SetupOrConfigurationFailure => "setup_or_configuration_failure",
            Self::PolicyDenied => "policy_denied",
            Self::InteractionRequired => "interaction_required",
            Self::BackendFailure => "backend_failure",
            Self::TargetFailure => "target_failure",
            Self::Timeout => "timeout",
            Self::InternalInvariantFailure => "internal_invariant_failure",
        }
    }
}

pub type Result<T> = std::result::Result<T, DesktopError>;

#[derive(Clone, Debug, Error, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum DesktopError {
    #[error("{} permission is required", permission.as_str())]
    PermissionRequired {
        permission: Permission,
        platform: Platform,
        /// Human-readable instructions, including which application the grant
        /// must be given to — on macOS that is the launching terminal, not
        /// this binary, which is the single most common confusion.
        remedy: String,
    },

    #[error("capability {} is not supported by the {} backend", capability.as_str(), backend.as_str())]
    UnsupportedCapability {
        capability: Capability,
        backend: Backend,
        platform: Platform,
        display_server: DisplayServer,
        desktop_environment: DesktopEnvironment,
    },

    #[error("no snapshot has been taken in this session")]
    NoSnapshot,

    #[error("element {element} no longer matches the tree it was recorded from")]
    ElementStale {
        element: ElementId,
        #[serde(flatten)]
        reason: StaleReason,
    },

    #[error("no element matched {selector}")]
    ElementNotFound { selector: String },

    #[error("{matches} elements matched {selector}; refine it or use --element")]
    AmbiguousSelector {
        selector: String,
        matches: usize,
        candidates: Vec<ElementId>,
    },

    #[error("{target} was not found")]
    TargetNotFound { target: String },

    #[error("timed out after {waited_ms}ms waiting for {condition}")]
    Timeout { waited_ms: u64, condition: String },

    #[error("policy denied {action} on {subject}")]
    PolicyDenied { action: String, subject: String },

    #[error("the {} backend is unavailable: {reason}", backend.as_str())]
    BackendUnavailable { backend: Backend, reason: String },

    #[error("this command needs a one-time interactive grant; run `desktop setup` first")]
    SetupRequired { permission: Permission },

    #[error("coordinates need a window under {}: pass --window, or use --element", display_server.as_str())]
    CoordinatesRequireWindow { display_server: DisplayServer },

    #[error("invalid argument: {message}")]
    InvalidArgument { message: String },

    #[error("{message}")]
    Backend { message: String },

    #[error("internal invariant violated: {message}")]
    Internal { message: String },
}

impl DesktopError {
    #[must_use]
    pub const fn exit_category(&self) -> ExitCategory {
        match self {
            Self::PermissionRequired { .. } | Self::SetupRequired { .. } => {
                ExitCategory::InteractionRequired
            }
            Self::UnsupportedCapability { .. } | Self::BackendUnavailable { .. } => {
                ExitCategory::SetupOrConfigurationFailure
            }
            Self::PolicyDenied { .. } => ExitCategory::PolicyDenied,
            Self::Timeout { .. } => ExitCategory::Timeout,
            Self::NoSnapshot
            | Self::ElementStale { .. }
            | Self::ElementNotFound { .. }
            | Self::AmbiguousSelector { .. }
            | Self::TargetNotFound { .. }
            | Self::CoordinatesRequireWindow { .. }
            | Self::InvalidArgument { .. } => ExitCategory::TargetFailure,
            Self::Backend { .. } => ExitCategory::BackendFailure,
            Self::Internal { .. } => ExitCategory::InternalInvariantFailure,
        }
    }

    /// Constructs the unsupported-capability error from an environment
    /// description, so every backend reports the same shape.
    #[must_use]
    pub const fn unsupported(
        capability: Capability,
        backend: Backend,
        info: &crate::models::backend::BackendInfo,
    ) -> Self {
        Self::UnsupportedCapability {
            capability,
            backend,
            platform: info.platform,
            display_server: info.display_server,
            desktop_environment: info.desktop_environment,
        }
    }

    #[must_use]
    pub fn backend(message: impl Into<String>) -> Self {
        Self::Backend {
            message: message.into(),
        }
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    #[must_use]
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::backend::BackendInfo;

    fn gnome_wayland() -> BackendInfo {
        BackendInfo {
            platform: Platform::Linux,
            display_server: DisplayServer::Wayland,
            desktop_environment: DesktopEnvironment::Unknown,
            accessibility: Backend::AtSpi,
            windows: Backend::AtSpi,
            screenshot: Backend::None,
            input: Backend::None,
        }
    }

    #[test]
    fn permission_errors_match_the_documented_json_shape() {
        let error = DesktopError::PermissionRequired {
            permission: Permission::ScreenRecording,
            platform: Platform::Macos,
            remedy: "System Settings → Privacy & Security → Screen Recording".to_owned(),
        };
        let json = serde_json::to_value(&error).expect("serializes");
        assert_eq!(json["error"], "permission_required");
        assert_eq!(json["permission"], "screen_recording");
        assert_eq!(json["platform"], "macos");
    }

    #[test]
    fn unsupported_capability_errors_match_the_documented_json_shape() {
        let error =
            DesktopError::unsupported(Capability::Screenshots, Backend::None, &gnome_wayland());
        let json = serde_json::to_value(&error).expect("serializes");
        assert_eq!(json["error"], "unsupported_capability");
        assert_eq!(json["capability"], "screenshots");
        assert_eq!(json["platform"], "linux");
        assert_eq!(json["display_server"], "wayland");
        assert_eq!(json["desktop_environment"], "unknown");
    }

    #[test]
    fn a_stale_element_error_carries_the_specific_reason_it_went_stale() {
        let error = DesktopError::ElementStale {
            element: ElementId::new(42),
            reason: StaleReason::PathTruncated { depth: 2 },
        };
        let json = serde_json::to_value(&error).expect("serializes");
        assert_eq!(json["error"], "element_stale");
        assert_eq!(json["element"], 42);
        assert_eq!(json["reason"], "path_truncated");
        assert_eq!(json["depth"], 2);
    }

    #[test]
    fn every_error_maps_to_a_distinct_and_stable_exit_status() {
        assert_eq!(ExitCategory::Success.status(), 0);
        assert_eq!(ExitCategory::SetupOrConfigurationFailure.status(), 2);
        assert_eq!(ExitCategory::PolicyDenied.status(), 3);
        assert_eq!(ExitCategory::InteractionRequired.status(), 4);
        assert_eq!(ExitCategory::BackendFailure.status(), 5);
        assert_eq!(ExitCategory::TargetFailure.status(), 6);
        assert_eq!(ExitCategory::Timeout.status(), 7);
        assert_eq!(ExitCategory::InternalInvariantFailure.status(), 70);
    }

    #[test]
    fn permission_and_setup_failures_are_interaction_required_so_agents_can_prompt() {
        assert_eq!(
            DesktopError::SetupRequired {
                permission: Permission::RemoteDesktopPortal
            }
            .exit_category(),
            ExitCategory::InteractionRequired
        );
        assert_eq!(
            DesktopError::PermissionRequired {
                permission: Permission::Accessibility,
                platform: Platform::Macos,
                remedy: String::new(),
            }
            .exit_category(),
            ExitCategory::InteractionRequired
        );
    }

    #[test]
    fn unsupported_capability_is_a_configuration_failure_not_a_crash() {
        assert_eq!(
            DesktopError::unsupported(Capability::Mouse, Backend::None, &gnome_wayland())
                .exit_category(),
            ExitCategory::SetupOrConfigurationFailure
        );
    }

    #[test]
    fn error_display_text_names_the_thing_that_failed() {
        let error = DesktopError::unsupported(Capability::Mouse, Backend::None, &gnome_wayland());
        let text = error.to_string();
        assert!(text.contains("mouse"), "got {text}");
        assert!(text.contains("none"), "got {text}");
    }

    #[test]
    fn ambiguous_selector_lists_the_candidates_so_the_caller_can_choose() {
        let error = DesktopError::AmbiguousSelector {
            selector: "role=button".to_owned(),
            matches: 3,
            candidates: vec![ElementId::new(4), ElementId::new(9), ElementId::new(12)],
        };
        let json = serde_json::to_value(&error).expect("serializes");
        assert_eq!(json["error"], "ambiguous_selector");
        assert_eq!(json["matches"], 3);
        assert_eq!(json["candidates"][0], 4);
    }

    #[test]
    fn coordinates_without_a_window_under_wayland_is_its_own_error() {
        // Absolute screen coordinates do not exist there; the caller needs a
        // different instruction, not a generic failure.
        let error = DesktopError::CoordinatesRequireWindow {
            display_server: DisplayServer::Wayland,
        };
        let json = serde_json::to_value(&error).expect("serializes");
        assert_eq!(json["error"], "coordinates_require_window");
        assert_eq!(json["display_server"], "wayland");
    }
}
