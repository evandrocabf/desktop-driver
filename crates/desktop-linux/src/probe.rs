//! Reporting what this Linux session can do, and why it cannot do the rest.

use desktop_core::{
    errors::{Permission, PermissionState},
    models::{
        backend::{Backend, BackendInfo, DesktopEnvironment, DisplayServer, SessionFacts},
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
                granted: crate::portal::has_input_token(),
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
/// degraded: there is no client-initiated raise, and AT-SPI `GrabFocus` returns
/// success while changing nothing — verified on GNOME 49 — so what is attempted
/// instead is asking the application to present itself over D-Bus, which
/// reaches only applications exporting `org.freedesktop.Application` and which
/// the compositor may answer by highlighting the window rather than raising it.
/// The result is checked either way, so the failure is reported rather than
/// assumed away.
///
/// **Screenshots and input** stop warning about the approval dialog once the
/// desktop has recorded the grant, since saying so then would be stale advice.
/// Away from GNOME they carry the caveat that the portal backend is untested
/// here, which no grant changes.
///
/// **Window screenshots** are unavailable under Wayland. The Screenshot portal
/// has no window target that any backend implements, and the ScreenCast route
/// needs a human to pick the window in a dialog. This reported "degraded" on
/// the strength of that dialog while the capture path refused every window
/// outright — a caveat describing a route the code never took.
#[must_use]
pub fn capabilities_for(info: &BackendInfo) -> CapabilitySet {
    capabilities_from(
        info,
        &crate::session::missing_requirements(),
        Grants::on_this_host(),
    )
}

/// The caveat a portal-backed capability carries away from GNOME.
///
/// The interfaces are freedesktop's and every backend implements the same ones,
/// which is why they are selected wherever they are advertised rather than on
/// GNOME alone. What is *not* the same everywhere is how each backend behaves
/// in practice, and only GNOME's has been run against. Saying so is the
/// difference between an untested path and an unclaimed one.
fn unverified_portal_backend(info: &BackendInfo) -> Option<String> {
    match info.desktop_environment {
        DesktopEnvironment::Gnome => None,
        other => Some(format!(
            "this build has been verified against GNOME's portal backend and not {}'s, so \
             treat it as untested here; `desktop session` needs no portal at all",
            other.as_str()
        )),
    }
}

/// The one-time approvals this session has already been given.
///
/// A grant is the difference between "this works" and "this works, after a
/// dialog someone has to answer" — which is the difference between an agent
/// proceeding and an agent hanging in front of a prompt nobody is watching.
/// Both are read from the host, so they arrive as data for the same reason the
/// session helpers do.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Grants {
    pub screenshot: bool,
    pub input: bool,
}

impl Grants {
    /// What this machine has actually been granted.
    #[must_use]
    pub fn on_this_host() -> Self {
        Self {
            screenshot: crate::portal::screenshot_permission_granted(),
            input: crate::portal::has_input_token(),
        }
    }
}

/// A portal-backed capability, given whether its grant has been made.
///
/// Once the desktop has recorded the approval there is no dialog left to warn
/// about, and saying otherwise is stale advice that reads as "this will
/// interrupt you" every time. What survives the grant is the caveat about which
/// portal backend this has been run against, because that does not change by
/// being approved.
fn portal_state(approval: &str, granted: bool, info: &BackendInfo) -> CapabilityState {
    match (granted, unverified_portal_backend(info)) {
        (true, None) => CapabilityState::Supported,
        (true, Some(caveat)) => CapabilityState::degraded(&caveat),
        (false, None) => CapabilityState::degraded(approval),
        (false, Some(caveat)) => CapabilityState::degraded(&format!("{approval}. {caveat}")),
    }
}

/// The same mapping, with the two inputs that do not come from `info`.
///
/// Every capability above is a function of which backends were selected. Agent
/// sessions and portal grants are the exceptions: the first needs `Xvfb`, a
/// window manager and a D-Bus daemon *installed on the host*, and the second is
/// a permission recorded by the desktop. Neither is described by any
/// `BackendInfo`.
///
/// Both were read from the host inside the mapping, which made the whole
/// function depend on the machine it ran on — so the table-driven tests below
/// passed on a developer's desktop and failed on a bare runner with none of
/// those packages, which is the honest answer for that machine and the wrong
/// question for a unit test.
///
/// Taking them as arguments keeps the mapping pure and leaves
/// [`capabilities_for`] as the one place that looks at the host.
#[must_use]
pub fn capabilities_from(
    info: &BackendInfo,
    missing_session_helpers: &[&str],
    grants: Grants,
) -> CapabilitySet {
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
            DisplayServer::Wayland => CapabilityState::degraded(
                "no protocol lets a client raise a window, so focus is attempted by asking \
                 the application to present itself over D-Bus and then checking whether it \
                 did: an application exporting no org.freedesktop.Application cannot be asked \
                 at all, and a compositor may answer by marking the window as demanding \
                 attention instead. Either way `desktop focus` fails rather than reporting a \
                 change that did not happen",
            ),
            _ => CapabilityState::unsupported(UnsupportedReason::NoBackendMechanism),
        },
    );

    set.set(
        Capability::Screenshots,
        match info.screenshot {
            Backend::X11 => CapabilityState::Supported,
            Backend::XdgDesktopPortal => portal_state(
                "the first capture needs a one-time approval dialog; run `desktop setup` \
                 to get it over with",
                grants.screenshot,
                info,
            ),
            _ => CapabilityState::unsupported(UnsupportedReason::NoBackendMechanism),
        },
    );

    set.set(
        Capability::WindowScreenshots,
        match info.screenshot {
            Backend::X11 => CapabilityState::Supported,
            _ => CapabilityState::unsupported(UnsupportedReason::NoBackendMechanism),
        },
    );

    set.set(
        Capability::AgentSession,
        if missing_session_helpers.is_empty() {
            CapabilityState::Supported
        } else {
            CapabilityState::unsupported(UnsupportedReason::ServiceUnavailable {
                service: missing_session_helpers.join(", "),
            })
        },
    );

    let input_state = match info.input {
        Backend::X11 => CapabilityState::Supported,
        Backend::RemoteDesktopPortal => portal_state(
            "input goes through the RemoteDesktop portal, whose first use needs approval; \
             run `desktop setup` to get it over with",
            grants.input,
            info,
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
/// Absolute pointer positioning needs both the RemoteDesktop and the ScreenCast
/// portal, because the coordinates are interpreted in a screencast stream's
/// space. The remedy names whichever half is actually missing: installing a
/// portal backend and enabling a disabled interface are different jobs, and
/// collapsing them into one message sends a user looking for the wrong thing.
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
            let remedy = match (facts.remote_desktop_portal, facts.screencast_portal) {
                (true, false) => {
                    "Your desktop advertises org.freedesktop.portal.RemoteDesktop but not                      ScreenCast, and absolute pointer positioning needs both — the                      coordinates are interpreted in a screencast stream's space. Install                      the ScreenCast half of your portal backend (on wlroots that is                      xdg-desktop-portal-wlr), or use `desktop session`, which needs no                      portal at all."
                }
                (false, _) => {
                    "This build drives input through the RemoteDesktop portal, and your                      session does not provide it. `desktop session` gives the agent an X11                      display where input works regardless."
                }
                (true, true) => {
                    "Both portals are advertised, so this is unexpected — run `desktop info`                      and report what it prints."
                }
            };
            out.push(Diagnostic::error(
                format!(
                    "No input backend for {} Wayland: mouse and keyboard are unavailable.",
                    info.desktop_environment.as_str()
                ),
                remedy,
            ));
        }

        if info.screenshot == Backend::None {
            out.push(Diagnostic::warning(
                format!(
                    "Screen capture is unavailable on {} Wayland: nothing on the session bus \
                     answers org.freedesktop.portal.Screenshot.",
                    info.desktop_environment.as_str()
                ),
                "Install xdg-desktop-portal plus the backend for your desktop                  (xdg-desktop-portal-gnome, -kde or -wlr). `desktop session` gives the agent                  an X11 display that captures natively without one.",
            ));
        }

        if info.desktop_environment != DesktopEnvironment::Gnome
            && (info.input == Backend::RemoteDesktopPortal
                || info.screenshot == Backend::XdgDesktopPortal)
        {
            out.push(Diagnostic::info(format!(
                "Capture and input here use the freedesktop portals your desktop advertises, \
                 but this build has only been verified against GNOME's backend — treat {} as \
                 untested and check the result of the first click.",
                info.desktop_environment.as_str()
            )));
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
            Backend::XdgDesktopPortal,
            Backend::RemoteDesktopPortal,
        )
    }

    fn kde_wayland_with_portals() -> BackendInfo {
        info(
            DisplayServer::Wayland,
            DesktopEnvironment::Kde,
            Backend::AtSpi,
            Backend::XdgDesktopPortal,
            Backend::RemoteDesktopPortal,
        )
    }

    /// This was unavailable outright, on the grounds that `GrabFocus` returns
    /// success and does nothing — calling *that* degraded would have invited an
    /// agent to rely on it and send every following keystroke to the wrong
    /// window. Asking the application to present itself is a different claim:
    /// it sometimes works, and the driver looks afterwards either way, so the
    /// failure mode is a reported failure rather than a silent one.
    #[test]
    fn focus_under_wayland_is_attempted_and_verified_rather_than_refused_outright() {
        let caps = capabilities_from(&gnome_wayland(), &HELPERS_INSTALLED, granted());
        match caps.get(Capability::Focus) {
            CapabilityState::Degraded { note } => {
                assert!(note.contains("org.freedesktop.Application"), "got {note}");
                assert!(note.contains("fails"), "got {note}");
            }
            other => panic!("expected degraded, got {other:?}"),
        }
        assert!(caps.is_available(Capability::Focus));
    }

    #[test]
    fn focus_remains_supported_on_x11() {
        assert_eq!(
            capabilities_from(&x11(), &HELPERS_INSTALLED, granted()).get(Capability::Focus),
            CapabilityState::Supported
        );
    }

    /// A session with no display server has no window to raise, so there is
    /// nothing to attempt and nothing to verify.
    #[test]
    fn focus_is_unavailable_where_there_is_no_display_at_all() {
        let mut headless = gnome_wayland();
        headless.display_server = DisplayServer::Headless;
        let caps = capabilities_from(&headless, &HELPERS_INSTALLED, granted());
        assert!(!caps.is_available(Capability::Focus));
    }

    /// The whole point of the grant: once it is recorded there is no dialog
    /// left to warn about, and repeating the warning is stale advice that reads
    /// as "this will interrupt you" on every run.
    #[test]
    fn a_granted_portal_capability_stops_warning_about_the_dialog() {
        let ungranted = capabilities_from(&gnome_wayland(), &HELPERS_INSTALLED, Grants::default());
        for capability in [Capability::Mouse, Capability::Screenshots] {
            match ungranted.get(capability) {
                CapabilityState::Degraded { note } => {
                    assert!(note.contains("approval"), "{capability:?} note was {note}");
                }
                other => panic!("expected {capability:?} degraded, got {other:?}"),
            }
        }

        let granted = capabilities_from(&gnome_wayland(), &HELPERS_INSTALLED, granted());
        for capability in [Capability::Mouse, Capability::Screenshots] {
            assert_eq!(
                granted.get(capability),
                CapabilityState::Supported,
                "{capability:?} should be plain once granted"
            );
        }
    }

    /// One grant is not the other. Input rides on the RemoteDesktop token and
    /// screen capture on the permission store, and a machine that has answered
    /// one dialog has not answered the other.
    #[test]
    fn the_two_grants_are_reported_independently() {
        let caps = capabilities_from(
            &gnome_wayland(),
            &HELPERS_INSTALLED,
            Grants {
                screenshot: true,
                input: false,
            },
        );
        assert_eq!(
            caps.get(Capability::Screenshots),
            CapabilityState::Supported
        );
        assert!(matches!(
            caps.get(Capability::Mouse),
            CapabilityState::Degraded { .. }
        ));
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

    /// The helpers a session needs, as a machine that has them would report.
    ///
    /// Passed explicitly so these assertions describe the mapping rather than
    /// the packages installed on whoever is running them.
    const HELPERS_INSTALLED: [&str; 0] = [];

    /// The state after `desktop setup`, which most of the table is about.
    const fn granted() -> Grants {
        Grants {
            screenshot: true,
            input: true,
        }
    }

    #[test]
    fn x11_supports_everything_without_caveats() {
        let caps = capabilities_from(&x11(), &HELPERS_INSTALLED, granted());
        for capability in Capability::ALL {
            assert_eq!(
                caps.get(capability),
                CapabilityState::Supported,
                "{capability:?} should be plainly supported on X11"
            );
        }
    }

    /// This reported "degraded — the portal shows a window picker" while
    /// `PortalCapture` returned `unsupported_capability` for every window, so
    /// `desktop capabilities` promised a route the code never took.
    #[test]
    fn window_capture_under_wayland_is_refused_because_the_capture_path_refuses_it() {
        let caps = capabilities_from(&gnome_wayland(), &HELPERS_INSTALLED, granted());
        assert!(!caps.is_available(Capability::WindowScreenshots));
    }

    /// The interface is freedesktop's and is selected wherever it is
    /// advertised; the caveat is about which backend has actually been run
    /// against, which is a different question and belongs in the note.
    #[test]
    fn a_portal_capability_away_from_gnome_says_it_is_untested_there() {
        let caps = capabilities_from(&kde_wayland_with_portals(), &HELPERS_INSTALLED, granted());
        for capability in [Capability::Mouse, Capability::Screenshots] {
            match caps.get(capability) {
                CapabilityState::Degraded { note } => {
                    assert!(note.contains("kde"), "{capability:?} note was {note}");
                }
                other => panic!("expected {capability:?} degraded, got {other:?}"),
            }
            assert!(caps.is_available(capability), "{capability:?}");
        }
    }

    /// Read before the grant, where there is still a note to inspect: on GNOME
    /// it says only that a dialog is coming, never that the backend is
    /// untested. After the grant there is no note at all, which the test above
    /// covers.
    #[test]
    fn the_same_capability_on_gnome_carries_no_untested_caveat() {
        let caps = capabilities_from(&gnome_wayland(), &HELPERS_INSTALLED, Grants::default());
        match caps.get(Capability::Mouse) {
            CapabilityState::Degraded { note } => {
                assert!(!note.contains("untested"), "got {note}");
                assert!(note.contains("approval"), "got {note}");
            }
            other => panic!("expected degraded, got {other:?}"),
        }
    }

    #[test]
    fn gnome_wayland_window_listing_is_degraded_because_at_spi_cannot_see_everything() {
        let caps = capabilities_from(&gnome_wayland(), &HELPERS_INSTALLED, granted());
        match caps.get(Capability::Windows) {
            CapabilityState::Degraded { note } => {
                assert!(note.contains("stacking order"), "got {note}");
            }
            other => panic!("expected degraded, got {other:?}"),
        }
    }

    #[test]
    fn kde_wayland_refuses_input_and_capture_but_keeps_accessibility() {
        let caps = capabilities_from(&kde_wayland(), &HELPERS_INSTALLED, granted());
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
        let caps = capabilities_from(&broken, &HELPERS_INSTALLED, granted());
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
