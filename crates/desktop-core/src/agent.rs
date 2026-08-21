//! A display of the agent's own.
//!
//! An agent and a person sharing one computer are competing for three
//! singletons: there is one keyboard focus, one pointer, and one screen. Every
//! failure that follows from that is the same failure — the agent aimed at
//! something, the human moved, and the keystrokes landed somewhere else — and
//! no amount of care on the agent's side fixes it, because the race is real.
//!
//! Element-addressed actions dodge two of the three: an accessibility action
//! needs no pointer and no focus. Nothing dodges the screen. A screenshot of a
//! shared desktop shows the human's windows, which is both a privacy leak and a
//! stream of irrelevant pixels.
//!
//! So the agent gets its own display. This module is the record of one — what
//! it is and how to reach it — written where the next process can find it,
//! because every `desktop` command is a fresh process.
//!
//! Creating a session is platform work and lives in the platform adapter. The
//! record is plain data, so the CLI can describe a session on any machine.

use std::{
    fs,
    io::Write as _,
    os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::errors::{DesktopError, Result};

/// Each toolkit's own way of being told to use X11.
///
/// Removing `WAYLAND_DISPLAY` is already enough for every one of these. They
/// are set anyway because the cost is nothing and the failure they prevent is
/// invisible: a toolkit that quietly picks the user's compositor produces a
/// window on the wrong screen and no error anywhere.
const X11_ONLY: [(&str, &str); 6] = [
    ("XDG_SESSION_TYPE", "x11"),
    ("GDK_BACKEND", "x11"),
    ("QT_QPA_PLATFORM", "xcb"),
    ("CLUTTER_BACKEND", "x11"),
    ("SDL_VIDEODRIVER", "x11"),
    ("ELECTRON_OZONE_PLATFORM_HINT", "x11"),
];

/// A process the session owns.
///
/// Recorded so a later `desktop session stop` can end exactly what was started
/// and nothing else — matching on process names would be a good way to kill a
/// user's own X server.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct SessionProcess {
    /// What it is, for `desktop session status`.
    pub name: String,
    pub pid: u32,
    /// When the process started, in kernel clock ticks since boot.
    ///
    /// A pid on its own is not an identity: the kernel reuses numbers, so a
    /// record written before a wrap can name a process the user started since.
    /// `(pid, start time)` is unique, and this is what stops
    /// `desktop session stop` from signalling something it did not start.
    #[serde(default)]
    pub started_at: u64,
    /// Whether this is an application launched onto the display rather than
    /// one of the services that implements the display itself.
    #[serde(default)]
    pub application: bool,
}

impl SessionProcess {
    #[must_use]
    pub fn new(name: impl Into<String>, pid: u32) -> Self {
        Self {
            name: name.into(),
            pid,
            started_at: 0,
            application: false,
        }
    }

    #[must_use]
    pub fn started_at(mut self, ticks: u64) -> Self {
        self.started_at = ticks;
        self
    }

    #[must_use]
    pub fn application(mut self) -> Self {
        self.application = true;
        self
    }
}

/// Everything needed to reach the agent's display and the services on it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct AgentSession {
    /// The persistent browser workspace this runtime belongs to.
    #[serde(default = "default_session_name")]
    pub name: String,
    /// The X display name, e.g. `:97`.
    pub display: String,
    pub width: u32,
    pub height: u32,
    /// The session's private D-Bus, which is what keeps its accessibility tree
    /// separate from the user's.
    pub dbus_address: String,
    /// The accessibility bus reached through that D-Bus.
    pub a11y_address: String,
    /// An `Xauthority` file holding [`cookie`](Self::cookie), for child
    /// processes that discover credentials the ordinary way.
    pub xauthority: PathBuf,
    /// The display's `MIT-MAGIC-COOKIE-1`, hex-encoded.
    ///
    /// Without one, every local user on the machine could read the agent's
    /// screen and inject keystrokes into it — a display with no authority is
    /// more exposed than the user's real one, not less. Never rendered: see
    /// [`redacted`](Self::redacted).
    pub cookie: String,
    /// Whether this session is rendered into a window the user can watch.
    #[serde(default)]
    pub visible: bool,
    /// A home directory of the session's own, or `None` when it shares the
    /// user's.
    ///
    /// A separate display is not separate enough. Firefox, Chrome and VS Code
    /// are all single-instance and coordinate through a lock file in the
    /// profile, so an agent launching one with the user's `HOME` either takes
    /// over the user's window or — worse — holds the lock and leaves the user
    /// unable to start their own browser at all. The same `HOME` also means
    /// the agent arrives logged in to everything the user is logged in to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<PathBuf>,
    pub processes: Vec<SessionProcess>,
}

