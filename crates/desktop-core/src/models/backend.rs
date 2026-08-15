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
    /// A window manager is running and publishes `_NET_CLIENT_LIST_STACKING`.
    ///
    /// Distinct from `x11_display`, and the distinction is load-bearing: a bare
    /// `Xvfb` with no window manager answers every X11 call and reports no
    /// windows whatsoever, which would read as "window listing works, and you
    /// have none open" rather than as "nothing here manages windows".
    pub ewmh: bool,
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

/// Whether this build drives a desktop at all.
///
/// KDE is refused outright. What its own compositor offers is shut to an
/// ordinary program: `org.kde.KWin.ScreenShot2` answers `NoAuthorized` unless
/// the caller was launched from a desktop entry declaring
/// `X-KDE-DBUS-Restricted-Interfaces`, which a command-line tool is not, and
/// KWin implements none of the `ext-*` capture protocols that would offer a way
/// round it. Driving it would mean leaning entirely on portal behaviour this
/// build has never been run against, on a desktop whose own interfaces have
/// already been closed — so it says no, once, where the alternative is a
/// scattering of half-working commands.
///
/// The refusal is about the *host* desktop. `desktop session` starts an X11
/// display of its own, with its own window manager, and nothing about the
/// user's desktop reaches inside it — which is why an agent session is still
/// selected normally on a KDE machine.
#[must_use]
pub const fn is_supported(desktop: DesktopEnvironment) -> bool {
    !matches!(desktop, DesktopEnvironment::Kde)
}

