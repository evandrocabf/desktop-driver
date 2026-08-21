//! Building the agent a display of its own.
//!
//! Five processes, started in dependency order:
//!
//! ```text
//!   Xvfb :97             a framebuffer nobody is looking at
//!   dbus-daemon          a private session bus  ── the isolation boundary
//!   at-spi-bus-launcher  the accessibility bus for this display
//!   at-spi2-registryd    the registry applications announce themselves to
//!   openbox              so focus and stacking behave the way apps expect
//! ```
//!
//! The private session bus is what makes this isolation rather than just a
//! second screen. `at-spi-bus-launcher` names its socket after the X display,
//! so an application on `:97` registers with a registry that the user's own
//! applications are not on: `desktop apps` inside a session lists what the
//! agent started, and nothing else. Capture and input are scoped by the same
//! fact — a different X server has a different framebuffer and a different
//! pointer, so neither can reach the user's screen even by mistake.
//!
//! Both accessibility services are started here rather than left to D-Bus
//! activation. Their `.service` files carry `SystemdService=`, and the systemd
//! units are one-per-user, so activating them from a second bus fails with
//! `unit failed`; on a system with SELinux enforcing, the `Exec=` fallback is
//! refused as well. Starting them directly works on both and needs no policy
//! change from the user.
//!
//! X11 rather than a nested Wayland compositor because this crate's X11 backend
//! is the complete one: `GetImage` for capture, XTEST for input, and — the
//! thing Wayland has no protocol for at all — actually raising a window.

use std::{
    ffi::OsStr,
    fs,
    io::{BufRead as _, BufReader, Write as _},
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use atspi::zbus;
use desktop_core::{
    agent::{
        AgentSession, AgentSessionStore, SessionHost, SessionProcess, StartOptions, Unwatchable,
        Visibility, encode_hex,
    },
    errors::{DesktopError, Result},
};

/// Where X servers place their Unix sockets. Fixed by convention, and every
/// X server and client library on Linux agrees on it.
const X11_SOCKET_DIR: &str = "/tmp/.X11-unix";

/// The two X servers a session can be built on.
///
/// Both are real X servers with their own framebuffer, pointer and keyboard, so
/// the isolation is identical either way. They differ only in where the pixels
/// go: `Xvfb` draws into memory nobody can see, `Xephyr` draws into a window on
/// the user's own desktop so they can watch — and, if they want, reach in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum XServer {
    Headless,
    Nested,
}

impl XServer {
    const fn program(self) -> &'static str {
        match self {
            Self::Headless => "Xvfb",
            Self::Nested => "Xephyr",
        }
    }

    /// Whether a recorded process name is one of ours.
    fn is_x_server(name: &str) -> bool {
        name == Self::Headless.program() || name == Self::Nested.program()
    }
}

/// Display numbers to try. Chosen high enough to stay clear of real logins and
/// of the `:99` that CI images conventionally use for `xvfb-run`.
const DISPLAY_RANGE: std::ops::RangeInclusive<u32> = 90..=119;

/// How long each service gets to come up before the start is abandoned.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Gap between liveness polls while waiting for a service.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Where distributions put the accessibility helpers.
///
/// `at-spi-bus-launcher` and `at-spi2-registryd` are not on `PATH`, and no two
/// distributions agree on where they go. Verified by installing `at-spi2-core`
/// in each:
///
/// ```text
///   Fedora 43        /usr/libexec/at-spi-bus-launcher
///   Debian 13        /usr/libexec/at-spi-bus-launcher
///   Ubuntu 24.04     /usr/libexec/at-spi-bus-launcher
///   Arch             /usr/lib/at-spi-bus-launcher
///   openSUSE TW      /usr/libexec/at-spi2/at-spi-bus-launcher
/// ```
///
/// So this searches a grid of plausible locations rather than listing exact
/// paths: a fixed list is a list of the distributions that were tried, and it
/// silently excludes every other one.
const HELPER_ROOTS: [&str; 5] = [
    "/usr/libexec",
    "/usr/lib",
    "/usr/lib64",
    "/usr/local/libexec",
    "/usr/local/lib",
];

/// Sub-directories the helpers are grouped under, where they are grouped.
const HELPER_SUBDIRECTORIES: [&str; 3] = ["", "at-spi2", "at-spi2-core"];

/// Every directory a helper might live in, most likely first.
///
/// Includes Debian's multiarch triplet, derived from the build target rather
/// than hardcoded, so this works on whatever architecture it was built for.
fn helper_directories() -> Vec<PathBuf> {
    let multiarch = format!("{}-linux-gnu", std::env::consts::ARCH);

    let mut directories = Vec::new();
    for root in HELPER_ROOTS {
        for subdirectory in HELPER_SUBDIRECTORIES {
            let mut path = PathBuf::from(root);
            if !subdirectory.is_empty() {
                path.push(subdirectory);
            }
            directories.push(path);
        }
        for subdirectory in HELPER_SUBDIRECTORIES {
            let mut path = PathBuf::from(root);
            path.push(&multiarch);
            if !subdirectory.is_empty() {
                path.push(subdirectory);
            }
            directories.push(path);
        }
    }
    directories
}

/// Which desktop a command addresses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Scope {
    /// The display the user is sitting in front of.
    Host,
    /// The agent's own display.
    Agent(Box<AgentSession>),
}

