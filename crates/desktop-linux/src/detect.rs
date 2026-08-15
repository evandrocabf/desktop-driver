//! Working out what kind of Linux session this is.
//!
//! Gathering facts and deciding from them are kept apart: the facts come from
//! the environment and D-Bus here, and the decision lives in
//! `desktop_core::models::backend`, where it is unit-tested against synthetic
//! sessions on any machine.

use std::{env, time::Duration};

use atspi::zbus;
use desktop_core::models::backend::{
    BackendInfo, DesktopEnvironment, SessionFacts, select_display_server, select_linux_backends,
};

/// D-Bus names probed to decide whether the portals are usable at all.
const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const A11Y_BUS: &str = "org.a11y.Bus";

/// Portal calls are cheap but a hung service must not hang the CLI.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1_500);

/// Everything the backend selector needs, read from the live session.
#[must_use]
pub fn session_facts() -> SessionFacts {
    let session_type = env::var("XDG_SESSION_TYPE").unwrap_or_default();
    SessionFacts {
        wayland_display: env::var_os("WAYLAND_DISPLAY").is_some(),
        x11_display: env::var_os("DISPLAY").is_some(),
        session_type_wayland: session_type.eq_ignore_ascii_case("wayland"),
        session_type_x11: session_type.eq_ignore_ascii_case("x11"),
        a11y_bus: probe_a11y_bus(),
        atspi_enabled: atspi_enabled(),
        ewmh: probe_ewmh(),
        screencast_portal: probe_portal_interface("ScreenCast"),
        remote_desktop_portal: probe_portal_interface("RemoteDesktop"),
        screenshot_portal: probe_portal_interface("Screenshot"),
    }
}

/// The desktop environment, from `XDG_CURRENT_DESKTOP` where it is set.
///
/// Falls back to compositor-specific variables, since some set only their own.
#[must_use]
pub fn desktop_environment() -> DesktopEnvironment {
    if let Ok(value) = env::var("XDG_CURRENT_DESKTOP") {
        let parsed = DesktopEnvironment::parse(&value);
        if parsed != DesktopEnvironment::Unknown {
            return parsed;
        }
    }
    if env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return DesktopEnvironment::Hyprland;
    }
    if env::var_os("SWAYSOCK").is_some() {
        return DesktopEnvironment::Sway;
    }
    if let Ok(value) = env::var("XDG_SESSION_DESKTOP") {
        return DesktopEnvironment::parse(&value);
    }
    DesktopEnvironment::Unknown
}

/// The full picture, ready for `desktop info`.
#[must_use]
pub fn detect() -> BackendInfo {
    let facts = session_facts();
    select_linux_backends(select_display_server(facts), desktop_environment(), facts)
}

/// The facts about an agent session, which are known rather than probed.
///
/// A session is X11 by construction: this crate started the X server itself, so
/// there is no display server to guess at and no XWayland trap to avoid. The
/// portals are deliberately absent — they belong to the user's login session
/// and would capture the user's screen, which is the opposite of the point.
///
/// The window manager is the one thing still probed rather than assumed: a
/// session starts `openbox`, and a session whose `openbox` failed to start
/// would otherwise report window listing as working and then list nothing.
#[must_use]
pub fn session_facts_for(session: &desktop_core::agent::AgentSession) -> SessionFacts {
    SessionFacts {
        wayland_display: false,
        x11_display: true,
        session_type_wayland: false,
        session_type_x11: true,
        a11y_bus: a11y_bus_reachable(&session.a11y_address),
        atspi_enabled: true,
        ewmh: crate::x11::supports_ewmh(&crate::x11::DisplayTarget {
            display: Some(session.display.clone()),
            cookie: session.cookie_bytes().ok(),
        }),
        screencast_portal: false,
        remote_desktop_portal: false,
        screenshot_portal: false,
    }
}

/// The backends selected for an agent session.
///
/// The desktop environment is reported as unknown rather than as openbox: it is
/// not one of the environments any decision here turns on, because under X11
/// every backend is selected natively regardless.
#[must_use]
pub fn detect_for(session: &desktop_core::agent::AgentSession) -> BackendInfo {
    let facts = session_facts_for(session);
    select_linux_backends(
        select_display_server(facts),
        DesktopEnvironment::Unknown,
        facts,
    )
}

/// Whether a window manager is running on the X display and publishes a window
/// list.
///
/// Only consulted for X11 sessions. It is probed unconditionally anyway,
/// because a Wayland session has `DISPLAY` set too and the answer there —
/// mutter does manage XWayland's root window — is a fact about XWayland rather
/// than about the session, which selection ignores by never asking under
/// Wayland.
fn probe_ewmh() -> bool {
    env::var_os("DISPLAY").is_some()
        && crate::x11::supports_ewmh(&crate::x11::DisplayTarget::host())
}