/// Chooses the four Linux mechanisms.
///
/// An unsupported desktop is refused before anything is probed — see
/// [`is_supported`].
///
/// Every other choice is made from what the session *advertises*, never from
/// its name. An earlier version gated the portals on GNOME, which left every
/// other compositor refusing mechanisms its own desktop implements — the
/// freedesktop portal interfaces are not GNOME's, and a probe already
/// establishes whether each one is present. What a supported desktop's name
/// still decides is the wording of a capability note: GNOME's portal backend is
/// the one this build has been verified against, and the rest say so.
///
/// Input requires *two* portals rather than one. Absolute pointer positioning
/// interprets its coordinates in a PipeWire stream's logical space, so the
/// RemoteDesktop session has to carry a ScreenCast source as well; without one
/// the only thing left is relative motion from wherever the pointer happens to
/// be, which is not a position anybody knows. `xdg-desktop-portal-wlr` offers
/// ScreenCast alone, and this is what makes that come out as "no input" rather
/// than as a session that fails on its first click.
///
/// The window *list* comes from EWMH wherever a window manager publishes
/// `_NET_CLIENT_LIST_STACKING`, and falls back to AT-SPI frames where none
/// does. That is the difference between a list carrying stacking order, screen
/// geometry and every managed window, and one that silently omits any
/// application without accessibility support.
#[must_use]
pub fn select_linux_backends(
    display_server: DisplayServer,
    desktop: DesktopEnvironment,
    facts: SessionFacts,
) -> BackendInfo {
    if !is_supported(desktop) {
        return BackendInfo {
            platform: Platform::Linux,
            display_server,
            desktop_environment: desktop,
            accessibility: Backend::None,
            windows: Backend::None,
            screenshot: Backend::None,
            input: Backend::None,
        };
    }

    let accessibility = if facts.a11y_bus {
        Backend::AtSpi
    } else {
        Backend::None
    };

    let frames = if facts.a11y_bus {
        Backend::AtSpi
    } else {
        Backend::None
    };

    let (windows, screenshot, input) = match display_server {
        DisplayServer::X11 => (
            if facts.ewmh { Backend::Ewmh } else { frames },
            Backend::X11,
            Backend::X11,
        ),
        DisplayServer::Wayland => {
            let screenshot = if facts.screenshot_portal {
                Backend::XdgDesktopPortal
            } else {
                Backend::None
            };
            let input = if facts.remote_desktop_portal && facts.screencast_portal {
                Backend::RemoteDesktopPortal
            } else {
                Backend::None
            };
            (frames, screenshot, input)
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

    /// `x11_display` is set because XWayland is running, which is the trap this
    /// module exists to avoid. `ewmh` is false because nothing on that X
    /// display manages windows; the compositor does.
    fn gnome_wayland() -> SessionFacts {
        SessionFacts {
            wayland_display: true,
            x11_display: true,
            session_type_wayland: true,
            session_type_x11: false,
            a11y_bus: true,
            atspi_enabled: true,
            ewmh: false,
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
            ewmh: true,
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
        assert_eq!(info.screenshot, Backend::XdgDesktopPortal);
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

    /// EWMH enumerates and positions windows where a window manager publishes
    /// the properties; AT-SPI frames are the fallback. Naming either where the
    /// other did the work would credit a mechanism that generated none of the
    /// output.
    #[test]
    fn the_window_list_is_credited_to_whichever_mechanism_produced_it() {
        let x11 = select_linux_backends(DisplayServer::X11, DesktopEnvironment::Xfce, plain_x11());
        assert_eq!(x11.windows, Backend::Ewmh);

        let wayland = select_linux_backends(
            DisplayServer::Wayland,
            DesktopEnvironment::Gnome,
            gnome_wayland(),
        );
        assert_eq!(wayland.windows, Backend::AtSpi);
    }

    /// A bare Xvfb answers every X11 call and reports no windows at all.
    /// Selecting EWMH there would turn "nothing manages windows here" into "you
    /// have no windows open".
    #[test]
    fn an_x11_display_with_no_window_manager_falls_back_to_at_spi_frames() {
        let facts = SessionFacts {
            ewmh: false,
            ..plain_x11()
        };
        let info = select_linux_backends(DisplayServer::X11, DesktopEnvironment::Unknown, facts);
        assert_eq!(info.windows, Backend::AtSpi);
    }

    /// A supported desktop's portal backend implements the same freedesktop
    /// interfaces GNOME's does. Refusing them on the strength of
    /// `XDG_CURRENT_DESKTOP` withheld a mechanism the machine was running.
    #[test]
    fn a_desktop_is_given_the_portals_it_advertises_rather_than_the_ones_its_name_implies() {
        for desktop in [
            DesktopEnvironment::Sway,
            DesktopEnvironment::Hyprland,
            DesktopEnvironment::Unknown,
        ] {
            let info = select_linux_backends(DisplayServer::Wayland, desktop, gnome_wayland());
            assert_eq!(info.screenshot, Backend::XdgDesktopPortal, "{desktop:?}");
            assert_eq!(info.input, Backend::RemoteDesktopPortal, "{desktop:?}");
        }
    }

    #[test]
    fn a_wayland_session_with_no_portal_at_all_still_refuses_both() {
        let facts = SessionFacts {
            screencast_portal: false,
            remote_desktop_portal: false,
            screenshot_portal: false,
            ..gnome_wayland()
        };
        let info =
            select_linux_backends(DisplayServer::Wayland, DesktopEnvironment::Wlroots, facts);
        assert_eq!(info.accessibility, Backend::AtSpi, "the tree is unaffected");
        assert_eq!(info.screenshot, Backend::None);
        assert_eq!(info.input, Backend::None);
    }

    /// `xdg-desktop-portal-wlr` offers ScreenCast and no RemoteDesktop, so
    /// there is nothing to send a key through.
    #[test]
    fn screencast_alone_is_not_enough_for_input() {
        let facts = SessionFacts {
            remote_desktop_portal: false,
            ..gnome_wayland()
        };
        let info =
            select_linux_backends(DisplayServer::Wayland, DesktopEnvironment::Wlroots, facts);
        assert_eq!(info.screenshot, Backend::XdgDesktopPortal);
        assert_eq!(info.input, Backend::None);
    }

    /// Absolute pointer motion is interpreted in a ScreenCast stream's space; a
    /// RemoteDesktop session without one can only move the pointer relative to
    /// wherever it already is.
    #[test]
    fn remote_desktop_alone_is_not_enough_for_input_either() {
        let facts = SessionFacts {
            screencast_portal: false,
            ..gnome_wayland()
        };
        let info = select_linux_backends(DisplayServer::Wayland, DesktopEnvironment::Sway, facts);
        assert_eq!(info.input, Backend::None);
    }

    /// KDE selects nothing at all, including the accessibility tree that AT-SPI
    /// would have answered perfectly well. Half-supporting a desktop whose own
    /// interfaces are closed is how a tool ends up with a scattering of
    /// commands that work and no way to tell which.
    #[test]
    fn kde_selects_no_backend_whatever_its_session_advertises() {
        let info = select_linux_backends(
            DisplayServer::Wayland,
            DesktopEnvironment::Kde,
            gnome_wayland(),
        );
        assert_eq!(info.accessibility, Backend::None);
        assert_eq!(info.windows, Backend::None);
        assert_eq!(info.screenshot, Backend::None);
        assert_eq!(info.input, Backend::None);
        assert_eq!(
            info.desktop_environment,
            DesktopEnvironment::Kde,
            "the desktop is still named, so the refusal can explain itself"
        );
    }

    /// The refusal follows the desktop, not the display server: a KDE session
    /// on X11 is as closed as one on Wayland.
    #[test]
    fn kde_is_refused_under_x11_too() {
        let info = select_linux_backends(DisplayServer::X11, DesktopEnvironment::Kde, plain_x11());
        assert_eq!(info.accessibility, Backend::None);
        assert_eq!(info.screenshot, Backend::None);
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

    /// The capture path asks Screenshot for a file. A session advertising
    /// ScreenCast but not Screenshot has nothing this build can call, and
    /// saying otherwise would promise pixels it cannot fetch.
    #[test]
    fn capture_follows_the_screenshot_portal_because_that_is_what_it_calls() {
        let facts = SessionFacts {
            screenshot_portal: false,
            ..gnome_wayland()
        };
        let info = select_linux_backends(DisplayServer::Wayland, DesktopEnvironment::Gnome, facts);
        assert_eq!(info.screenshot, Backend::None);
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
        assert_eq!(json["screenshot"], "xdg-desktop-portal");
        assert_eq!(json["input"], "remote-desktop-portal");
    }
}