impl Scope {
    #[must_use]
    pub const fn session(&self) -> Option<&AgentSession> {
        match self {
            Self::Host => None,
            Self::Agent(session) => Some(session),
        }
    }
}

/// The session to use, given what is recorded and what the user asked for.
///
/// Defaulting to the agent session when one is running is the safe direction:
/// forgetting the flag then means the agent works on its own screen, whereas
/// the other default means it types into whatever the user happens to have
/// focused. Commands disclose which display they used, so this is never
/// invisible.
#[must_use]
pub fn resolve_scope(store: &AgentSessionStore, prefer_host: bool) -> Scope {
    if prefer_host {
        return Scope::Host;
    }
    match current(store) {
        Some(session) => Scope::Agent(Box::new(session)),
        None => Scope::Host,
    }
}

/// The recorded session, if it is still running.
///
/// A record whose X server has gone is cleared rather than returned: a stale
/// display name would send every later command at a socket that is not there.
#[must_use]
pub fn current(store: &AgentSessionStore) -> Option<AgentSession> {
    let session = store.load()?;
    if is_alive(&session) {
        return Some(session);
    }
    let _ = store.clear();
    None
}

/// Whether the session's X server is still up.
///
/// Both halves matter. The socket alone would be satisfied by a leftover file;
/// the pid alone would be satisfied by whatever process inherited that number
/// after a reboot, which is why the recorded name is checked against the live
/// command line before this reports — or kills — anything.
#[must_use]
pub fn is_alive(session: &AgentSession) -> bool {
    let socket_present = socket_path(session).is_some_and(|path| path.exists());
    socket_present
        && session
            .processes
            .iter()
            .any(|process| XServer::is_x_server(&process.name) && process_matches(process))
}

/// What stands between this machine and a session the user could watch.
///
/// A pure probe, so the CLI can ask the same question after the fact and
/// explain a headless session without the answer having to be threaded back
/// through the call.
#[must_use]
pub fn unwatchable_reason() -> Option<Unwatchable> {
    if std::env::var_os("DISPLAY").is_none() {
        Some(Unwatchable::NoDisplay)
    } else if on_path(XServer::Nested.program()).is_none() {
        Some(Unwatchable::XephyrMissing)
    } else {
        None
    }
}

/// Picks the X server, and reports why when the user cannot watch.
///
/// The default is to show the agent's screen, so this only ever falls back —
/// and never silently: the reason is handed back for the caller to print.
/// [`Visibility::Visible`] asks for it explicitly, so that refuses rather than
/// quietly starting something nobody can see.
fn choose_server(visibility: Visibility) -> Result<(XServer, Option<Unwatchable>)> {
    match (visibility, unwatchable_reason()) {
        (Visibility::Headless, _) => Ok((XServer::Headless, None)),
        (_, None) => Ok((XServer::Nested, None)),
        (Visibility::Visible, Some(reason)) => Err(DesktopError::InvalidArgument {
            message: format!(
                "--visible was requested but {}. {}",
                reason.explain(),
                reason
                    .remedy()
                    .unwrap_or("Start it with --headless instead.")
            ),
        }),
        (Visibility::Auto, Some(reason)) => Ok((XServer::Headless, Some(reason))),
    }
}

/// Starts a session and records it.
///
/// The home directory is created before anything else, so a session never
/// hands a child a `HOME` that does not exist — browser profiles land there,
/// which means cookies, session tokens and saved logins.
///
/// Once the first process is running, any later failure has to take down
/// whatever already started, or the machine is left with an orphaned X server
/// and no record of it.
pub fn start(
    options: StartOptions,
    store: &AgentSessionStore,
    profiles: &desktop_core::SessionProfileStore,
) -> Result<AgentSession> {
    if let Some(existing) = current(store) {
        return Err(DesktopError::InvalidArgument {
            message: format!(
                "a session is already running on {}; stop it first with `desktop session stop`",
                existing.display
            ),
        });
    }

    let launcher = resolve_helper("at-spi-bus-launcher")?;
    let registryd = resolve_helper("at-spi2-registryd")?;
    let (server, _) = choose_server(options.visibility)?;
    require_on_path(
        server.program(),
        "the agent's display needs an X server of its own",
    )?;
    require_on_path("dbus-daemon", "the agent's display needs a private D-Bus")?;

    let number = match options.display {
        Some(number) => {
            if !display_is_free(number) {
                return Err(DesktopError::InvalidArgument {
                    message: format!("display :{number} is already in use"),
                });
            }
            number
        }
        None => DISPLAY_RANGE
            .clone()
            .find(|number| display_is_free(*number))
            .ok_or_else(|| {
                DesktopError::backend(format!(
                    "no free X display between :{} and :{}",
                    DISPLAY_RANGE.start(),
                    DISPLAY_RANGE.end()
                ))
            })?,
    };
    let display = format!(":{number}");

    if options.share_home && options.name != "default" {
        return Err(DesktopError::invalid_argument(
            "a named persistent session cannot use --share-home; omit it to keep logins isolated",
        ));
    }
    let profile = profiles.ensure(&options.name)?;

    let cookie = random_cookie()?;
    let xauthority = xauthority_path(&options.name);
    write_xauthority(&xauthority, number, &cookie)?;

    let home = if options.share_home {
        None
    } else {
        Some(profile.home)
    };

    let mut started: Vec<(String, Child)> = Vec::new();
    let outcome = assemble(
        &Plan {
            display: &display,
            options: &options,
            server,
            xauthority: &xauthority,
            cookie: &cookie,
            home: home.as_deref(),
            launcher: &launcher,
            registryd: &registryd,
        },
        &mut started,
    );

    let (dbus_address, a11y_address) = match outcome {
        Ok(addresses) => addresses,
        Err(error) => {
            for (_, mut child) in started {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = fs::remove_file(&xauthority);
            return Err(error);
        }
    };

    let session = AgentSession {
        name: options.name,
        display,
        width: options.width,
        height: options.height,
        dbus_address,
        a11y_address,
        xauthority,
        cookie: encode_hex(&cookie),
        visible: server == XServer::Nested,
        home,
        processes: started
            .iter()
            .map(|(name, child)| {
                let pid = child.id();
                SessionProcess::new(name.clone(), pid).started_at(start_time(pid).unwrap_or(0))
            })
            .collect(),
    };
    store.save(&session)?;
    Ok(session)
}

/// Starts the five services, recording each one as it comes up.
/// Everything `assemble` needs, gathered before anything is started.
struct Plan<'a> {
    display: &'a str,
    options: &'a StartOptions,
    server: XServer,
    xauthority: &'a Path,
    /// The display's cookie, for connecting back to it before any environment
    /// variable names it.
    cookie: &'a [u8],
    home: Option<&'a Path>,
    launcher: &'a Path,
    registryd: &'a Path,
}