impl AgentSession {
    /// The display number, e.g. `97` for `:97`.
    #[must_use]
    pub fn display_number(&self) -> Option<u32> {
        self.display
            .trim_start_matches(':')
            .split('.')
            .next()?
            .parse()
            .ok()
    }

    /// The cookie as bytes, for an X11 connection that supplies its own
    /// credentials rather than reading `XAUTHORITY` out of the environment.
    pub fn cookie_bytes(&self) -> Result<Vec<u8>> {
        decode_hex(&self.cookie)
            .ok_or_else(|| DesktopError::internal("the session's cookie is not valid hex"))
    }

    /// The environment a process must inherit to live on this display.
    ///
    /// Setting `DISPLAY` is not enough, and the way it fails is nasty. A
    /// toolkit that finds `WAYLAND_DISPLAY` in its environment prefers Wayland
    /// — GTK4 and Qt6 both do — so an application launched with only `DISPLAY`
    /// changed opens its window on the user's compositor while its
    /// accessibility tree registers on the agent's private bus. Every reading
    /// command then reports a healthy, correct-looking tree for a window
    /// sitting on somebody else's screen, collecting their keystrokes. The
    /// display is authoritative only once the Wayland handles are gone, which
    /// is why [`removed_environment`](Self::removed_environment) exists and why
    /// each toolkit is additionally pinned to X11 by name.
    ///
    /// `AT_SPI_BUS_ADDRESS` is set explicitly rather than left to D-Bus
    /// activation: activation of `org.a11y.Bus` is what SELinux refuses on a
    /// hand-started bus, and the address is already known here anyway.
    ///
    /// `ACCESSIBILITY_ENABLED` is what Chromium and Electron watch. They build
    /// no accessibility tree at all until something asks for one, which
    /// presents as an application with a window and no contents.
    #[must_use]
    pub fn environment(&self) -> Vec<(String, String)> {
        let mut environment = vec![
            ("DISPLAY".to_owned(), self.display.clone()),
            (
                "DBUS_SESSION_BUS_ADDRESS".to_owned(),
                self.dbus_address.clone(),
            ),
            (
                "XAUTHORITY".to_owned(),
                self.xauthority.display().to_string(),
            ),
            ("AT_SPI_BUS_ADDRESS".to_owned(), self.a11y_address.clone()),
            ("ACCESSIBILITY_ENABLED".to_owned(), "1".to_owned()),
            ("GTK_A11Y".to_owned(), "atspi".to_owned()),
        ];
        environment.extend(
            X11_ONLY
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
        );

        if let Some(home) = &self.home {
            let home = home.display().to_string();
            environment.push(("HOME".to_owned(), home.clone()));
            for (key, leaf) in [
                ("XDG_CONFIG_HOME", ".config"),
                ("XDG_DATA_HOME", ".local/share"),
                ("XDG_STATE_HOME", ".local/state"),
                ("XDG_CACHE_HOME", ".cache"),
            ] {
                environment.push((key.to_owned(), format!("{home}/{leaf}")));
            }
        }

        environment
    }

    /// Variables a child must *not* inherit.
    ///
    /// Unsetting `WAYLAND_DISPLAY` is what actually decides which screen an
    /// application appears on; everything in `X11_ONLY` is a second lock on
    /// the same door.
    #[must_use]
    pub const fn removed_environment() -> &'static [&'static str] {
        &["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
    }

    /// A copy safe to print.
    #[must_use]
    pub fn redacted(&self) -> Self {
        Self {
            cookie: "<redacted>".to_owned(),
            ..self.clone()
        }
    }
}

