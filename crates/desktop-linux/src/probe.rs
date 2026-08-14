//! Reporting what this Linux session can do, and why it cannot do the rest.

use desktop_core::{
    errors::{Permission, PermissionState},
    models::{
        backend::{Backend, BackendInfo, DisplayServer, SessionFacts},
        capability::{Capability, CapabilitySet, CapabilityState, UnsupportedReason},
    },
    ports::{Diagnostic, PlatformProbe},
};

pub struct LinuxProbe {
    info: BackendInfo,
    facts: SessionFacts,
}

impl LinuxProbe {
    #[must_use]
    pub const fn new(info: BackendInfo, facts: SessionFacts) -> Self {
        Self { info, facts }
    }
}

impl PlatformProbe for LinuxProbe {
    fn info(&self) -> BackendInfo {
        self.info.clone()
    }

    fn capabilities(&self) -> CapabilitySet {
        capabilities_for(&self.info)
    }

    /// The grants this session needs but may not hold.
    ///
    /// Linux has no equivalent of macOS TCC. What it has instead is the portal
    /// grant, which is per-session rather than per-application.
    fn permissions(&self) -> Vec<PermissionState> {
        if self.info.display_server != DisplayServer::Wayland {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.info.input == Backend::RemoteDesktopPortal {
            out.push(PermissionState {
                permission: Permission::RemoteDesktopPortal,
                granted: crate::portal::has_stored_token(),
                remedy: Some(
                    "run `desktop setup` once and approve the dialog; the grant is then \
                     remembered via a portal restore token"
                        .to_owned(),
                ),
            });
        }
        out
    }

    fn diagnostics(&self) -> Vec<Diagnostic> {
        diagnostics_for(&self.info, self.facts)
    }

    fn dependencies(&self) -> Vec<desktop_core::models::dependency::SystemDependency> {
        crate::dependencies::dependencies(&self.info)
    }

    fn install_command(&self) -> Option<String> {
        crate::dependencies::install_command(&self.info)
    }

    /// Performs the one-time grant, rather than only reporting on it.
    ///
    /// Turns on the switch lazy toolkits watch: without it Firefox and Chromium
    /// report a window with an empty tree, which reads as a broken tool rather
    /// than a session setting.
    ///
    /// Then opens the portal session, because opening it *is* the request — the
    /// dialog appears during `Start`, and the restore token handed back is what
    /// makes every later run dialog-free. Reporting the state without opening
    /// one would leave `desktop setup` describing work it never did.
    fn request_permissions(&self) -> Vec<PermissionState> {
        let _ = crate::detect::enable_atspi();

        if self.info.input == Backend::RemoteDesktopPortal {
            let tokens = crate::portal::TokenStore::at_default_path();
            let _ = crate::runtime::try_block_on(crate::wayland::PortalSession::open(&tokens));
        }
        self.permissions()
    }
}

/// Derives the capability set from the selected backends.
///
/// Written as a pure function of [`BackendInfo`] so the whole table can be
/// checked without a desktop present.
///
/// Several entries encode findings rather than lookups.
///
/// **Windows** is degraded wherever the list comes from AT-SPI. Whether a
/// position comes back is the toolkit's choice, not the display server's:
/// Firefox and VTE report real screen coordinates under X11, GTK4 reports
/// (0,0) for every node on X11 and Wayland alike, and under Wayland nothing can
/// report a true one. Elements whose position was never measured carry
/// `bounds: null` rather than a fabricated origin.
///
/// **Focus** is supported under X11 via `_NET_ACTIVE_WINDOW`, whose result is
/// read back from the root window afterwards, so a window manager that refuses
/// is reported as a failure rather than as success. Under Wayland it is
/// unsupported outright: verified on GNOME 49, AT-SPI `GrabFocus` returns
/// success and changes nothing, and there is no client-initiated raise, so this
/// is a no-op rather than a best effort.
///
/// **Screenshots** stop warning about the approval dialog once the desktop has
/// recorded the grant, since saying so then would be stale advice.
///
/// **Window screenshots** through the ScreenCast portal are degraded because it
/// shows a picker and offers no way to name the window programmatically: it
/// works, but a human chooses.
#[must_use]
pub fn capabilities_for(info: &BackendInfo) -> CapabilitySet {
    let mut set = CapabilitySet::new();

    let a11y_present = info.accessibility != Backend::None;
    set.set(
        Capability::Accessibility,
        if a11y_present {
            CapabilityState::Supported
        } else {
            CapabilityState::unsupported(UnsupportedReason::ServiceUnavailable {
                service: "org.a11y.Bus".to_owned(),
            })
        },
    );
    for capability in [Capability::ElementActions, Capability::ElementText] {
        set.set(
            capability,
            if a11y_present {
                CapabilityState::Supported
            } else {
                CapabilityState::unsupported(UnsupportedReason::ServiceUnavailable {
                    service: "org.a11y.Bus".to_owned(),
                })
            },
        );
    }

    set.set(
        Capability::Windows,
        match info.windows {
            Backend::Ewmh => CapabilityState::Supported,
            Backend::AtSpi => CapabilityState::degraded(
                "window list comes from AT-SPI frames: no stacking order, applications \
                 without accessibility support are invisible, and some toolkits report \
                 no position at all — check `bounds` for null before using coordinates",
            ),
            _ => CapabilityState::unsupported(UnsupportedReason::NoBackendMechanism),
        },
    );

    set.set(
        Capability::Focus,
        match info.display_server {
            DisplayServer::X11 => CapabilityState::Supported,
            _ => CapabilityState::unsupported(UnsupportedReason::NoBackendMechanism),
        },
    );

    set.set(
        Capability::Screenshots,
        match info.screenshot {
            Backend::X11 => CapabilityState::Supported,
            Backend::PortalScreenCast | Backend::XdgDesktopPortal => {
                if crate::portal::screenshot_permission_granted() {
                    CapabilityState::Supported
                } else {
                    CapabilityState::degraded(
                        "the first capture needs a one-time approval dialog; \
                         run `desktop setup` to get it over with",
                    )
                }
            }
            _ => CapabilityState::unsupported(UnsupportedReason::NoBackendMechanism),
        },
    );

    set.set(
        Capability::WindowScreenshots,
        match info.screenshot {
            Backend::X11 => CapabilityState::Supported,
            Backend::PortalScreenCast => CapabilityState::degraded(
                "the portal shows a window picker; the specific window cannot be chosen \
                 programmatically, and the choice is remembered by restore token",
            ),
            Backend::XdgDesktopPortal => {
                CapabilityState::unsupported(UnsupportedReason::NoBackendMechanism)
            }
            _ => CapabilityState::unsupported(UnsupportedReason::NoBackendMechanism),
        },
    );

    set.set(Capability::AgentSession, {
        let missing = crate::session::missing_requirements();
        if missing.is_empty() {
            CapabilityState::Supported
        } else {
            CapabilityState::unsupported(UnsupportedReason::ServiceUnavailable {
                service: missing.join(", "),
            })
        }
    });

    let input_state = match info.input {
        Backend::X11 => CapabilityState::Supported,
        Backend::RemoteDesktopPortal => CapabilityState::degraded(
            "input goes through the RemoteDesktop portal; the first use needs approval",
        ),
        _ => CapabilityState::unsupported(UnsupportedReason::NoBackendMechanism),
    };
    for capability in [Capability::Mouse, Capability::Keyboard, Capability::Scroll] {
        set.set(capability, input_state.clone());
    }

    set
}

/// The advice `desktop doctor` prints, most severe first.
///
/// Where a portal interface is advertised but not selected, the remedy says so.
/// "There is no mechanism" and "the mechanism is there and this build has not
/// been verified against it" call for different next steps, and collapsing them
/// into one message sends a KDE user looking for a protocol that already exists
/// on their machine.
#[must_use]
pub fn diagnostics_for(info: &BackendInfo, facts: SessionFacts) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    if info.accessibility == Backend::None {
        out.push(Diagnostic::error(
            "The accessibility bus is not reachable, so no UI tree can be read.",
            "Install at-spi2-core and ensure `org.a11y.Bus` is running on the session bus.",
        ));
    }