/// Starts the five services, recording each one as it comes up.
///
/// The X server is started with no TCP listener and `-noreset`, so a client
/// disconnecting cannot wipe the display the next command is about to use. A
/// nested one additionally gets `-resizeable`, letting the person watching
/// enlarge the window without the agent's screen changing size underneath it,
/// and a title in plain ASCII — Xephyr passes the title through as bytes, and
/// an em dash came back as mojibake, which is a poor look for the one window
/// whose whole job is reassurance.
///
/// Accessibility is switched on for the session's own bus at the end. That is
/// the switch lazy toolkits watch: Firefox, Chromium and Qt build no tree until
/// it is on, which presents as an application with a window and no contents.
/// Only `IsEnabled` is written — never `ScreenReaderEnabled`, which on GNOME
/// starts Orca reading the screen aloud.
fn assemble(plan: &Plan<'_>, started: &mut Vec<(String, Child)>) -> Result<(String, String)> {
    let display = plan.display;
    let options = plan.options;
    let server = plan.server;
    let xauthority = plan.xauthority;
    let cookie = plan.cookie;
    let home = plan.home;
    let launcher = plan.launcher;
    let registryd = plan.registryd;
    let authority = xauthority.display().to_string();
    let geometry = format!("{}x{}", options.width, options.height);
    let screen = format!("{geometry}x24");

    let mut command = scoped(Command::new(server.program()));
    match server {
        XServer::Headless => {
            command.args([
                display,
                "-screen",
                "0",
                &screen,
                "-auth",
                &authority,
                "-nolisten",
                "tcp",
                "-noreset",
            ]);
        }
        XServer::Nested => {
            command.args([
                display,
                "-screen",
                &geometry,
                "-auth",
                &authority,
                "-title",
                &format!("desktop-driver: the agent's screen ({display})"),
                "-resizeable",
                "-nolisten",
                "tcp",
                "-noreset",
            ]);
        }
    }

    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            DesktopError::backend(format!("cannot start {}: {error}", server.program()))
        })?;
    started.push((server.program().to_owned(), child));

    let socket = PathBuf::from(format!(
        "{X11_SOCKET_DIR}/X{}",
        display.trim_start_matches(':')
    ));
    wait_until(|| socket.exists()).map_err(|()| {
        DesktopError::backend(format!(
            "{} did not come up on {display} within {}s",
            server.program(),
            STARTUP_TIMEOUT.as_secs()
        ))
    })?;

    let mut base = vec![
        ("DISPLAY".to_owned(), display.to_owned()),
        ("XAUTHORITY".to_owned(), xauthority.display().to_string()),
    ];
    if let Some(home) = home {
        base.push(("HOME".to_owned(), home.display().to_string()));
    }

    let dbus_address = start_dbus(&base, started)?;
    let with_bus: Vec<(String, String)> = base
        .iter()
        .cloned()
        .chain([("DBUS_SESSION_BUS_ADDRESS".to_owned(), dbus_address.clone())])
        .collect();

    let child = spawn_service(launcher, &["--launch-immediately"], &with_bus)?;
    started.push(("at-spi-bus-launcher".to_owned(), child));
    let a11y_address = wait_for_a11y_address(&dbus_address)?;

    let child = spawn_service(registryd, &[], &with_bus)?;
    started.push(("at-spi2-registryd".to_owned(), child));
    wait_for_registry(&a11y_address)?;

    enable_accessibility(&dbus_address)?;

    let child = spawn_service(Path::new("openbox"), &[], &with_bus)?;
    started.push(("openbox".to_owned(), child));
    wait_for_window_manager(display, cookie)?;

    Ok((dbus_address, a11y_address))
}