/// Whether the agent's screen is one the user can see.
///
/// Watching costs nothing in isolation — a nested X server has its own
/// framebuffer, pointer and keyboard exactly as a headless one does — so the
/// default is to show it. An agent driving someone's computer while they cannot
/// see what it is doing is asking them to take its word for it, and there is no
/// reason to.
///
/// It is not always possible: a nested server needs a desktop to open its
/// window on. That is what [`Auto`](Self::Auto) is for.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Visibility {
    /// Show it when this machine can, and say so plainly when it cannot.
    #[default]
    Auto,
    /// Show it, and fail rather than start something nobody can see.
    Visible,
    /// Do not show it.
    Headless,
}

/// Why a session the user asked to watch ended up invisible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Unwatchable {
    /// There is no desktop to open a window on.
    NoDisplay,
    /// The nested X server is not installed.
    XephyrMissing,
}

impl Unwatchable {
    /// The reason, as a clause that follows "you cannot watch it: ".
    ///
    /// Deliberately short and without its own framing — the caller supplies
    /// that, and a reason that restates it reads as though the program is
    /// arguing with itself.
    #[must_use]
    pub const fn explain(self) -> &'static str {
        match self {
            Self::NoDisplay => "there is no desktop to open a window on (DISPLAY is not set)",
            Self::XephyrMissing => "Xephyr is not installed",
        }
    }

    /// What to do about it, when there is anything to do.
    #[must_use]
    pub const fn remedy(self) -> Option<&'static str> {
        match self {
            Self::NoDisplay => None,
            Self::XephyrMissing => Some("`desktop doctor` names the package to install"),
        }
    }
}

/// What kind of display to build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartOptions {
    /// The persistent browser workspace to attach this runtime to.
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Use this display number instead of searching for a free one.
    pub display: Option<u32>,
    /// Whether the user should be able to watch.
    pub visibility: Visibility,
    /// Let the session use the user's own home directory.
    ///
    /// Off by default. Sharing it means sharing every application profile,
    /// which for a single-instance application means the agent and the user
    /// contend for one lock — and the one who loses cannot start the
    /// application at all.
    pub share_home: bool,
}

impl Default for StartOptions {
    fn default() -> Self {
        Self {
            name: default_session_name(),
            width: 1920,
            height: 1080,
            display: None,
            visibility: Visibility::Auto,
            share_home: false,
        }
    }
}

/// The backwards-compatible workspace used when no name is supplied.
#[must_use]
pub fn default_session_name() -> String {
    "default".to_owned()
}

/// Durable identity and browser state, separate from the display runtime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct SessionProfile {
    pub name: String,
    pub home: PathBuf,
}

/// Persistent browser workspaces.
///
/// Runtime credentials and pids never enter this store. They remain under
/// `XDG_RUNTIME_DIR`; this store contains only the private home in which a
/// browser keeps cookies, local storage and saved logins.
#[derive(Clone, Debug)]
pub struct SessionProfileStore {
    root: PathBuf,
}

impl SessionProfileStore {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn default_root() -> PathBuf {
        persistent_data_root().join("sessions")
    }

