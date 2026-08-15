//! Linux platform adapters for `desktop-driver`.
//!
//! Linux is assembled at runtime rather than being one implementation: AT-SPI
//! for the semantic tree (which works identically under X11 and Wayland), and
//! then whichever capture and input mechanisms the session actually provides.
//!
//! The rule that shapes everything: a Wayland session never silently falls back
//! to X11. XWayland answers X11 calls perfectly and reports an empty window
//! list, which is the worst kind of failure — confident and wrong.
//!
//! The whole crate is Linux-only, and it is the *contents* that are gated
//! rather than the dependency `cfg`, so `cargo check --target
//! aarch64-apple-darwin --workspace` type-checks the macOS side without trying
//! to build AT-SPI.

#![deny(unsafe_code)]
#![cfg(target_os = "linux")]

mod a11y;
mod activate;
pub mod dependencies;
mod detect;
pub mod portal;
mod probe;
mod runtime;
pub mod session;
mod unsupported;
mod wayland;
mod x11;

use desktop_core::{
    errors::Result,
    models::backend::{Backend, BackendInfo, DisplayServer},
    ports::Ports,
};

pub use detect::{detect, detect_for, session_facts};
pub use probe::{capabilities_for, diagnostics_for};
pub use session::{LinuxSessions, Scope};

/// Everything the ports need to know about *which* desktop they address.
struct Wiring {
    info: BackendInfo,
    facts: desktop_core::models::backend::SessionFacts,
    /// The accessibility bus, or `None` for the session's own.
    a11y_address: Option<String>,
    display: x11::DisplayTarget,
}

impl Wiring {
    fn for_scope(scope: &Scope) -> Result<Self> {
        match scope {
            Scope::Host => Ok(Self {
                info: detect::detect(),
                facts: detect::session_facts(),
                a11y_address: None,
                display: x11::DisplayTarget::host(),
            }),
            Scope::Agent(session) => Ok(Self {
                info: detect::detect_for(session),
                facts: detect::session_facts_for(session),
                a11y_address: Some(session.a11y_address.clone()),
                display: x11::DisplayTarget {
                    display: Some(session.display.clone()),
                    cookie: Some(session.cookie_bytes()?),
                },
            }),
        }
    }
}

/// Builds the port set for the host's own desktop.
pub fn build_ports() -> Result<Ports> {
    build_ports_for(&Scope::Host)
}

/// Builds the port set for a scope.
///
/// Failures to reach one subsystem do not fail the others: a session with no
/// input backend still reads trees and takes snapshots, and says so through
/// `desktop capabilities`. That holds for the accessibility bus too — capture
/// and input do not need it, and refusing a screenshot because an unrelated
/// service is down would contradict the whole point of four independent ports.
/// Observed for real in a container with no session bus, where `desktop
/// screenshot` failed with an AT-SPI error. When it is unreachable the tree is
/// reported as absent, so `capabilities` matches what actually happens rather
/// than promising one that cannot be read.
///
/// No window activator is supplied under Wayland: raising a window is X11-only,
/// and there is no protocol for it, so the refusal reaches the caller intact.
pub fn build_ports_for(scope: &Scope) -> Result<Ports> {
    let wiring = Wiring::for_scope(scope)?;
    let mut info = wiring.info;

    let window_source: Option<Box<dyn x11::WindowSource>> =
        if info.display_server == DisplayServer::X11 {
            x11::Ewmh::connect(&wiring.display)
                .ok()
                .map(|ewmh| Box::new(ewmh) as Box<dyn x11::WindowSource>)
        } else {
            None
        };

    let accessibility: Box<dyn desktop_core::ports::AccessibilityPort> =
        match a11y::AtSpi::connect_to(
            wiring.a11y_address.as_deref(),
            info.display_server == DisplayServer::Wayland,
            info.clone(),
        ) {
            Ok(atspi) => Box::new(atspi.with_window_source(window_source)),
            Err(error) => {
                tracing::debug!(%error, "accessibility unavailable; other ports continue");
                info.accessibility = Backend::None;
                info.windows = Backend::None;
                Box::new(unsupported::UnsupportedAccessibility::new(info.clone()))
            }
        };

    let capture: Box<dyn desktop_core::ports::CapturePort> = match info.screenshot {
        Backend::X11 => Box::new(x11::X11Capture::connect(&wiring.display)?),
        Backend::XdgDesktopPortal => Box::new(wayland::PortalCapture::new(info.clone())),
        _ => Box::new(unsupported::UnsupportedCapture::new(info.clone())),
    };

    let input: Box<dyn desktop_core::ports::InputPort> = match info.input {
        Backend::X11 => Box::new(x11::X11Input::connect(&wiring.display)?),
        Backend::RemoteDesktopPortal => Box::new(wayland::PortalInput::new()),
        _ => Box::new(unsupported::UnsupportedInput::new(info.clone())),
    };

    Ok(Ports {
        accessibility,
        capture,
        input,
        probe: Box::new(probe::LinuxProbe::new(info, wiring.facts)),
    })
}

/// Ports that can only describe the environment.
///
/// `info`, `capabilities` and `doctor` are exactly the commands a user reaches
/// for when the accessibility bus is down, so they must not require it. Every
/// doing-port here refuses with a structured error.
#[must_use]
pub fn describe_only_ports() -> Ports {
    describe_only_ports_for(&Scope::Host)
}

#[must_use]
pub fn describe_only_ports_for(scope: &Scope) -> Ports {
    let (info, facts) = match scope {
        Scope::Host => (detect::detect(), detect::session_facts()),
        Scope::Agent(session) => (
            detect::detect_for(session),
            detect::session_facts_for(session),
        ),
    };
    Ports {
        accessibility: Box::new(unsupported::UnsupportedAccessibility::new(info.clone())),
        capture: Box::new(unsupported::UnsupportedCapture::new(info.clone())),
        input: Box::new(unsupported::UnsupportedInput::new(info.clone())),
        probe: Box::new(probe::LinuxProbe::new(info, facts)),
    }
}

/// The environment description, available without connecting to anything.
#[must_use]
pub fn backend_info() -> BackendInfo {
    detect::detect()
}