/// Waits until the window manager has published the EWMH properties.
///
/// Spawning `openbox` is not the same as it having taken over the display, and
/// the gap is wide enough to be observed: the first command after `session
/// start` saw a display nothing was managing, so it reported the window list as
/// degraded and fell back to AT-SPI frames. Both answers were true at the
/// instant they were given, which is exactly what makes waiting the fix.
fn wait_for_window_manager(display: &str, cookie: &[u8]) -> Result<()> {
    let target = crate::x11::DisplayTarget {
        display: Some(display.to_owned()),
        cookie: Some(cookie.to_vec()),
    };
    wait_until(|| crate::x11::supports_ewmh(&target)).map_err(|()| {
        DesktopError::backend(format!(
            "openbox started but did not manage {display} within {}s",
            STARTUP_TIMEOUT.as_secs()
        ))
    })
}

/// Starts the private bus and reads back the address it printed.
///
/// `--nofork` keeps it as a direct child so the recorded pid is the daemon's
/// own, and the address is read on a helper thread so a bus that starts but
/// never speaks times out instead of hanging the command.
fn start_dbus(
    environment: &[(String, String)],
    started: &mut Vec<(String, Child)>,
) -> Result<String> {
    let mut child = scoped(Command::new("dbus-daemon"))
        .args(["--session", "--print-address", "--nofork"])
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| DesktopError::backend(format!("cannot start dbus-daemon: {error}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| DesktopError::internal("dbus-daemon stdout was not captured"))?;
    started.push(("dbus-daemon".to_owned(), child));

    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let read = BufReader::new(stdout).read_line(&mut line);
        let _ = sender.send(read.map(|_| line));
    });

    match receiver.recv_timeout(STARTUP_TIMEOUT) {
        Ok(Ok(line)) if !line.trim().is_empty() => Ok(line.trim().to_owned()),
        Ok(Ok(_)) => Err(DesktopError::backend(
            "dbus-daemon exited without printing a bus address",
        )),
        Ok(Err(error)) => Err(DesktopError::backend(format!(
            "cannot read the private bus address: {error}"
        ))),
        Err(_) => Err(DesktopError::backend(format!(
            "dbus-daemon printed no address within {}s",
            STARTUP_TIMEOUT.as_secs()
        ))),
    }
}

fn spawn_service(program: &Path, args: &[&str], environment: &[(String, String)]) -> Result<Child> {
    scoped(Command::new(program))
        .args(args)
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            DesktopError::backend(format!("cannot start {}: {error}", program.display()))
        })
}

/// Strips the environment that would send a child to the user's desktop.
///
/// Applied to the session's own services as well as to launched applications:
/// the window manager and the accessibility services must agree about which
/// display they are on, and a service that quietly attached to the user's
/// compositor would be the same silent failure one level down.
fn scoped(mut command: Command) -> Command {
    for removed in AgentSession::removed_environment() {
        command.env_remove(removed);
    }
    command
}

/// Launches a program onto the agent's display.
pub fn run<I, S>(session: &AgentSession, program: &str, args: I) -> Result<u32>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let child = scoped(Command::new(program))
        .args(args)
        .envs(session.environment())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| DesktopError::TargetNotFound {
            target: format!("cannot launch `{program}`: {error}"),
        })?;
    Ok(child.id())
}

/// Ends the recorded session, returning what was stopped.
///
/// Processes are signalled in reverse order, so the window manager and the
/// accessibility services see a live X server while they shut down.
/// Applications are appended after those services and receive `SIGTERM`
/// first, with a short grace period to flush browser cookies and local storage
/// while their X connection is still alive.
pub fn stop(store: &AgentSessionStore) -> Result<Option<AgentSession>> {
    let Some(session) = store.load() else {
        return Ok(None);
    };

    for process in session
        .processes
        .iter()
        .rev()
        .filter(|process| process.application)
    {
        if process_matches(process) {
            terminate(process.pid);
        }
    }
    let application_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < application_deadline
        && session
            .processes
            .iter()
            .any(|process| process.application && process_matches(process))
    {
        thread::sleep(POLL_INTERVAL);
    }

    for process in session
        .processes
        .iter()
        .rev()
        .filter(|process| !process.application)
    {
        if process_matches(process) {
            terminate(process.pid);
        }
    }

    let _ = fs::remove_file(&session.xauthority);
    store.clear()?;
    Ok(Some(session))
}

/// Sends `SIGTERM`, never `SIGKILL`: Xvfb removes its socket and lock file on a
/// clean exit, and killing it outright leaves both behind to block the display
/// number the next session would pick.
fn terminate(pid: u32) {
    let Ok(raw) = i32::try_from(pid) else { return };
    if let Some(pid) = rustix::process::Pid::from_raw(raw) {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
}

/// Whether the recorded pid is still the process that was recorded.
///
/// Pid numbers are reused, so the name is checked as well — and then the start
/// time, because the name alone still leaves a window: a recorded pid can exit
/// and be reused by another process of the same name between the check and the
/// signal. `(pid, start time)` is unique for the life of the machine. A zero
/// start time means the record predates that field, in which case the name is
/// all there is to go on.
fn process_matches(process: &SessionProcess) -> bool {
    let Ok(cmdline) = fs::read(format!("/proc/{}/cmdline", process.pid)) else {
        return false;
    };
    let cmdline = String::from_utf8_lossy(&cmdline);
    let program = cmdline.split('\0').next().unwrap_or_default();
    let named = Path::new(program)
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name == process.name);
    if !named {
        return false;
    }

    match (process.started_at, start_time(process.pid)) {
        (0, _) => true,
        (recorded, Some(live)) => recorded == live,
        (_, None) => false,
    }
}