    if info.display_server == DisplayServer::Wayland {
        out.push(Diagnostic::info(
            "Under Wayland, element bounds are window-relative: absolute screen \
             coordinates are not available to any client, by design.",
        ));

        if info.input == Backend::None {
            let remedy = if facts.remote_desktop_portal {
                format!(
                    "Your desktop does advertise org.freedesktop.portal.RemoteDesktop, so                      the mechanism exists — this build only selects it on GNOME, where it                      has been verified. Driving {} through it is untested rather than                      impossible; an agent display (`desktop session`) works here today and                      does not depend on the portal at all.",
                    info.desktop_environment.as_str()
                )
            } else {
                "This build drives input only through the RemoteDesktop portal, and your                  session does not provide it. KDE and wlroots have working mechanisms                  upstream that desktop-driver does not implement yet. `desktop session`                  gives the agent an X11 display where input works regardless."
                    .to_owned()
            };
            out.push(Diagnostic::error(
                format!(
                    "No input backend for {} Wayland: mouse and keyboard are unavailable.",
                    info.desktop_environment.as_str()
                ),
                remedy,
            ));
        }

        if info.screenshot == Backend::None && facts.screenshot_portal {
            out.push(Diagnostic::warning(
                format!(
                    "Screen capture is unavailable on {} Wayland, though your desktop does                      advertise the Screenshot portal.",
                    info.desktop_environment.as_str()
                ),
                "This build selects the portal only on GNOME, where it has been verified.                  `desktop session` gives the agent an X11 display that captures natively.",
            ));
        }

        if facts.x11_display {
            out.push(Diagnostic::warning(
                "XWayland is running, so X11 tools appear to work but see only XWayland \
                 clients — an empty window list rather than an error.",
                "desktop-driver deliberately ignores X11 in a Wayland session. Pass \
                 `--backend x11` only if you specifically want XWayland clients.",
            ));
        }
    }