    #[must_use]
    pub fn at_default_path() -> Self {
        Self::new(Self::default_root())
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create(&self, name: &str) -> Result<SessionProfile> {
        validate_session_name(name)?;
        if self.load(name)?.is_some() {
            return Err(DesktopError::invalid_argument(format!(
                "session {name:?} already exists"
            )));
        }
        self.create_new(name)
    }

    pub fn ensure(&self, name: &str) -> Result<SessionProfile> {
        validate_session_name(name)?;
        if let Some(profile) = self.load(name)? {
            return Ok(profile);
        }
        self.create_new(name)
    }

    pub fn load(&self, name: &str) -> Result<Option<SessionProfile>> {
        validate_session_name(name)?;
        let path = self.manifest_path(name);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(DesktopError::internal(format!(
                    "cannot read {}: {error}",
                    path.display()
                )));
            }
        };
        let profile: SessionProfile = serde_json::from_slice(&bytes).map_err(|error| {
            DesktopError::internal(format!("cannot decode {}: {error}", path.display()))
        })?;
        let expected_home = self.profile_dir(name).join("home");
        if profile.name != name || profile.home != expected_home {
            return Err(DesktopError::internal(format!(
                "{} points outside session {name:?}",
                path.display()
            )));
        }
        Ok(Some(profile))
    }

    pub fn list(&self) -> Result<Vec<SessionProfile>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(DesktopError::internal(format!(
                    "cannot list {}: {error}",
                    self.root.display()
                )));
            }
        };
        let mut profiles = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                DesktopError::internal(format!("cannot list {}: {error}", self.root.display()))
            })?;
            let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            if validate_session_name(&name).is_err() {
                continue;
            }
            if let Some(profile) = self.load(&name)? {
                profiles.push(profile);
            }
        }
        profiles.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(profiles)
    }

    pub fn delete(&self, name: &str) -> Result<Option<SessionProfile>> {
        let Some(profile) = self.load(name)? else {
            return Ok(None);
        };
        let directory = self.profile_dir(name);
        fs::remove_dir_all(&directory).map_err(|error| {
            DesktopError::internal(format!("cannot remove {}: {error}", directory.display()))
        })?;
        Ok(Some(profile))
    }

    /// Creates one workspace and migrates the first release's global home
    /// into `default`, so updating never silently logs an existing user out.
    fn create_new(&self, name: &str) -> Result<SessionProfile> {
        let directory = self.profile_dir(name);
        create_private_dir(&directory)?;
        let home = directory.join("home");

        let legacy = self.legacy_home();
        if name == "default" && legacy.exists() && !home.exists() {
            fs::rename(&legacy, &home).map_err(|error| {
                DesktopError::internal(format!(
                    "cannot migrate {} to {}: {error}",
                    legacy.display(),
                    home.display()
                ))
            })?;
        }
        create_private_dir(&home)?;

        let profile = SessionProfile {
            name: name.to_owned(),
            home,
        };
        let encoded = serde_json::to_vec_pretty(&profile)
            .map_err(|error| DesktopError::internal(format!("cannot encode session: {error}")))?;
        let path = self.manifest_path(name);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| {
                DesktopError::internal(format!("cannot write {}: {error}", path.display()))
            })?;
        file.write_all(&encoded)
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                DesktopError::internal(format!("cannot write {}: {error}", path.display()))
            })?;
        Ok(profile)
    }

    fn profile_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn manifest_path(&self, name: &str) -> PathBuf {
        self.profile_dir(name).join("profile.json")
    }

    fn legacy_home(&self) -> PathBuf {
        self.root.parent().unwrap_or(&self.root).join("home")
    }
}

/// Session names become directory names, so accept a deliberately small set.
pub fn validate_session_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && matches!(byte, b'-' | b'_'))
        });
    if valid {
        Ok(())
    } else {
        Err(DesktopError::invalid_argument(
            "a session name must be 1-64 ASCII letters or digits, with '-' and '_' allowed after the first character",
        ))
    }
}

fn persistent_data_root() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("desktop-driver")
}

/// Creates a directory only its owner can enter.
///
/// `create_dir_all` obeys the umask, which on several distributions yields
/// 0755 — fine for a cache, wrong for anything holding an application profile
/// or the text of somebody's screen. The mode is set afterwards as well as at
/// creation so a directory made by an earlier version is tightened rather than
/// left as it was found.
pub fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| {
        DesktopError::internal(format!("cannot create {}: {error}", path.display()))
    })?;
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        DesktopError::internal(format!("cannot secure {}: {error}", path.display()))
    })
}

