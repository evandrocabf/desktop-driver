//! Environment description and backend selection.
//!
//! Selection is a pure function of observable facts about the session, so the
//! whole decision table is unit-testable on any machine — including the rule
//! that matters most: **never pick X11 backends inside a Wayland session**.
//! XWayland answers X11 calls perfectly well and reports an empty window list,
//! which is the worst possible failure mode: confident and wrong.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Macos,
    Linux,
}

impl Platform {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Linux => "linux",
        }
    }

    #[must_use]
    pub const fn current() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::Macos)
        } else if cfg!(target_os = "linux") {
            Some(Self::Linux)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DisplayServer {
    Quartz,
    X11,
    Wayland,
    /// No graphical session was detected at all.
    Headless,
}

impl DisplayServer {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Quartz => "quartz",
            Self::X11 => "x11",
            Self::Wayland => "wayland",
            Self::Headless => "headless",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DesktopEnvironment {
    Aqua,
    Gnome,
    Kde,
    Sway,
    Hyprland,
    Wlroots,
    Xfce,
    Cinnamon,
    Unknown,
}

impl DesktopEnvironment {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Aqua => "aqua",
            Self::Gnome => "gnome",
            Self::Kde => "kde",
            Self::Sway => "sway",
            Self::Hyprland => "hyprland",
            Self::Wlroots => "wlroots",
            Self::Xfce => "xfce",
            Self::Cinnamon => "cinnamon",
            Self::Unknown => "unknown",
        }
    }

    /// Parses `XDG_CURRENT_DESKTOP`, which is a colon-separated list of names
    /// in priority order (e.g. `ubuntu:GNOME`).
    #[must_use]
    pub fn parse(value: &str) -> Self {
        for token in value.split(':') {
            let token = token.trim().to_ascii_lowercase();
            let matched = match token.as_str() {
                "gnome" | "gnome-classic" | "gnome-flashback" | "unity" | "pantheon" => {
                    Some(Self::Gnome)
                }
                "kde" | "plasma" => Some(Self::Kde),
                "sway" => Some(Self::Sway),
                "hyprland" => Some(Self::Hyprland),
                "wlroots" | "river" | "niri" | "labwc" | "wayfire" => Some(Self::Wlroots),
                "xfce" => Some(Self::Xfce),
                "x-cinnamon" | "cinnamon" => Some(Self::Cinnamon),
                _ => None,
            };
            if let Some(de) = matched {
                return de;
            }
        }
        Self::Unknown
    }
}

/// The concrete mechanism chosen for one concern.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    None,

    AtSpi,
    AxUiElement,

    X11,
    XdgDesktopPortal,
    #[serde(rename = "portal-screencast")]
    PortalScreenCast,
    RemoteDesktopPortal,
    ScreenCaptureKit,
    CoreGraphics,
    Ewmh,
}

impl Backend {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AtSpi => "at-spi",
            Self::AxUiElement => "ax-ui-element",
            Self::X11 => "x11",
            Self::XdgDesktopPortal => "xdg-desktop-portal",
            Self::PortalScreenCast => "portal-screencast",
            Self::RemoteDesktopPortal => "remote-desktop-portal",
            Self::ScreenCaptureKit => "screen-capture-kit",
            Self::CoreGraphics => "core-graphics",
            Self::Ewmh => "ewmh",
        }
    }
}

/// Observable facts about the session, gathered by the platform adapter.
/// Kept separate from the decision so the decision stays pure.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionFacts {
    pub wayland_display: bool,
    pub x11_display: bool,
    pub session_type_wayland: bool,
    pub session_type_x11: bool,
    pub a11y_bus: bool,
    /// `org.a11y.Status.IsEnabled`. Distinct from `a11y_bus`: the bus can be
    /// reachable while this is off, in which case lazy toolkits (Firefox,
    /// Chromium, Qt) expose a window with an empty tree.
    pub atspi_enabled: bool,
    pub screencast_portal: bool,
    pub remote_desktop_portal: bool,
    pub screenshot_portal: bool,
}

/// The environment and the mechanisms selected for it. This is what
/// `desktop info --json` prints.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct BackendInfo {
    pub platform: Platform,
    pub display_server: DisplayServer,
    pub desktop_environment: DesktopEnvironment,
    pub accessibility: Backend,
    pub windows: Backend,
    pub screenshot: Backend,
    pub input: Backend,
}

