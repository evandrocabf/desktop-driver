//! Applications and windows.

use serde::{Deserialize, Serialize};

use crate::models::{
    geometry::Bounds,
    ids::{ProcessId, WindowId},
};

/// Stable-enough identity for an application across two CLI invocations.
///
/// macOS has a genuinely stable bundle identifier; Linux does not, so `pid` and
/// `name` carry the weight there. All three are compared when re-resolving, and
/// a pid that has been recycled onto a different program is caught by the name.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, schemars::JsonSchema)]
pub struct AppKey {
    pub pid: ProcessId,
    pub name: String,
    /// macOS bundle identifier, or the AT-SPI unique bus name on Linux.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
}

impl AppKey {
    #[must_use]
    pub fn new(pid: ProcessId, name: &str) -> Self {
        Self {
            pid,
            name: name.to_owned(),
            identifier: None,
        }
    }

    #[must_use]
    pub fn with_identifier(mut self, identifier: &str) -> Self {
        self.identifier = Some(identifier.to_owned());
        self
    }

    /// Case-insensitive match against whatever the user typed for `--app`.
    /// Accepts the name, the identifier, or the pid as a decimal string.
    #[must_use]
    pub fn matches(&self, needle: &str) -> bool {
        let needle = needle.trim();
        if needle.eq_ignore_ascii_case(&self.name) {
            return true;
        }
        if self
            .identifier
            .as_deref()
            .is_some_and(|id| id.eq_ignore_ascii_case(needle))
        {
            return true;
        }
        needle.parse::<i32>().ok() == Some(self.pid.get())
    }
}

/// A running application.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Application {
    pub pid: ProcessId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// `true` when the platform reports this application as frontmost.
    pub active: bool,
    /// Number of top-level windows the adapter could see. On Wayland this
    /// counts AT-SPI frames, which excludes applications with no accessibility
    /// support at all.
    pub window_count: u32,
}

impl Application {
    #[must_use]
    pub fn key(&self) -> AppKey {
        AppKey {
            pid: self.pid,
            name: self.name.clone(),
            identifier: self.identifier.clone(),
        }
    }
}

/// Identity for a window that survives a process boundary.
///
/// Deliberately not the platform handle: an `XID` is stable but a Wayland
/// window has no id at all, so the title plus its ordinal among the app's
/// windows is the only thing both platforms can supply.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, schemars::JsonSchema)]
pub struct WindowKey {
    pub title: Option<String>,
    pub index: u16,
}

impl WindowKey {
    #[must_use]
    pub fn new(title: Option<&str>, index: u16) -> Self {
        Self {
            title: title.map(str::to_owned),
            index,
        }
    }
}

/// A top-level window.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Window {
    pub id: WindowId,
    pub title: Option<String>,
    pub app: AppKey,
    /// Absent under Wayland, where a client cannot learn its own screen
    /// position. Absent bounds are reported as `null` rather than as zeros,
    /// so an agent can tell "at the origin" from "unknowable".
    pub bounds: Option<Bounds>,
    pub focused: bool,
    pub minimized: bool,
    /// Ordinal among the owning application's windows, used by [`WindowKey`].
    pub index: u16,
}

impl Window {
    #[must_use]
    pub fn key(&self) -> WindowKey {
        WindowKey::new(self.title.as_deref(), self.index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> AppKey {
        AppKey::new(ProcessId::new(4242), "Visual Studio Code")
            .with_identifier("com.microsoft.VSCode")
    }

    #[test]
    fn app_matching_accepts_name_identifier_or_pid() {
        let k = key();
        assert!(k.matches("Visual Studio Code"));
        assert!(k.matches("visual studio code"));
        assert!(k.matches("com.microsoft.VSCode"));
        assert!(k.matches("4242"));
        assert!(!k.matches("Firefox"));
        assert!(!k.matches("4243"));
    }

    #[test]
    fn app_matching_ignores_surrounding_whitespace_from_shell_quoting() {
        assert!(key().matches("  Visual Studio Code  "));
    }

    #[test]
    fn window_bounds_are_null_not_zero_when_the_platform_cannot_report_them() {
        // Under Wayland this is the honest answer; zeros would read as a real
        // position at the screen origin.
        let window = Window {
            id: WindowId::new(3),
            title: Some("main.rs".to_owned()),
            app: key(),
            bounds: None,
            focused: true,
            minimized: false,
            index: 0,
        };
        let value = serde_json::to_value(&window).expect("serializes");
        assert!(value["bounds"].is_null());
    }
}