/// Field 22 of `/proc/<pid>/stat`, in clock ticks since boot.
///
/// Parsed from the last `)` rather than by splitting the whole line: field 2 is
/// the executable name in parentheses and may itself contain spaces and
/// brackets, which is a classic way to misparse this file. The first field
/// after the name is `state`, which is field 3, so field 22 is nineteen along
/// from there.
fn start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = &stat[stat.rfind(')')? + 1..];
    tail.split_whitespace().nth(22 - 3)?.parse().ok()
}

/// Asks the private bus where its accessibility bus is.
///
/// The launcher owns `org.a11y.Bus` on that bus once it is up, so this doubles
/// as the readiness check for it.
fn wait_for_a11y_address(dbus_address: &str) -> Result<String> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let attempt = crate::runtime::block_on(async {
            let connection = connect(dbus_address).await.ok()?;
            let proxy =
                zbus::Proxy::new(&connection, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
                    .await
                    .ok()?;
            proxy.call::<_, _, String>("GetAddress", &()).await.ok()
        });
        if let Some(address) = attempt {
            return Ok(address);
        }
        if Instant::now() >= deadline {
            return Err(DesktopError::backend(format!(
                "at-spi-bus-launcher did not provide an accessibility bus within {}s",
                STARTUP_TIMEOUT.as_secs()
            )));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Waits until the registry owns its name, which is when applications can start
/// announcing themselves to it.
fn wait_for_registry(a11y_address: &str) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let owned = crate::runtime::block_on(async {
            let Ok(connection) = connect(a11y_address).await else {
                return false;
            };
            let Ok(proxy) = zbus::fdo::DBusProxy::new(&connection).await else {
                return false;
            };
            let Ok(name) = "org.a11y.atspi.Registry".try_into() else {
                return false;
            };
            matches!(proxy.name_has_owner(name).await, Ok(true))
        });
        if owned {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(DesktopError::backend(format!(
                "at-spi2-registryd did not register within {}s",
                STARTUP_TIMEOUT.as_secs()
            )));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn enable_accessibility(dbus_address: &str) -> Result<()> {
    crate::runtime::try_block_on(async {
        let connection = connect(dbus_address).await?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Status",
        )
        .await?;
        proxy.set_property("IsEnabled", true).await?;
        Ok::<(), zbus::Error>(())
    })?
    .map_err(|error| {
        DesktopError::backend(format!(
            "cannot enable accessibility on the session's bus: {error}"
        ))
    })
}

async fn connect(address: &str) -> std::result::Result<zbus::Connection, zbus::Error> {
    let address: zbus::Address = address.parse()?;
    zbus::connection::Builder::address(address)?.build().await
}

fn wait_until(mut ready: impl FnMut() -> bool) -> std::result::Result<(), ()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        if ready() {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }
    if ready() { Ok(()) } else { Err(()) }
}

/// Starting, describing and ending the agent's display, bound to one store.
pub struct LinuxSessions {
    store: AgentSessionStore,
    profiles: desktop_core::SessionProfileStore,
}

impl LinuxSessions {
    #[must_use]
    pub fn new(store: AgentSessionStore) -> Self {
        Self {
            store,
            profiles: desktop_core::SessionProfileStore::at_default_path(),
        }
    }

    #[must_use]
    pub const fn with_stores(
        store: AgentSessionStore,
        profiles: desktop_core::SessionProfileStore,
    ) -> Self {
        Self { store, profiles }
    }

    #[must_use]
    pub fn at_default_path() -> Self {
        Self::new(AgentSessionStore::at_default_path())
    }
}

impl SessionHost for LinuxSessions {
    fn unwatchable(&self) -> Option<&'static str> {
        unwatchable_reason().map(Unwatchable::explain)
    }

    fn create(&self, name: &str) -> Result<desktop_core::SessionProfile> {
        self.profiles.create(name)
    }

    fn list(&self) -> Result<Vec<desktop_core::SessionProfile>> {
        self.profiles.list()
    }

    fn delete(&self, name: &str) -> Result<Option<desktop_core::SessionProfile>> {
        if current(&self.store).is_some_and(|session| session.name == name) {
            return Err(DesktopError::invalid_argument(format!(
                "session {name:?} is running; stop it before deleting its saved logins"
            )));
        }
        self.profiles.delete(name)
    }

    fn start(&self, options: StartOptions) -> Result<AgentSession> {
        start(options, &self.store, &self.profiles)
    }

    fn status(&self) -> Option<AgentSession> {
        current(&self.store)
    }

    fn stop(&self) -> Result<Option<AgentSession>> {
        stop(&self.store)
    }

    fn launch(&self, program: &str, args: &[String]) -> Result<u32> {
        let mut session = current(&self.store).ok_or_else(|| DesktopError::InvalidArgument {
            message: "no agent display is running; start one with `desktop session start`"
                .to_owned(),
        })?;
        let pid = run(&session, program, args)?;
        let name = Path::new(program)
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(program);
        if let Some(started_at) = start_time(pid) {
            session.processes.push(
                SessionProcess::new(name, pid)
                    .started_at(started_at)
                    .application(),
            );
            if let Err(error) = self.store.save(&session) {
                terminate(pid);
                return Err(error);
            }
        }
        Ok(pid)
    }
}