/// Where a session's private home lives.
///
/// Under `XDG_DATA_HOME` rather than the runtime directory so it survives a
/// logout: an agent that logged in to something should still be logged in
/// tomorrow, the same as the user's own profile would be.
#[must_use]
pub fn default_agent_home() -> PathBuf {
    SessionProfileStore::default_root()
        .join("default")
        .join("home")
}

/// Where the current session record is kept.
#[derive(Clone, Debug)]
pub struct AgentSessionStore {
    path: PathBuf,
}

impl AgentSessionStore {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// `$XDG_RUNTIME_DIR/desktop-driver/agent-session.json`.
    ///
    /// The runtime directory is right for this in a way it is not for most
    /// state: a session cannot outlive the login it was started from, and the
    /// directory is cleared at logout, so a stale record cannot survive a
    /// reboot and send the next run at a display that no longer exists.
    #[must_use]
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("desktop-driver").join("agent-session.json")
    }

    #[must_use]
    pub fn at_default_path() -> Self {
        Self::new(Self::default_path())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The recorded session, or `None` when there is none.
    ///
    /// A corrupt record reads as "no session" rather than as an error: the
    /// caller's next move either way is to start one.
    #[must_use]
    pub fn load(&self) -> Option<AgentSession> {
        let bytes = fs::read(&self.path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Writes the record readable only by its owner, because it holds the
    /// display's cookie.
    pub fn save(&self, session: &AgentSession) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            create_private_dir(parent)?;
        }

        let encoded = serde_json::to_vec_pretty(session)
            .map_err(|error| DesktopError::internal(format!("cannot encode session: {error}")))?;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&self.path)
            .map_err(|error| {
                DesktopError::internal(format!("cannot write {}: {error}", self.path.display()))
            })?;
        file.write_all(&encoded)
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                DesktopError::internal(format!("cannot write {}: {error}", self.path.display()))
            })
    }

    pub fn clear(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(DesktopError::internal(format!(
                "cannot remove {}: {error}",
                self.path.display()
            ))),
        }
    }
}

/// Starting, describing and ending the agent's display.
///
/// A fifth port, kept out of [`Ports`](crate::ports::Ports) because it is the
/// one thing here that does not talk to a desktop — it starts processes to
/// create one. It is also outside the policy gates on purpose: nothing it does
/// can reach the user's screen, so `--read-only` has nothing to protect. The
/// operation that *would* need a gate is acting on the resulting display, and
/// that goes through the ordinary ports like everything else.
pub trait SessionHost {
    /// Whether this platform can give an agent a display at all.
    ///
    /// Asked before advising anyone to start one: telling a macOS user to run
    /// `desktop session start` when the answer will always be a refusal is
    /// worse than saying nothing.
    fn supported(&self) -> bool {
        true
    }

    /// Why a session on this machine would be one nobody can watch, if so.
    ///
    /// Asked after starting a headless session, so the user is told rather than
    /// left wondering why no window appeared.
    fn unwatchable(&self) -> Option<&'static str> {
        None
    }

    fn create(&self, name: &str) -> Result<SessionProfile>;

    fn list(&self) -> Result<Vec<SessionProfile>>;

    fn delete(&self, name: &str) -> Result<Option<SessionProfile>>;

    fn start(&self, options: StartOptions) -> Result<AgentSession>;

    /// The running session, or `None`.
    fn status(&self) -> Option<AgentSession>;

    /// Ends the session, returning what was stopped.
    fn stop(&self) -> Result<Option<AgentSession>>;

    /// Launches a program onto the running session, returning its pid.
    fn launch(&self, program: &str, args: &[String]) -> Result<u32>;
}

/// A platform with no notion of a second display.
///
/// macOS has no equivalent: the window server belongs to the login session and
/// there is no supported way to create another for an agent to work in. Saying
/// so plainly beats shipping something that half-works.
pub struct NoSessionHost {
    platform: crate::models::backend::Platform,
    display_server: crate::models::backend::DisplayServer,
    desktop_environment: crate::models::backend::DesktopEnvironment,
}

