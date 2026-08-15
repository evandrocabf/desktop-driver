//! xdg-desktop-portal plumbing shared by the Wayland capture and input paths.
//!
//! The piece that matters here is the restore token. A portal session belongs
//! to the D-Bus connection that created it, so a one-shot CLI loses it on exit
//! — which would mean a permission dialog on every single command. Asking for
//! `PersistMode::ExplicitlyRevoked` and replaying a stored token reduces that
//! to one dialog, ever.
//!
//! The subtlety that bites implementations: **the token rotates**. Every
//! `Start` response carries a *fresh* token and invalidates the one that was
//! replayed, so failing to write the new one back silently reintroduces the
//! dialog on the next run.

use std::{fs, io::Write as _, os::unix::fs::OpenOptionsExt as _, path::PathBuf};

use desktop_core::errors::{DesktopError, Result};

/// Which portal session a stored token belongs to.
///
/// Tokens are per-selection, not per-application: the token that restores "the
/// monitor you picked" is not the one that restores "the window you picked", so
/// they cannot share a slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenKind {
    /// A RemoteDesktop session with a monitor screencast attached, used for
    /// absolute pointer positioning and full-screen capture.
    ScreenInput,
    /// A ScreenCast session bound to a single window.
    WindowCapture,
}

impl TokenKind {
    const fn filename(self) -> &'static str {
        match self {
            Self::ScreenInput => "screen-input.token",
            Self::WindowCapture => "window-capture.token",
        }
    }
}

/// `$XDG_STATE_HOME/desktop-driver`, falling back to `~/.local/state`.
///
/// State rather than cache: losing a token costs the user another approval
/// dialog, so it should survive a cache clean.
#[must_use]
pub fn state_dir() -> PathBuf {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(state).join("desktop-driver");
    }
    std::env::var_os("HOME").map_or_else(
        || std::env::temp_dir().join("desktop-driver"),
        |home| {
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("desktop-driver")
        },
    )
}

/// Where restore tokens live.
///
/// The directory is a field rather than read from the environment at each call,
/// so tests can point it somewhere scratch without mutating process-wide state
/// — which in Rust 2024 is `unsafe` and racy across parallel tests.
#[derive(Clone, Debug)]
pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    #[must_use]
    pub fn at_default_path() -> Self {
        Self::new(state_dir())
    }

    #[must_use]
    pub fn path(&self, kind: TokenKind) -> PathBuf {
        self.dir.join(kind.filename())
    }

    /// Reads a stored token, if there is one.
    #[must_use]
    pub fn load(&self, kind: TokenKind) -> Option<String> {
        let raw = fs::read_to_string(self.path(kind)).ok()?;
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }

    /// Replaces the stored token.
    ///
    /// Called after every `Start`, because the portal rotates the token each
    /// time and the previous one stops working.
    ///
    /// Created owner-only rather than created and then tightened: between those
    /// two steps the token would sit readable, and a reader that opened it in
    /// that window keeps its handle regardless of the later chmod.
    pub fn store(&self, kind: TokenKind, token: &str) -> Result<()> {
        let path = self.path(kind);
        if let Some(parent) = path.parent() {
            desktop_core::agent::create_private_dir(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| {
                DesktopError::internal(format!("cannot write {}: {error}", path.display()))
            })?;
        file.write_all(token.as_bytes()).map_err(|error| {
            DesktopError::internal(format!("cannot write {}: {error}", path.display()))
        })?;
        set_owner_only(&path);
        Ok(())
    }

    pub fn clear(&self, kind: TokenKind) -> Result<()> {
        match fs::remove_file(self.path(kind)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(DesktopError::internal(format!(
                "cannot clear token: {error}"
            ))),
        }
    }

    /// Whether any grant has been recorded, for `desktop capabilities`.
    #[must_use]
    pub fn has_any(&self) -> bool {
        self.load(TokenKind::ScreenInput).is_some() || self.load(TokenKind::WindowCapture).is_some()
    }
}