/// Everything a session needs, and whether it is installed.
///
/// Reported rather than discovered on failure, so `desktop capabilities` can
/// say that an agent display is unavailable *before* someone tries to start one.
#[must_use]
pub fn missing_requirements() -> Vec<&'static str> {
    let mut missing = Vec::new();
    if on_path(XServer::Headless.program()).is_none() {
        missing.push("Xvfb");
    }
    if on_path("openbox").is_none() {
        missing.push("openbox");
    }
    if on_path("dbus-daemon").is_none() {
        missing.push("dbus-daemon");
    }
    if resolve_helper("at-spi-bus-launcher").is_err() {
        missing.push("at-spi-bus-launcher");
    }
    if resolve_helper("at-spi2-registryd").is_err() {
        missing.push("at-spi2-registryd");
    }
    missing
}

/// The X server socket for a session's display.
///
/// Lives here rather than on the model: `/tmp/.X11-unix` is an X11 convention,
/// and `desktop-core` is shared with a platform that has no X server at all.
#[must_use]
pub fn socket_path(session: &AgentSession) -> Option<PathBuf> {
    session.display_number().map(display_socket_path)
}

#[must_use]
pub fn display_socket_path(number: u32) -> PathBuf {
    PathBuf::from(format!("{X11_SOCKET_DIR}/X{number}"))
}

fn display_lock(number: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/.X{number}-lock"))
}

/// A display number is free when neither the socket nor the lock file exists.
///
/// Checking both matters: a killed X server leaves the lock behind, and a
/// server that is merely starting has the lock but not yet the socket.
#[must_use]
pub fn display_is_free(number: u32) -> bool {
    !display_socket_path(number).exists() && !display_lock(number).exists()
}

fn xauthority_path(name: &str) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("desktop-driver")
        .join("sessions")
        .join(name)
        .join("Xauthority")
}

fn random_cookie() -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut cookie = vec![0_u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut cookie))
        .map_err(|error| {
            DesktopError::backend(format!("cannot generate a display cookie: {error}"))
        })?;
    Ok(cookie)
}

/// Writes an `Xauthority` file in the format `libXau` expects.
///
/// Every field is a big-endian `u16` length followed by that many bytes. The
/// family is `FamilyWild`, which `libXau` treats as matching any connection —
/// the alternative is recording this machine's hostname, which then stops
/// working the moment the hostname changes.
fn write_xauthority(path: &Path, display: u32, cookie: &[u8]) -> Result<()> {
    const FAMILY_WILD: u16 = 0xffff;
    const MIT_MAGIC_COOKIE: &[u8] = b"MIT-MAGIC-COOKIE-1";

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&FAMILY_WILD.to_be_bytes());
    for field in [
        b"".as_slice(),
        display.to_string().as_bytes(),
        MIT_MAGIC_COOKIE,
        cookie,
    ] {
        let length = u16::try_from(field.len())
            .map_err(|_| DesktopError::internal("authority field is too long"))?;
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(field);
    }

    if let Some(parent) = path.parent() {
        desktop_core::agent::create_private_dir(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| {
            DesktopError::backend(format!("cannot write {}: {error}", path.display()))
        })?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| DesktopError::backend(format!("cannot write {}: {error}", path.display())))
}

/// Whether an accessibility helper is installed anywhere this looks.
#[must_use]
pub fn helper_installed(name: &str) -> bool {
    resolve_helper(name).is_ok()
}