impl NoSessionHost {
    #[must_use]
    pub const fn new(
        platform: crate::models::backend::Platform,
        display_server: crate::models::backend::DisplayServer,
        desktop_environment: crate::models::backend::DesktopEnvironment,
    ) -> Self {
        Self {
            platform,
            display_server,
            desktop_environment,
        }
    }

    fn refuse<T>(&self) -> Result<T> {
        Err(DesktopError::UnsupportedCapability {
            capability: crate::models::capability::Capability::AgentSession,
            backend: crate::models::backend::Backend::None,
            platform: self.platform,
            display_server: self.display_server,
            desktop_environment: self.desktop_environment,
        })
    }
}

impl SessionHost for NoSessionHost {
    fn supported(&self) -> bool {
        false
    }

    fn create(&self, _name: &str) -> Result<SessionProfile> {
        self.refuse()
    }

    fn list(&self) -> Result<Vec<SessionProfile>> {
        self.refuse()
    }

    fn delete(&self, _name: &str) -> Result<Option<SessionProfile>> {
        self.refuse()
    }

    fn start(&self, _options: StartOptions) -> Result<AgentSession> {
        self.refuse()
    }

    fn status(&self) -> Option<AgentSession> {
        None
    }

    fn stop(&self) -> Result<Option<AgentSession>> {
        self.refuse()
    }

    fn launch(&self, _program: &str, _args: &[String]) -> Result<u32> {
        self.refuse()
    }
}

