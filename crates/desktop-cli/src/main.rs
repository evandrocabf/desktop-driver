//! `desktop` — see, inspect and control desktop applications.

#![forbid(unsafe_code)]

use std::{io::Write as _, process::ExitCode};

use clap::Parser as _;
use desktop_cli::{Cli, cli::Command, output, run};
use desktop_core::{Driver, SessionHost, SessionStore, errors::DesktopError};

/// Runs one command.
///
/// Warnings go to stderr, so `--json` on stdout stays exactly one document, and
/// a backend that cannot start still explains itself in the requested format —
/// an agent parsing JSON must not be handed prose.
fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing();

    for role in desktop_cli::policy::unrecognised_roles(&cli) {
        eprintln!(
            "warning: --deny-role {role}: not a role this build recognises. It is kept, but \
             will only match an element whose platform role is literally {role:?}. Run \
             `desktop snapshot` to see the role names in use."
        );
    }

    let sessions = platform_sessions();
    let mut stdout = std::io::stdout().lock();
    let category = match build_driver(&cli) {
        Ok(driver) => run(&cli, &driver, sessions.as_ref(), &mut stdout),
        Err(error) => {
            let mut sink = output::Sink::new(&mut stdout, cli.json);
            output::render_error(&mut sink, &error);
            error.exit_category()
        }
    };

    let _ = stdout.flush();
    ExitCode::from(category.status())
}

fn build_driver(cli: &Cli) -> Result<Driver, DesktopError> {
    let policy = desktop_cli::policy::from_cli(cli);
    let store = SessionStore::at_default_path();
    let ports = platform_ports(cli)?;
    Ok(Driver::new(ports, policy, store))
}

/// Whether this command needs a live connection, or only a description of the
/// environment.
///
/// `info`, `capabilities` and `doctor` must work even when the a11y bus is
/// unreachable — they are precisely the commands that explain why. `session`
/// needs no desktop connection at all: it starts processes.
fn describes_only(command: &Command) -> bool {
    matches!(
        command,
        Command::Info
            | Command::Capabilities
            | Command::Doctor
            | Command::Session(_)
            | Command::Browser(_)
            | Command::BrowserDaemon(_)
    )
}

#[cfg(target_os = "linux")]
fn platform_ports(cli: &Cli) -> Result<desktop_core::ports::Ports, DesktopError> {
    let store = desktop_core::AgentSessionStore::at_default_path();
    let scope = desktop_linux::session::resolve_scope(&store, cli.host);
    if describes_only(&cli.command) {
        return Ok(desktop_linux::describe_only_ports_for(&scope));
    }
    desktop_linux::build_ports_for(&scope)
}

#[cfg(target_os = "linux")]
fn platform_sessions() -> Box<dyn SessionHost> {
    Box::new(desktop_linux::session::LinuxSessions::at_default_path())
}

#[cfg(target_os = "macos")]
fn platform_ports(cli: &Cli) -> Result<desktop_core::ports::Ports, DesktopError> {
    if describes_only(&cli.command) {
        return Ok(desktop_macos::describe_only_ports());
    }
    desktop_macos::build_ports()
}

/// macOS has one window server per login session and no supported way to make
/// another, so there is no display to give an agent of its own.
#[cfg(target_os = "macos")]
fn platform_sessions() -> Box<dyn SessionHost> {
    use desktop_core::models::backend::{DesktopEnvironment, DisplayServer, Platform};
    Box::new(desktop_core::NoSessionHost::new(
        Platform::Macos,
        DisplayServer::Quartz,
        DesktopEnvironment::Aqua,
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_ports(_cli: &Cli) -> Result<desktop_core::ports::Ports, DesktopError> {
    Err(DesktopError::BackendUnavailable {
        backend: desktop_core::models::backend::Backend::None,
        reason: "desktop-driver supports macOS and Linux only".to_owned(),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_sessions() -> Box<dyn SessionHost> {
    use desktop_core::models::backend::{DesktopEnvironment, DisplayServer, Platform};
    Box::new(desktop_core::NoSessionHost::new(
        Platform::Linux,
        DisplayServer::Headless,
        DesktopEnvironment::Unknown,
    ))
}

/// Diagnostics go to stderr so they never contaminate the JSON on stdout.
///
/// Opt-in via `DESKTOP_DRIVER_LOG`, and all-or-nothing: the `env-filter`
/// feature is deliberately not enabled, because directive parsing pulls in a
/// regex engine for something a debugging aid does not need.
fn init_tracing() {
    if std::env::var_os("DESKTOP_DRIVER_LOG").is_none() {
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::DEBUG)
        .try_init();
}