    if facts.a11y_bus && !facts.atspi_enabled {
        out.push(Diagnostic::warning(
            "Session accessibility (org.a11y.Status.IsEnabled) is off. Firefox, Chromium \
             and Qt applications build no tree until it is on, so they appear as a window \
             with no contents.",
            "Run `desktop setup`, which sets IsEnabled for this session. It does NOT touch \
             ScreenReaderEnabled — on GNOME that would launch Orca and start reading the \
             screen aloud.",
        ));
    }

    out.push(Diagnostic::info(
        "Electron applications need their own switch even once the session flag is on: \
         launch them with --force-renderer-accessibility, or set ACCESSIBILITY_ENABLED=1.",
    ));

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_core::models::backend::{DesktopEnvironment, Platform};

    fn info(
        display_server: DisplayServer,
        desktop: DesktopEnvironment,
        windows: Backend,
        screenshot: Backend,
        input: Backend,
    ) -> BackendInfo {
        BackendInfo {
            platform: Platform::Linux,
            display_server,
            desktop_environment: desktop,
            accessibility: Backend::AtSpi,
            windows,
            screenshot,
            input,
        }
    }

    fn x11() -> BackendInfo {
        info(
            DisplayServer::X11,
            DesktopEnvironment::Gnome,
            Backend::Ewmh,
            Backend::X11,
            Backend::X11,
        )
    }

    fn gnome_wayland() -> BackendInfo {
        info(
            DisplayServer::Wayland,
            DesktopEnvironment::Gnome,
            Backend::AtSpi,
            Backend::PortalScreenCast,
            Backend::RemoteDesktopPortal,
        )
    }

    #[test]
    fn focus_is_reported_unavailable_on_wayland_rather_than_merely_degraded() {
        // GrabFocus there returns success and does nothing; calling that
        // "degraded" invites an agent to rely on it and send every following
        // keystroke to the wrong window.
        let caps = capabilities_for(&gnome_wayland());
        assert!(!caps.is_available(Capability::Focus));
    }