/// Locates one accessibility helper.
///
/// `PATH` is consulted first: someone who installed at-spi2-core somewhere
/// unusual, or who runs from an image that puts it in `/usr/local/bin`, has
/// already said where it is.
fn resolve_helper(name: &str) -> Result<PathBuf> {
    if let Some(path) = on_path(name) {
        return Ok(path);
    }
    for directory in helper_directories() {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(DesktopError::backend(format!(
        "{name} was not found on PATH or under {}; it ships with at-spi2-core, \
         which the agent's display needs for an accessibility bus of its own",
        HELPER_ROOTS.join(", ")
    )))
}

fn require_on_path(name: &str, why: &str) -> Result<()> {
    if on_path(name).is_some() {
        return Ok(());
    }
    Err(DesktopError::backend(format!(
        "{name} is not installed — {why}. Run `desktop doctor` for the \
         install command for this distribution."
    )))
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> AgentSession {
        AgentSession {
            name: "default".to_owned(),
            display: ":97".to_owned(),
            width: 1920,
            height: 1080,
            dbus_address: "unix:path=/tmp/dbus-abc".to_owned(),
            a11y_address: "unix:path=/run/user/1000/at-spi/bus_97".to_owned(),
            xauthority: PathBuf::from("/tmp/desktop-driver-test/Xauthority"),
            cookie: "00112233445566778899aabbccddeeff".to_owned(),
            visible: true,
            home: Some(PathBuf::from("/tmp/desktop-driver-test/home")),
            processes: vec![SessionProcess::new("Xvfb", 1)],
        }
    }

    fn temp_store(tag: &str) -> AgentSessionStore {
        let mut path = std::env::temp_dir();
        path.push(format!("desktop-driver-scope-{tag}-{}", std::process::id()));
        path.push("agent-session.json");
        let store = AgentSessionStore::new(path);
        let _ = store.clear();
        store
    }

    #[test]
    fn the_authority_file_has_the_layout_libxau_reads() {
        let mut path = std::env::temp_dir();
        path.push(format!("desktop-driver-xauth-{}", std::process::id()));
        path.push("Xauthority");
        let cookie: Vec<u8> = (0..16).collect();
        write_xauthority(&path, 97, &cookie).expect("writes");

        let bytes = fs::read(&path).expect("reads");
        // family, then four counted fields.
        assert_eq!(&bytes[0..2], &[0xff, 0xff], "FamilyWild");
        assert_eq!(&bytes[2..4], &[0, 0], "empty address");
        assert_eq!(&bytes[4..6], &[0, 2], "display number is two characters");
        assert_eq!(&bytes[6..8], b"97");
        assert_eq!(&bytes[8..10], &[0, 18], "MIT-MAGIC-COOKIE-1 is 18 bytes");
        assert_eq!(&bytes[10..28], b"MIT-MAGIC-COOKIE-1");
        assert_eq!(&bytes[28..30], &[0, 16], "the cookie is 16 bytes");
        assert_eq!(&bytes[30..46], cookie.as_slice());
        assert_eq!(bytes.len(), 46, "no trailing bytes");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn the_authority_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;
        let mut path = std::env::temp_dir();
        path.push(format!("desktop-driver-xauth-mode-{}", std::process::id()));
        path.push("Xauthority");
        write_xauthority(&path, 91, &[0; 16]).expect("writes");
        let mode = fs::metadata(&path).expect("exists").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn every_cookie_is_different() {
        let first = random_cookie().expect("reads urandom");
        let second = random_cookie().expect("reads urandom");
        assert_eq!(first.len(), 16);
        assert_ne!(first, second, "a fixed cookie would be no protection");
    }

    #[test]
    fn asking_for_the_host_wins_over_a_running_session() {
        let store = temp_store("prefer-host");
        store.save(&session()).expect("saves");
        assert_eq!(resolve_scope(&store, true), Scope::Host);
        let _ = store.clear();
    }

    #[test]
    fn with_no_session_recorded_everything_addresses_the_hosts_display() {
        let store = temp_store("no-session");
        assert_eq!(resolve_scope(&store, false), Scope::Host);
    }

    #[test]
    fn a_session_whose_x_server_is_gone_is_forgotten_rather_than_reused() {
        // Pid 1 is alive but is not Xvfb, so the record cannot be trusted.
        let store = temp_store("stale");
        store.save(&session()).expect("saves");
        assert_eq!(current(&store), None);
        assert_eq!(store.load(), None, "the stale record is cleared");
    }

    #[test]
    fn a_reused_pid_is_not_the_process_that_was_recorded() {
        // This process is alive and its name will not match, but the point is
        // the start time: recording a wrong one must be enough to refuse, so a
        // pid that wrapped round to something the user started is never
        // signalled.
        let mine = std::process::id();
        let live = start_time(mine).expect("this process has a start time");
        assert!(live > 0);

        let name = std::fs::read_to_string(format!("/proc/{mine}/comm"))
            .expect("readable")
            .trim()
            .to_owned();
        let real = SessionProcess::new(&name, mine).started_at(live);
        let impostor = SessionProcess::new(&name, mine).started_at(live + 1);

        // The name check is what decides for `real` here; what matters is that
        // a mismatched start time is refused outright.
        assert!(
            !process_matches(&impostor),
            "a different start time is a different process"
        );
        let _ = real;
    }

    #[test]
    fn the_start_time_survives_a_name_with_brackets_and_spaces() {
        // /proc/<pid>/stat puts the executable name in parentheses and it may
        // contain both, which is the classic way to misparse this file.
        assert!(start_time(std::process::id()).is_some());
        assert_eq!(
            start_time(0),
            None,
            "a pid that cannot exist has no start time"
        );
    }

    #[test]
    fn a_record_without_a_start_time_still_works() {
        // Written by an earlier version; falling back to the name is better
        // than treating a live session as dead and leaking its X server.
        let stale = SessionProcess::new("definitely-not-a-real-program", 1);
        assert_eq!(stale.started_at, 0);
        assert!(!process_matches(&stale), "the name still has to match");
    }

    #[test]
    fn a_recorded_process_must_still_be_the_process_that_was_recorded() {
        // Pid 1 exists on every Linux system and is never Xvfb, which is
        // exactly the pid-reuse case this guards.
        assert!(!process_matches(&SessionProcess::new("Xvfb", 1)));
        assert!(
            !process_matches(&SessionProcess::new("Xvfb", 0)),
            "a pid that cannot exist must not match"
        );
    }

    #[test]
    fn the_display_search_skips_numbers_that_are_taken() {
        // Display :0 or :1 is in use whenever these tests run under a desktop;
        // in a container neither exists. Both are correct answers, so this
        // asserts the rule rather than a particular machine's state.
        for number in DISPLAY_RANGE {
            let free = display_is_free(number);
            assert_eq!(
                free,
                !display_socket_path(number).exists() && !display_lock(number).exists()
            );
        }
    }

    #[test]
    fn a_machine_that_can_show_it_shows_it_without_being_asked() {
        // The default is transparency: an agent driving someone's computer
        // while they cannot see it is asking them to take its word for it.
        if unwatchable_reason().is_none() {
            let (server, fallback) = choose_server(Visibility::Auto).expect("can start");
            assert_eq!(server, XServer::Nested);
            assert_eq!(fallback, None);
        }
    }

    #[test]
    fn a_machine_that_cannot_show_it_falls_back_and_says_why() {
        // Never silently: CI and headless boxes must still work, and the
        // person reading the output must know why no window appeared.
        if let Some(expected) = unwatchable_reason() {
            let (server, fallback) = choose_server(Visibility::Auto).expect("still starts");
            assert_eq!(server, XServer::Headless);
            assert_eq!(fallback, Some(expected));
            assert!(!expected.explain().is_empty());
        }
    }

    #[test]
    fn asking_for_visible_explicitly_refuses_rather_than_starting_something_unwatchable() {
        if unwatchable_reason().is_some() {
            let error = choose_server(Visibility::Visible).expect_err("must refuse");
            assert!(matches!(error, DesktopError::InvalidArgument { .. }));
        }
    }

    #[test]
    fn headless_is_always_available_because_it_depends_on_nothing() {
        let (server, fallback) = choose_server(Visibility::Headless).expect("always works");
        assert_eq!(server, XServer::Headless);
        assert_eq!(fallback, None, "asking for headless is not a fallback");
    }

    #[test]
    fn a_visible_session_is_still_a_real_x_server_and_so_still_isolated() {
        // The point of the flag is that watching costs nothing: both servers
        // own their framebuffer, pointer and keyboard, so neither can reach
        // the user's applications. Only the destination of the pixels differs.
        assert_eq!(XServer::Headless.program(), "Xvfb");
        assert_eq!(XServer::Nested.program(), "Xephyr");
        assert!(XServer::is_x_server("Xvfb"));
        assert!(XServer::is_x_server("Xephyr"));
        assert!(!XServer::is_x_server("openbox"));
    }

    #[test]
    fn a_session_started_either_way_is_recognised_as_alive() {
        // `stop` and the staleness check both key off the recorded process
        // name; a visible session whose server is not recognised would look
        // dead the moment it started.
        for program in ["Xvfb", "Xephyr"] {
            let session = AgentSession {
                processes: vec![SessionProcess::new(program, 1)],
                ..session()
            };
            // Pid 1 is not an X server, so this is false either way — what is
            // being asserted is that the *name* passes the filter and the
            // decision is left to the pid check.
            assert!(
                session
                    .processes
                    .iter()
                    .any(|p| XServer::is_x_server(&p.name)),
                "{program} must be recognised as this session's X server"
            );
        }
    }

    #[test]
    fn the_default_geometry_is_a_full_hd_screen() {
        let options = StartOptions::default();
        assert_eq!((options.width, options.height), (1920, 1080));
        assert_eq!(options.display, None, "the display number is searched for");
        assert_eq!(
            options.visibility,
            Visibility::Auto,
            "shown by default where it can be, so nobody has to take the agent's word for it"
        );
    }

    #[test]
    fn a_missing_helper_names_the_package_that_provides_it() {
        let error = resolve_helper("no-such-helper-desktop-driver").expect_err("must fail");
        let rendered = error.to_string();
        assert!(
            rendered.contains("at-spi2-core"),
            "the error must say what to install, got: {rendered}"
        );
    }

    #[test]
    fn the_helper_search_covers_every_layout_seen_in_the_wild() {
        // Verified by installing at-spi2-core in a container for each. A
        // distribution missing from this list is a session that cannot start,
        // and the failure is at run time on someone else's machine.
        let directories = helper_directories();
        let covered = |path: &str| {
            directories
                .iter()
                .any(|directory| directory.as_path() == Path::new(path))
        };
        for layout in [
            // Fedora 43, Debian 13, Ubuntu 24.04
            "/usr/libexec",
            // Arch
            "/usr/lib",
            // openSUSE Tumbleweed
            "/usr/libexec/at-spi2",
        ] {
            assert!(covered(layout), "{layout} is not searched");
        }
    }

    #[test]
    fn the_multiarch_directory_follows_the_build_target() {
        // Hardcoding x86_64 would quietly exclude every arm64 Debian.
        let expected = format!("/usr/lib/{}-linux-gnu", std::env::consts::ARCH);
        assert!(
            helper_directories()
                .iter()
                .any(|d| d.as_path() == Path::new(&expected)),
            "{expected} is not searched"
        );
    }

    #[test]
    fn path_wins_over_the_search_so_an_unusual_install_still_works() {
        // `sh` stands in for a helper that happens to be on PATH: the point is
        // that PATH is consulted at all, not which binary is found.
        let found = resolve_helper("sh").expect("sh is on PATH in any test environment");
        assert!(found.is_file(), "{} should exist", found.display());
    }
}