/// Whether a named accessibility bus answers.
fn a11y_bus_reachable(address: &str) -> bool {
    crate::runtime::block_on(async {
        let Ok(address) = address.parse::<zbus::Address>() else {
            return false;
        };
        let Ok(builder) = zbus::connection::Builder::address(address) else {
            return false;
        };
        let Ok(Ok(connection)) = tokio::time::timeout(PROBE_TIMEOUT, builder.build()).await else {
            return false;
        };
        let Ok(proxy) = zbus::fdo::DBusProxy::new(&connection).await else {
            return false;
        };
        let Ok(name) = "org.a11y.atspi.Registry".try_into() else {
            return false;
        };
        matches!(
            tokio::time::timeout(PROBE_TIMEOUT, proxy.name_has_owner(name)).await,
            Ok(Ok(true))
        )
    })
}

/// Whether the accessibility bus can be reached.
///
/// `org.a11y.Bus` is D-Bus-activated, so this succeeds even when
/// `toolkit-accessibility` is off — which is correct, because GTK4 apps expose
/// their trees regardless of that setting.
fn probe_a11y_bus() -> bool {
    crate::runtime::block_on(async {
        let Ok(connection) = zbus::Connection::session().await else {
            return false;
        };
        let Ok(proxy) = zbus::fdo::DBusProxy::new(&connection).await else {
            return false;
        };
        matches!(
            tokio::time::timeout(
                PROBE_TIMEOUT,
                proxy.name_has_owner(A11Y_BUS.try_into().expect("A11Y_BUS is a valid bus name"),),
            )
            .await,
            Ok(Ok(true))
        )
    })
}

/// Whether `org.a11y.Status.IsEnabled` is set.
///
/// This is the switch lazy toolkits watch. Firefox and Chromium build no
/// accessibility tree at all until something sets it, which presents as an
/// application that has a window but no contents.
#[must_use]
pub fn atspi_enabled() -> bool {
    crate::runtime::block_on(atspi::connection::read_session_accessibility()).unwrap_or(false)
}

/// Turns accessibility on for this session.
///
/// Only `IsEnabled` is written. `ScreenReaderEnabled` is deliberately left
/// alone: on GNOME, `gsd-a11y-settings` mirrors it into
/// `org.gnome.desktop.a11y.applications.screen-reader-enabled`, which launches
/// Orca and starts reading the screen aloud.
pub fn enable_atspi() -> desktop_core::errors::Result<()> {
    crate::runtime::try_block_on(atspi::connection::set_session_accessibility(true))?.map_err(
        |error| {
            desktop_core::errors::DesktopError::backend(format!(
                "cannot enable accessibility for this session: {error}"
            ))
        },
    )
}

/// Whether a portal interface is present, read from its `version` property.
///
/// Checking the property rather than just the bus name matters: the portal
/// service is always running under a desktop that ships it, but an individual
/// interface may be missing when its backend is not installed.
fn probe_portal_interface(interface: &str) -> bool {
    let interface = format!("org.freedesktop.portal.{interface}");
    crate::runtime::block_on(async move {
        let Ok(connection) = zbus::Connection::session().await else {
            return false;
        };
        let query = async {
            let proxy = zbus::Proxy::new(&connection, PORTAL_BUS, PORTAL_PATH, interface.as_str())
                .await
                .ok()?;
            proxy.get_property::<u32>("version").await.ok()
        };
        matches!(
            tokio::time::timeout(PROBE_TIMEOUT, query).await,
            Ok(Some(version)) if version > 0
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_core::models::backend::DisplayServer;

    #[test]
    fn detection_reports_a_coherent_picture_of_whatever_session_runs_the_tests() {
        // Deliberately not asserting a specific desktop: this must pass in CI
        // containers, on X11 and under Wayland alike.
        let info = detect();
        assert_eq!(
            info.platform,
            desktop_core::models::backend::Platform::Linux
        );

        // The rule that matters: a Wayland session never selects X11 backends,
        // even though XWayland leaves DISPLAY set.
        if info.display_server == DisplayServer::Wayland {
            use desktop_core::models::backend::Backend;
            assert_ne!(info.screenshot, Backend::X11);
            assert_ne!(info.input, Backend::X11);
            assert_ne!(info.windows, Backend::Ewmh);
        }
    }

    #[test]
    fn a_headless_session_selects_no_display_backends() {
        let facts = SessionFacts {
            a11y_bus: true,
            ..SessionFacts::default()
        };
        assert_eq!(select_display_server(facts), DisplayServer::Headless);
    }
}