/// Lowercase hex, for the cookie.
#[must_use]
pub fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[must_use]
pub fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect()
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
            xauthority: PathBuf::from("/run/user/1000/desktop-driver/Xauthority"),
            cookie: "00112233445566778899aabbccddeeff".to_owned(),
            visible: true,
            home: Some(PathBuf::from("/tmp/agent-home")),
            processes: vec![SessionProcess::new("Xvfb", 1234)],
        }
    }

    fn temp_store(tag: &str) -> AgentSessionStore {
        let mut path = std::env::temp_dir();
        path.push(format!("desktop-driver-agent-{tag}-{}", std::process::id()));
        path.push("agent-session.json");
        let store = AgentSessionStore::new(path);
        let _ = store.clear();
        store
    }

    fn temp_profiles(tag: &str) -> SessionProfileStore {
        let mut root = std::env::temp_dir();
        root.push(format!(
            "desktop-driver-profiles-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        SessionProfileStore::new(root.join("sessions"))
    }

    #[test]
    fn named_sessions_keep_browser_homes_separate_and_persistent() {
        let store = temp_profiles("isolated");
        let github = store.create("github").expect("creates github");
        let customer = store.create("customer-a").expect("creates customer");

        assert_ne!(github.home, customer.home);
        assert!(github.home.is_dir());
        assert_eq!(store.ensure("github").expect("reloads"), github);
        assert_eq!(store.list().expect("lists"), vec![customer, github]);

        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn the_legacy_global_home_becomes_default_without_losing_login_state() {
        let store = temp_profiles("legacy");
        let legacy = store.legacy_home();
        create_private_dir(&legacy).expect("creates legacy home");
        fs::write(legacy.join("cookie"), b"still-signed-in").expect("writes login state");

        let profile = store.ensure("default").expect("migrates");

        assert!(!legacy.exists());
        assert_eq!(
            fs::read(profile.home.join("cookie")).expect("preserved"),
            b"still-signed-in"
        );
        let _ = fs::remove_dir_all(store.root().parent().expect("has data root"));
    }

    #[test]
    fn a_profile_and_its_home_are_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let store = temp_profiles("permissions");
        let profile = store.create("bank").expect("creates");
        let home_mode = fs::metadata(&profile.home)
            .expect("home exists")
            .permissions()
            .mode();
        let manifest_mode = fs::metadata(store.manifest_path("bank"))
            .expect("manifest exists")
            .permissions()
            .mode();
        assert_eq!(home_mode & 0o777, 0o700);
        assert_eq!(manifest_mode & 0o777, 0o600);
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn deleting_a_session_removes_its_saved_browser_state() {
        let store = temp_profiles("delete");
        let profile = store.create("temporary").expect("creates");
        fs::write(profile.home.join("cookie"), b"secret").expect("writes state");

        assert_eq!(
            store.delete("temporary").expect("deletes"),
            Some(profile.clone())
        );
        assert!(!profile.home.exists());
        assert_eq!(store.delete("temporary").expect("already gone"), None);
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn session_names_cannot_escape_the_profile_root() {
        let store = temp_profiles("names");
        for name in ["", ".hidden", "../host", "a/b", "two words"] {
            assert!(store.create(name).is_err(), "{name:?} must be refused");
        }
        assert!(!store.root().exists());
    }

    #[test]
    fn a_tampered_manifest_cannot_redirect_the_browser_home() {
        let store = temp_profiles("tampered");
        store.create("github").expect("creates");
        fs::write(
            store.manifest_path("github"),
            br#"{"name":"github","home":"/tmp/not-this-session"}"#,
        )
        .expect("tampers");

        assert!(store.load("github").is_err());
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn a_private_directory_is_tightened_even_when_it_already_existed() {
        // Sessions created by an earlier version left 0755 behind; finding one
        // should fix it rather than trust it.
        use std::os::unix::fs::PermissionsExt as _;
        let mut path = std::env::temp_dir();
        path.push(format!("desktop-driver-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("creates");
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("loosens");

        create_private_dir(&path).expect("secures");
        let mode = fs::metadata(&path).expect("exists").permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "got {:o}", mode & 0o777);
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    fn a_session_record_survives_the_round_trip_that_spans_two_processes() {
        let store = temp_store("round-trip");
        store.save(&session()).expect("saves");
        assert_eq!(store.load(), Some(session()));
        let _ = store.clear();
    }

    #[test]
    fn the_record_is_readable_only_by_its_owner_because_it_holds_the_cookie() {
        use std::os::unix::fs::PermissionsExt as _;
        let store = temp_store("permissions");
        store.save(&session()).expect("saves");
        let mode = fs::metadata(store.path())
            .expect("exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
        let _ = store.clear();
    }

    #[test]
    fn no_session_and_a_corrupt_record_are_both_simply_no_session() {
        // The caller's next move is the same either way: start one.
        let store = temp_store("corrupt");
        assert_eq!(store.load(), None);

        fs::create_dir_all(store.path().parent().expect("has parent")).expect("creates dir");
        fs::write(store.path(), b"{not json").expect("writes");
        assert_eq!(store.load(), None);
        let _ = store.clear();
    }

    #[test]
    fn the_display_number_is_read_back_out_of_the_display_name() {
        assert_eq!(session().display_number(), Some(97));
        assert_eq!(
            AgentSession {
                display: ":12.0".to_owned(),
                ..session()
            }
            .display_number(),
            Some(12)
        );
    }

    #[test]
    fn the_environment_sends_a_child_to_the_agents_display_and_nowhere_else() {
        let environment = session().environment();
        let value = |key: &str| {
            environment
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(value("DISPLAY"), Some(":97".to_owned()));
        assert_eq!(
            value("DBUS_SESSION_BUS_ADDRESS"),
            Some("unix:path=/tmp/dbus-abc".to_owned())
        );
        assert_eq!(
            value("AT_SPI_BUS_ADDRESS"),
            Some("unix:path=/run/user/1000/at-spi/bus_97".to_owned())
        );
    }

    #[test]
    fn a_private_home_redirects_every_directory_an_application_stores_a_profile_in() {
        let environment = session().environment();
        let value = |key: &str| {
            environment
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(value("HOME"), Some("/tmp/agent-home"));
        assert_eq!(value("XDG_CONFIG_HOME"), Some("/tmp/agent-home/.config"));
        assert_eq!(value("XDG_DATA_HOME"), Some("/tmp/agent-home/.local/share"));
        assert_eq!(
            value("XDG_STATE_HOME"),
            Some("/tmp/agent-home/.local/state")
        );
        assert_eq!(value("XDG_CACHE_HOME"), Some("/tmp/agent-home/.cache"));
    }

    #[test]
    fn the_runtime_directory_is_never_redirected() {
        // The accessibility socket lives there and has to stay the real
        // per-user directory the desktop already created with mode 0700.
        assert!(
            !session()
                .environment()
                .iter()
                .any(|(key, _)| key == "XDG_RUNTIME_DIR")
        );
    }

    #[test]
    fn sharing_the_users_home_leaves_their_environment_untouched() {
        let shared = AgentSession {
            home: None,
            ..session()
        };
        for key in ["HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"] {
            assert!(
                !shared.environment().iter().any(|(name, _)| name == key),
                "{key} must be inherited unchanged when the home is shared"
            );
        }
    }

    #[test]
    fn a_session_defaults_to_a_home_of_its_own() {
        // Sharing one means the agent and the user contend for every
        // single-instance application's profile lock, and the loser cannot
        // start the application at all.
        assert!(!StartOptions::default().share_home);
    }

    #[test]
    fn a_child_never_inherits_the_users_wayland_socket() {
        // Observed for real: with only DISPLAY changed, gnome-calculator
        // opened on the user's Wayland compositor while registering on the
        // agent's private accessibility bus. Every read looked healthy and
        // the window was on someone else's screen, taking their keystrokes.
        // WAYLAND_DISPLAY is what decides, so it has to go.
        assert!(
            AgentSession::removed_environment().contains(&"WAYLAND_DISPLAY"),
            "a session that leaves WAYLAND_DISPLAY set is not isolation"
        );
        assert!(AgentSession::removed_environment().contains(&"WAYLAND_SOCKET"));
    }

    #[test]
    fn every_toolkit_is_pinned_to_the_agents_x_display_by_name() {
        let environment = session().environment();
        let value = |key: &str| {
            environment
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(value("GDK_BACKEND"), Some("x11"), "GTK 3 and 4");
        assert_eq!(value("QT_QPA_PLATFORM"), Some("xcb"), "Qt 5 and 6");
        assert_eq!(value("XDG_SESSION_TYPE"), Some("x11"));
        assert_eq!(value("ELECTRON_OZONE_PLATFORM_HINT"), Some("x11"));
    }

    #[test]
    fn nothing_is_both_set_and_removed() {
        let environment = session().environment();
        for removed in AgentSession::removed_environment() {
            assert!(
                !environment.iter().any(|(key, _)| key == removed),
                "{removed} is both set and unset, so the result depends on order"
            );
        }
    }

    #[test]
    fn chromium_style_applications_are_told_to_build_an_accessibility_tree() {
        // Without this they show a window with no contents, which reads as a
        // broken tool rather than a lazily-initialised toolkit.
        assert!(
            session()
                .environment()
                .contains(&("ACCESSIBILITY_ENABLED".to_owned(), "1".to_owned()))
        );
    }

    #[test]
    fn the_cookie_is_never_part_of_a_rendered_session() {
        let redacted = session().redacted();
        assert_eq!(redacted.cookie, "<redacted>");
        assert!(!redacted.cookie.contains("0011"));

        let json = serde_json::to_string(&redacted).expect("serializes");
        assert!(
            !json.contains("00112233"),
            "the cookie leaked into JSON: {json}"
        );
    }

    #[test]
    fn a_cookie_round_trips_through_hex() {
        let bytes = vec![0x00, 0x11, 0xff, 0xa0];
        let text = encode_hex(&bytes);
        assert_eq!(text, "0011ffa0");
        assert_eq!(decode_hex(&text), Some(bytes));
    }

    #[test]
    fn malformed_hex_is_rejected_rather_than_silently_truncated() {
        assert_eq!(decode_hex("abc"), None, "odd length");
        assert_eq!(decode_hex("zz"), None, "not hex");
        assert!(
            AgentSession {
                cookie: "nonsense".to_owned(),
                ..session()
            }
            .cookie_bytes()
            .is_err()
        );
    }
}
