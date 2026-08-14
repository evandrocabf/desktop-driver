//! Platform-independent core of `desktop-driver`.
//!
//! Everything here compiles and runs on any platform: the domain models, the
//! snapshot normalizer, selector matching, policy, structured errors, and the
//! [`Driver`] that composes the four platform
//! [`ports`]. No accessibility or windowing dependency is pulled in.
//!
//! The design principle throughout: where two platforms genuinely disagree,
//! the model says so. An absolute screen coordinate that does not exist under
//! Wayland is `None`, not zero.

#![forbid(unsafe_code)]

pub mod agent;
pub mod driver;
pub mod errors;
pub mod models;
pub mod normalize;
pub mod policy;
pub mod ports;
pub mod session;
pub mod testing;

pub use agent::{
    AgentSession, AgentSessionStore, NoSessionHost, SessionHost, SessionProcess, StartOptions,
};
pub use driver::{Activation, Driver};
pub use errors::{DesktopError, ExitCategory, Permission, PermissionState, Result};
pub use policy::{Action, Policy};
pub use ports::{
    AccessibilityPort, CapturePort, CaptureTarget, Diagnostic, InputPort, MouseButton,
    PlatformProbe, Ports, ResolvedTree, Severity,
};
pub use session::SessionStore;