/// Whether the desktop's permission store already records a screenshot grant.
///
/// The Screenshot portal does not use a restore token — it records the answer
/// in `org.freedesktop.impl.portal.PermissionStore` under the `screenshot`
/// table, keyed by application id (empty for a non-sandboxed process). Reading
/// it lets `desktop capabilities` say *Supported* once the grant exists,
/// instead of forever warning about a dialog that will never appear again.
///
/// The zbus result type is spelled out at the call site because `Result` is
/// aliased to this crate's error type throughout the module.
#[must_use]
pub fn screenshot_permission_granted() -> bool {
    const STORE_BUS: &str = "org.freedesktop.impl.portal.PermissionStore";
    const STORE_PATH: &str = "/org/freedesktop/impl/portal/PermissionStore";

    crate::runtime::block_on(async {
        let Ok(connection) = atspi::zbus::Connection::session().await else {
            return false;
        };
        let Ok(proxy) =
            atspi::zbus::Proxy::new(&connection, STORE_BUS, STORE_PATH, STORE_BUS).await
        else {
            return false;
        };
        let reply: std::result::Result<Vec<String>, atspi::zbus::Error> = proxy
            .call("GetPermission", &("screenshot", "screenshot", ""))
            .await;
        reply.is_ok_and(|permissions| permissions.iter().any(|value| value == "yes"))
    })
}

/// Whether the RemoteDesktop grant that mouse, keyboard and scroll ride on has
/// been given.
///
/// One token rather than any token: a window-capture grant is a grant for
/// something else entirely, and answering "input needs no approval" on the
/// strength of it would send an agent into a dialog it was told was not coming.
#[must_use]
pub fn has_input_token() -> bool {
    TokenStore::at_default_path()
        .load(TokenKind::ScreenInput)
        .is_some()
}

/// A restore token is a capability to control the desktop; other users on the
/// machine have no business reading it.
#[cfg(unix)]
fn set_owner_only(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_owner_only(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> TokenStore {
        let dir = std::env::temp_dir().join(format!(
            "desktop-driver-token-test-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        TokenStore::new(dir)
    }

    #[test]
    fn a_token_round_trips_through_the_store() {
        let store = scratch("round-trip");
        assert_eq!(store.load(TokenKind::ScreenInput), None);
        store
            .store(TokenKind::ScreenInput, "abc123")
            .expect("stores");
        assert_eq!(
            store.load(TokenKind::ScreenInput),
            Some("abc123".to_owned())
        );
        store.clear(TokenKind::ScreenInput).expect("clears");
        assert_eq!(store.load(TokenKind::ScreenInput), None);
    }

    #[test]
    fn the_two_token_kinds_do_not_share_a_slot() {
        // A token that restores "the monitor you picked" cannot stand in for
        // one that restores "the window you picked".
        let store = scratch("kinds");
        store
            .store(TokenKind::ScreenInput, "screen")
            .expect("stores");
        store
            .store(TokenKind::WindowCapture, "window")
            .expect("stores");
        assert_eq!(
            store.load(TokenKind::ScreenInput),
            Some("screen".to_owned())
        );
        assert_eq!(
            store.load(TokenKind::WindowCapture),
            Some("window".to_owned())
        );
        assert_ne!(
            store.path(TokenKind::ScreenInput),
            store.path(TokenKind::WindowCapture)
        );
    }

    #[test]
    fn replacing_a_token_overwrites_rather_than_appends() {
        // The portal rotates the token on every Start; appending would leave a
        // file that parses as neither token.
        let store = scratch("rotate");
        store
            .store(TokenKind::ScreenInput, "first")
            .expect("stores");
        store
            .store(TokenKind::ScreenInput, "second")
            .expect("stores");
        assert_eq!(
            store.load(TokenKind::ScreenInput),
            Some("second".to_owned())
        );
    }

    #[test]
    fn a_blank_token_file_reads_as_absent_rather_than_as_an_empty_token() {
        let store = scratch("blank");
        store
            .store(TokenKind::ScreenInput, "   \n ")
            .expect("stores");
        assert_eq!(store.load(TokenKind::ScreenInput), None);
    }

    #[test]
    fn clearing_a_token_that_was_never_stored_is_not_an_error() {
        let store = scratch("clear-empty");
        assert!(store.clear(TokenKind::WindowCapture).is_ok());
    }

    #[test]
    fn has_any_reports_whether_setup_has_ever_completed() {
        let store = scratch("has-any");
        assert!(!store.has_any());
        store.store(TokenKind::WindowCapture, "t").expect("stores");
        assert!(store.has_any());
    }

    #[test]
    fn the_default_state_directory_is_under_xdg_state_home() {
        let dir = state_dir();
        assert!(dir.ends_with("desktop-driver"), "got {dir:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_stored_token_is_not_readable_by_other_users() {
        use std::os::unix::fs::PermissionsExt as _;
        let store = scratch("perms");
        store
            .store(TokenKind::ScreenInput, "secret")
            .expect("stores");
        let mode = fs::metadata(store.path(TokenKind::ScreenInput))
            .expect("exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "token must not be group or world readable");
    }
}
