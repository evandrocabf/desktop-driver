//! Carrying a snapshot from one process to the next.
//!
//! `desktop snapshot` prints `[42] button "Save"` and exits. `desktop click
//! --element 42` is a fresh process with no memory of it. The bridge is a small
//! JSON file in the runtime directory holding each element's
//! [`ElementPath`] — never its coordinates,
//! so a moved window can never be clicked at a stale position.

use std::{
    fs,
    io::Write as _,
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
};

use crate::{
    errors::{DesktopError, Result},
    models::{ids::ElementId, path::ElementPath, snapshot::Snapshot},
};

/// Where the last snapshot is kept.
#[derive(Clone, Debug)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// `$XDG_RUNTIME_DIR/desktop-driver/snapshot.json`, falling back to the
    /// temp directory. The runtime directory is preferred because it is
    /// per-user, tmpfs-backed and cleared at logout — a snapshot is session
    /// state, not something to leave lying on disk.
    #[must_use]
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("desktop-driver").join("snapshot.json")
    }

    #[must_use]
    pub fn at_default_path() -> Self {
        Self::new(Self::default_path())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(&self, snapshot: &Snapshot) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            crate::agent::create_private_dir(parent)?;
        }

        let encoded = serde_json::to_vec(snapshot)
            .map_err(|error| DesktopError::internal(format!("cannot encode snapshot: {error}")))?;

        let temporary = self.path.with_extension("json.tmp");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| {
                DesktopError::internal(format!("cannot write {}: {error}", temporary.display()))
            })?;
        file.write_all(&encoded)
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                DesktopError::internal(format!("cannot write {}: {error}", temporary.display()))
            })?;
        drop(file);

        fs::rename(&temporary, &self.path).map_err(|error| {
            DesktopError::internal(format!("cannot publish {}: {error}", self.path.display()))
        })
    }

    pub fn load(&self) -> Result<Snapshot> {
        let bytes = fs::read(&self.path).map_err(|_| DesktopError::NoSnapshot)?;
        serde_json::from_slice(&bytes).map_err(|_| DesktopError::NoSnapshot)
    }

    /// The path recorded for an element id in the last snapshot.
    pub fn lookup(&self, id: ElementId) -> Result<ElementPath> {
        let snapshot = self.load()?;
        let element = snapshot
            .find(id)
            .ok_or_else(|| DesktopError::ElementNotFound {
                selector: format!("element {id}"),
            })?;
        element.path.clone().ok_or_else(|| {
            DesktopError::internal(format!("element {id} was stored without a path"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        app::{AppKey, WindowKey},
        element::Element,
        geometry::{Bounds, CoordinateSpace},
        ids::ProcessId,
        path::PathStep,
        role::Role,
    };

    fn temp_store(tag: &str) -> SessionStore {
        let mut path = std::env::temp_dir();
        path.push(format!("desktop-driver-test-{tag}-{}", std::process::id()));
        path.push("snapshot.json");
        let store = SessionStore::new(path);
        let _ = store.clear();
        store
    }

    fn snapshot_with_element() -> Snapshot {
        let path = ElementPath::new(
            AppKey::new(ProcessId::new(7), "Fixture"),
            WindowKey::new(Some("Main"), 0),
            vec![PathStep::new(Role::Button, Some("Save"), 0)],
        );
        Snapshot {
            app: Some("Fixture".to_owned()),
            window: Some("Main".to_owned()),
            coordinate_space: CoordinateSpace::primary_screen(),
            elements: vec![Element {
                id: ElementId::new(42),
                role: Role::Button,
                name: Some("Save".to_owned()),
                description: None,
                value: None,
                enabled: true,
                focused: false,
                selected: false,
                redacted: false,
                bounds: Some(Bounds::new(1100, 700, 80, 32)),
                actions: Vec::new(),
                path: Some(path),
            }],
            truncated: false,
            visited_nodes: 3,
        }
    }

    #[test]
    fn a_snapshot_is_readable_only_by_the_user_who_took_it() {
        // It holds the text of somebody's screen, and when there is no runtime
        // directory it lands in the shared temporary one.
        use std::os::unix::fs::PermissionsExt as _;
        let store = temp_store("mode");
        store.save(&snapshot_with_element()).expect("saves");

        let file = fs::metadata(store.path())
            .expect("exists")
            .permissions()
            .mode();
        assert_eq!(file & 0o777, 0o600, "file: {:o}", file & 0o777);

        let dir = fs::metadata(store.path().parent().expect("has parent"))
            .expect("exists")
            .permissions()
            .mode();
        assert_eq!(dir & 0o777, 0o700, "directory: {:o}", dir & 0o777);
        let _ = store.clear();
    }

    #[test]
    fn a_snapshot_survives_the_round_trip_that_spans_two_processes() {
        let store = temp_store("round-trip");
        let original = snapshot_with_element();
        store.save(&original).expect("saves");
        let loaded = store.load().expect("loads");
        assert_eq!(loaded, original);
        let _ = store.clear();
    }

    #[test]
    fn looking_up_an_element_returns_the_path_not_the_coordinates() {
        // The whole point: acting re-resolves the path against the live tree
        // rather than trusting remembered geometry.
        let store = temp_store("lookup");
        store.save(&snapshot_with_element()).expect("saves");
        let path = store.lookup(ElementId::new(42)).expect("looks up");
        assert_eq!(path.steps.len(), 1);
        assert_eq!(path.steps[0].role, Role::Button);
        let _ = store.clear();
    }

    #[test]
    fn acting_before_any_snapshot_reports_no_snapshot_rather_than_a_file_error() {
        let store = temp_store("missing");
        assert_eq!(store.load().unwrap_err(), DesktopError::NoSnapshot);
        assert_eq!(
            store.lookup(ElementId::new(1)).unwrap_err(),
            DesktopError::NoSnapshot
        );
    }

    #[test]
    fn an_unknown_element_id_is_distinguished_from_a_missing_snapshot() {
        let store = temp_store("unknown-id");
        store.save(&snapshot_with_element()).expect("saves");
        let error = store.lookup(ElementId::new(999)).expect_err("must fail");
        assert!(matches!(error, DesktopError::ElementNotFound { .. }));
        let _ = store.clear();
    }

    #[test]
    fn a_corrupt_snapshot_file_reads_as_no_snapshot_rather_than_panicking() {
        let store = temp_store("corrupt");
        fs::create_dir_all(store.path().parent().expect("has parent")).expect("creates dir");
        fs::write(store.path(), b"{not json").expect("writes");
        assert_eq!(store.load().unwrap_err(), DesktopError::NoSnapshot);
        let _ = store.clear();
    }

    #[test]
    fn clearing_a_store_that_was_never_written_is_not_an_error() {
        let store = temp_store("clear-empty");
        assert!(store.clear().is_ok());
    }

    #[test]
    fn the_default_path_lives_under_the_runtime_directory_when_one_exists() {
        let path = SessionStore::default_path();
        assert!(
            path.ends_with("desktop-driver/snapshot.json"),
            "got {path:?}"
        );
    }
}