    #[test]
    fn focus_remains_supported_on_x11() {
        assert_eq!(
            capabilities_for(&x11()).get(Capability::Focus),
            CapabilityState::Supported
        );
    }

    fn kde_wayland() -> BackendInfo {
        info(
            DisplayServer::Wayland,
            DesktopEnvironment::Kde,
            Backend::AtSpi,
            Backend::None,
            Backend::None,
        )
    }

    #[test]
    fn x11_supports_everything_without_caveats() {
        let caps = capabilities_for(&x11());
        for capability in Capability::ALL {
            assert_eq!(
                caps.get(capability),
                CapabilityState::Supported,
                "{capability:?} should be plainly supported on X11"
            );
        }
    }

    #[test]
    fn gnome_wayland_window_capture_is_degraded_rather_than_supported() {
        // The portal picker means a human chooses the window on first use.
        // Calling that "supported" would mislead an agent into a hang.
        let caps = capabilities_for(&gnome_wayland());
        assert!(matches!(
            caps.get(Capability::WindowScreenshots),
            CapabilityState::Degraded { .. }
        ));
        assert!(caps.is_available(Capability::WindowScreenshots));
    }

    #[test]
    fn gnome_wayland_window_listing_is_degraded_because_at_spi_cannot_see_everything() {
        let caps = capabilities_for(&gnome_wayland());
        match caps.get(Capability::Windows) {
            CapabilityState::Degraded { note } => {
                assert!(note.contains("stacking order"), "got {note}");
            }
            other => panic!("expected degraded, got {other:?}"),
        }
    }

    #[test]
    fn kde_wayland_refuses_input_and_capture_but_keeps_accessibility() {
        let caps = capabilities_for(&kde_wayland());
        assert!(caps.is_available(Capability::Accessibility));
        assert!(caps.is_available(Capability::ElementActions));
        assert!(!caps.is_available(Capability::Mouse));
        assert!(!caps.is_available(Capability::Keyboard));
        assert!(!caps.is_available(Capability::Screenshots));
    }

    #[test]
    fn a_missing_a11y_bus_is_reported_as_a_service_problem_not_a_missing_feature() {
        let mut broken = x11();
        broken.accessibility = Backend::None;
        let caps = capabilities_for(&broken);
        assert_eq!(
            caps.get(Capability::Accessibility),
            CapabilityState::unsupported(UnsupportedReason::ServiceUnavailable {
                service: "org.a11y.Bus".to_owned()
            })
        );
    }

    #[test]
    fn the_xwayland_trap_is_called_out_explicitly_when_it_applies() {
        let facts = SessionFacts {
            x11_display: true,
            a11y_bus: true,
            ..SessionFacts::default()
        };
        let notes = diagnostics_for(&gnome_wayland(), facts);
        assert!(
            notes.iter().any(|d| d.summary.contains("XWayland")),
            "expected an XWayland warning in {notes:?}"
        );
    }

    #[test]
    fn doctor_never_advises_the_setting_that_makes_orca_start_talking() {
        // Writing org.a11y.Status.ScreenReaderEnabled launches Orca on GNOME.
        // Suggesting it would be actively harmful advice.
        let facts = SessionFacts::default();
        for info in [x11(), gnome_wayland(), kde_wayland()] {
            for diagnostic in diagnostics_for(&info, facts) {
                let text = format!(
                    "{} {}",
                    diagnostic.summary,
                    diagnostic.remedy.unwrap_or_default()
                );
                assert!(
                    !text.contains("ScreenReaderEnabled true")
                        && !text.contains("ScreenReaderEnabled=true"),
                    "doctor must not recommend enabling the screen reader: {text}"
                );
            }
        }
    }

    #[test]
    fn a_missing_wayland_input_backend_is_reported_as_an_error_with_an_explanation() {
        let notes = diagnostics_for(&kde_wayland(), SessionFacts::default());
        let found = notes
            .iter()
            .find(|d| d.summary.contains("No input backend"))
            .expect("expected an input diagnostic");
        assert_eq!(found.severity, desktop_core::ports::Severity::Error);
        assert!(found.remedy.is_some());
    }
}