/// Chooses the display server from environment facts.
///
/// `XDG_SESSION_TYPE` is authoritative when present because `DISPLAY` is
/// *also* set inside a Wayland session (XWayland), and treating that as X11 is
/// the trap this function exists to avoid.
#[must_use]
pub fn select_display_server(facts: SessionFacts) -> DisplayServer {
    if facts.session_type_wayland || (facts.wayland_display && !facts.session_type_x11) {
        DisplayServer::Wayland
    } else if facts.session_type_x11 || facts.x11_display {
        DisplayServer::X11
    } else {
        DisplayServer::Headless
    }
}

/// Chooses the four Linux mechanisms.
///
/// KDE and wlroots deliberately land on [`Backend::None`] for capture and
/// input: the mechanisms exist upstream but are not implemented here, and a
/// wrong-but-plausible fallback is worse than an honest refusal.
///
/// The window *list* is attributed to AT-SPI on every Linux session, X11
/// included: EWMH raises a window, it does not enumerate one, so naming it here
/// would credit a mechanism that produced none of the output.
#[must_use]
pub fn select_linux_backends(
    display_server: DisplayServer,
    desktop: DesktopEnvironment,
    facts: SessionFacts,
) -> BackendInfo {
    let accessibility = if facts.a11y_bus {
        Backend::AtSpi
    } else {
        Backend::None
    };

    let windows = if facts.a11y_bus {
        Backend::AtSpi
    } else {
        Backend::None
    };

    let (windows, screenshot, input) = match display_server {
        DisplayServer::X11 => (windows, Backend::X11, Backend::X11),
        DisplayServer::Wayland => {
            let screenshot = match desktop {
                DesktopEnvironment::Gnome if facts.screencast_portal => Backend::PortalScreenCast,
                DesktopEnvironment::Gnome if facts.screenshot_portal => Backend::XdgDesktopPortal,
                _ => Backend::None,
            };
            let input = match desktop {
                DesktopEnvironment::Gnome if facts.remote_desktop_portal => {
                    Backend::RemoteDesktopPortal
                }
                _ => Backend::None,
            };
            (windows, screenshot, input)
        }
        DisplayServer::Headless | DisplayServer::Quartz => {
            (Backend::None, Backend::None, Backend::None)
        }
    };

    BackendInfo {
        platform: Platform::Linux,
        display_server,
        desktop_environment: desktop,
        accessibility,
        windows,
        screenshot,
        input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gnome_wayland() -> SessionFacts {
        SessionFacts {
            wayland_display: true,
            // XWayland is running, so DISPLAY is set too. This is the trap.
            x11_display: true,
            session_type_wayland: true,
            session_type_x11: false,
            a11y_bus: true,
            atspi_enabled: true,
            screencast_portal: true,
            remote_desktop_portal: true,
            screenshot_portal: true,
        }
    }

    fn plain_x11() -> SessionFacts {
        SessionFacts {
            wayland_display: false,
            x11_display: true,
            session_type_wayland: false,
            session_type_x11: true,
            a11y_bus: true,
            atspi_enabled: true,
            screencast_portal: false,
            remote_desktop_portal: false,
            screenshot_portal: false,
        }
    }

    #[test]
    fn a_wayland_session_is_never_mistaken_for_x11_just_because_display_is_set() {
        // Probed on this machine: DISPLAY=:0 with XWayland running, yet
        // _NET_CLIENT_LIST is empty. Selecting X11 here yields confident
        // wrong answers.
        assert_eq!(
            select_display_server(gnome_wayland()),
            DisplayServer::Wayland
        );
    }

    #[test]
    fn a_real_x11_session_selects_x11() {
        assert_eq!(select_display_server(plain_x11()), DisplayServer::X11);
    }

    #[test]
    fn a_session_with_no_display_at_all_is_headless() {
        assert_eq!(
            select_display_server(SessionFacts::default()),
            DisplayServer::Headless
        );
    }

    #[test]
    fn an_explicit_x11_session_type_wins_over_a_stray_wayland_display() {
        let facts = SessionFacts {
            wayland_display: true,
            session_type_x11: true,
            session_type_wayland: false,
            ..SessionFacts::default()
        };
        assert_eq!(select_display_server(facts), DisplayServer::X11);
    }

    #[test]
    fn gnome_wayland_selects_portals_for_capture_and_input_and_at_spi_for_windows() {
        let info = select_linux_backends(
            DisplayServer::Wayland,
            DesktopEnvironment::Gnome,
            gnome_wayland(),
        );
        assert_eq!(info.accessibility, Backend::AtSpi);
        assert_eq!(info.windows, Backend::AtSpi);
        assert_eq!(info.screenshot, Backend::PortalScreenCast);
        assert_eq!(info.input, Backend::RemoteDesktopPortal);
    }

    #[test]
    fn x11_selects_native_mechanisms_for_capture_and_input() {
        let info =
            select_linux_backends(DisplayServer::X11, DesktopEnvironment::Gnome, plain_x11());
        assert_eq!(info.accessibility, Backend::AtSpi);
        assert_eq!(info.screenshot, Backend::X11);
        assert_eq!(info.input, Backend::X11);
    }

    #[test]
    fn the_window_list_is_attributed_to_at_spi_on_every_display_server() {
        // EWMH raises windows; it is not what produces the list. Naming it
        // here would credit a mechanism that generated none of the output.
        for (server, facts) in [
            (DisplayServer::X11, plain_x11()),
            (DisplayServer::Wayland, gnome_wayland()),
        ] {
            let info = select_linux_backends(server, DesktopEnvironment::Gnome, facts);
            assert_eq!(info.windows, Backend::AtSpi, "{server:?}");
        }
    }

    #[test]
    fn kde_wayland_gets_accessibility_but_refuses_capture_and_input() {
        // AT-SPI is display-server independent, so reading the tree still
        // works. Capture and input are not implemented and must say so.
        let info = select_linux_backends(
            DisplayServer::Wayland,
            DesktopEnvironment::Kde,
            gnome_wayland(),
        );
        assert_eq!(info.accessibility, Backend::AtSpi);
        assert_eq!(info.screenshot, Backend::None);
        assert_eq!(info.input, Backend::None);
    }

    #[test]
    fn an_unknown_wayland_compositor_refuses_capture_and_input() {
        let info = select_linux_backends(
            DisplayServer::Wayland,
            DesktopEnvironment::Unknown,
            gnome_wayland(),
        );
        assert_eq!(info.screenshot, Backend::None);
        assert_eq!(info.input, Backend::None);
    }

    #[test]
    fn a_missing_a11y_bus_disables_accessibility_everywhere() {
        let facts = SessionFacts {
            a11y_bus: false,
            ..gnome_wayland()
        };
        let info = select_linux_backends(DisplayServer::Wayland, DesktopEnvironment::Gnome, facts);
        assert_eq!(info.accessibility, Backend::None);
        assert_eq!(info.windows, Backend::None);
    }

    #[test]
    fn gnome_falls_back_to_the_screenshot_portal_when_screencast_is_absent() {
        let facts = SessionFacts {
            screencast_portal: false,
            ..gnome_wayland()
        };
        let info = select_linux_backends(DisplayServer::Wayland, DesktopEnvironment::Gnome, facts);
        assert_eq!(info.screenshot, Backend::XdgDesktopPortal);
    }

    #[test]
    fn desktop_environment_parses_the_colon_separated_xdg_list() {
        assert_eq!(
            DesktopEnvironment::parse("GNOME"),
            DesktopEnvironment::Gnome
        );
        assert_eq!(
            DesktopEnvironment::parse("ubuntu:GNOME"),
            DesktopEnvironment::Gnome
        );
        assert_eq!(DesktopEnvironment::parse("KDE"), DesktopEnvironment::Kde);
        assert_eq!(
            DesktopEnvironment::parse("Hyprland"),
            DesktopEnvironment::Hyprland
        );
        assert_eq!(
            DesktopEnvironment::parse("something-else"),
            DesktopEnvironment::Unknown
        );
        assert_eq!(DesktopEnvironment::parse(""), DesktopEnvironment::Unknown);
    }

    #[test]
    fn backend_info_serializes_to_the_documented_info_json_shape() {
        let info = select_linux_backends(
            DisplayServer::Wayland,
            DesktopEnvironment::Gnome,
            gnome_wayland(),
        );
        let json = serde_json::to_value(&info).expect("serializes");
        assert_eq!(json["platform"], "linux");
        assert_eq!(json["display_server"], "wayland");
        assert_eq!(json["desktop_environment"], "gnome");
        assert_eq!(json["accessibility"], "at-spi");
        assert_eq!(json["screenshot"], "portal-screencast");
        assert_eq!(json["input"], "remote-desktop-portal");
    }
}
