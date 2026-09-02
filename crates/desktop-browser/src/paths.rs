use std::{
    io::Write as _,
    path::{Path, PathBuf},
};

use crate::{BrowserEngine, BrowserError, BrowserResult};

#[derive(Clone, Debug)]
pub struct ProfilePaths {
    pub socket: PathBuf,
    pub user_data: PathBuf,
    pub firefox_user_data: PathBuf,
    pub downloads: PathBuf,
    pub engine_file: PathBuf,
}

pub fn profile_name(explicit: Option<&str>, active: Option<&str>) -> BrowserResult<String> {
    let name = explicit.or(active).unwrap_or("default");
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(BrowserError::new(
            "invalid_profile",
            format!("invalid browser profile {name:?}; use letters, digits, '-' or '_'"),
        ));
    }
    Ok(name.to_owned())
}

pub fn profile_paths(profile: &str) -> BrowserResult<ProfilePaths> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "desktop-driver-{}",
                std::env::var("USER").unwrap_or_else(|_| "user".into())
            ))
        });
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))
                .join(".local/share")
        });
    let inherited_home = std::env::var_os("HOME").map(PathBuf::from);
    let inside_matching_session = inherited_home.as_ref().is_some_and(|home| {
        home.file_name().is_some_and(|name| name == "home")
            && home
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == profile)
            && home
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .is_some_and(|name| name == "sessions")
    });
    let browser_root = if inside_matching_session {
        inherited_home.unwrap().join(".config/desktop-driver")
    } else {
        data.join("desktop-driver/sessions")
            .join(profile)
            .join("home/.config/desktop-driver")
    };
    let base = browser_root.join("chromium");
    Ok(ProfilePaths {
        socket: runtime
            .join("desktop-driver/browser")
            .join(format!("{profile}.sock")),
        downloads: base.join("Downloads"),
        user_data: base,
        firefox_user_data: browser_root.join("firefox"),
        engine_file: browser_root.join("engine"),
    })
}

pub fn profile_engine(profile: &str) -> BrowserResult<Option<BrowserEngine>> {
    let paths = profile_paths(profile)?;
    profile_engine_from_paths(profile, &paths)
}

fn profile_engine_from_paths(
    profile: &str,
    paths: &ProfilePaths,
) -> BrowserResult<Option<BrowserEngine>> {
    if paths.engine_file.is_file() {
        let value = std::fs::read_to_string(&paths.engine_file).map_err(io_error)?;
        return match value.trim() {
            "chromium" => Ok(Some(BrowserEngine::Chromium)),
            "firefox" => Ok(Some(BrowserEngine::Firefox)),
            value => Err(BrowserError::new(
                "invalid_profile_engine",
                format!("browser profile {profile:?} has invalid engine marker {value:?}"),
            )
            .remedy("Pass --browser chromium or --browser firefox to repair the profile.")),
        };
    }

    // Profiles created before the marker was introduced can still be
    // recovered without making the agent repeat its original engine choice.
    match (paths.user_data.is_dir(), paths.firefox_user_data.is_dir()) {
        (false, true) => Ok(Some(BrowserEngine::Firefox)),
        (true, false) => Ok(Some(BrowserEngine::Chromium)),
        _ => Ok(None),
    }
}

pub fn save_profile_engine(profile: &str, engine: BrowserEngine) -> BrowserResult<()> {
    let path = profile_paths(profile)?.engine_file;
    save_engine_file(&path, engine)
}

fn save_engine_file(path: &Path, engine: BrowserEngine) -> BrowserResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| BrowserError::new("browser_io", "invalid engine marker path"))?;
    ensure_private_dir(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(io_error)?;
        let temporary = parent.join(format!(".engine-{}.tmp", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(io_error)?;
        file.write_all(engine.as_str().as_bytes())
            .map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
            .map_err(io_error)?;
        std::fs::rename(&temporary, path).map_err(io_error)?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, engine.as_str()).map_err(io_error)?;
    Ok(())
}

/// Creates an application-owned directory chain with private permissions.
///
/// The XDG/HOME root belongs to the caller and is never chmodded. Starting at
/// the `desktop-driver` component, every directory belongs to this tool and is
/// held at 0700, including intermediate session and browser directories.
#[doc(hidden)]
pub fn ensure_private_dir(path: &Path) -> BrowserResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path).map_err(io_error)?;

        let mut owned = Vec::new();
        let mut found_root = false;
        for ancestor in path.ancestors() {
            owned.push(ancestor);
            if ancestor
                .file_name()
                .is_some_and(|name| name == "desktop-driver")
            {
                found_root = true;
                break;
            }
        }
        if !found_root {
            owned.truncate(1);
        }
        for directory in owned.into_iter().rev() {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                .map_err(io_error)?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path).map_err(io_error)
}

pub fn browser_executable(engine: BrowserEngine, explicit: Option<&str>) -> BrowserResult<PathBuf> {
    if let Some(path) = explicit {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(BrowserError::new(
            "browser_not_found",
            format!("browser executable does not exist: {}", path.display()),
        ));
    }
    if engine == BrowserEngine::Chromium {
        let installed = installed_browser_path();
        if installed.is_file() {
            return Ok(installed);
        }
    }
    for name in browser_names(engine) {
        if let Some(path) = find_in_path(name) {
            return Ok(path);
        }
    }
    Err(BrowserError::new(
        "browser_not_found",
        format!("no compatible {} browser was found", engine.as_str()),
    )
    .remedy(match engine {
        BrowserEngine::Chromium => {
            "Install Chrome/Chromium, pass --executable, or run `desktop browser install`."
        }
        BrowserEngine::Firefox => {
            "Install Firefox or pass --executable to `desktop browser open --browser firefox`."
        }
    }))
}

pub fn installed_browser_path() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))
                .join(".local/share")
        });
    #[cfg(target_os = "macos")]
    return data.join("desktop-driver/browsers/chrome-for-testing/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing");
    #[cfg(not(target_os = "macos"))]
    data.join("desktop-driver/browsers/chrome-for-testing/chrome-linux64/chrome")
}

fn browser_names(engine: BrowserEngine) -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        match engine {
            BrowserEngine::Chromium => &[
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                "/Applications/Chromium.app/Contents/MacOS/Chromium",
            ],
            BrowserEngine::Firefox => &["/Applications/Firefox.app/Contents/MacOS/firefox"],
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        match engine {
            BrowserEngine::Chromium => &[
                "google-chrome",
                "google-chrome-stable",
                "chromium",
                "chromium-browser",
            ],
            BrowserEngine::Firefox => &["firefox", "firefox-esr"],
        }
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return path.is_file().then(|| path.to_owned());
    }
    std::env::var_os("PATH")?
        .to_string_lossy()
        .split(':')
        .map(Path::new)
        .map(|p| p.join(name))
        .find(|p| p.is_file())
}

fn io_error(error: std::io::Error) -> BrowserError {
    BrowserError::new("browser_io", error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names_are_safe_for_paths_and_sockets() {
        assert_eq!(profile_name(Some("github_2"), None).unwrap(), "github_2");
        for invalid in ["", "../x", "has space", "a/b"] {
            assert!(
                profile_name(Some(invalid), None).is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn active_session_is_the_default_profile() {
        assert_eq!(profile_name(None, Some("work")).unwrap(), "work");
        assert_eq!(profile_name(None, None).unwrap(), "default");
    }

    #[test]
    fn firefox_and_chromium_profiles_do_not_share_browser_state() {
        let paths = profile_paths("engine-isolation").unwrap();
        assert_ne!(paths.user_data, paths.firefox_user_data);
        assert!(paths.user_data.ends_with("chromium"));
        assert!(paths.firefox_user_data.ends_with("firefox"));
    }

    #[test]
    fn a_saved_engine_wins_and_legacy_firefox_profiles_are_inferred() {
        let root = std::env::temp_dir().join(format!(
            "desktop-browser-engine-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let paths = ProfilePaths {
            socket: root.join("browser.sock"),
            user_data: root.join("chromium"),
            firefox_user_data: root.join("firefox"),
            downloads: root.join("downloads"),
            engine_file: root.join("engine"),
        };
        std::fs::create_dir_all(&paths.firefox_user_data).unwrap();
        assert_eq!(
            profile_engine_from_paths("test", &paths).unwrap(),
            Some(BrowserEngine::Firefox)
        );
        save_engine_file(&paths.engine_file, BrowserEngine::Chromium).unwrap();
        assert_eq!(
            profile_engine_from_paths("test", &paths).unwrap(),
            Some(BrowserEngine::Chromium)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn every_application_owned_profile_directory_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = std::env::temp_dir().join(format!(
            "desktop-browser-permissions-test-{}",
            std::process::id()
        ));
        let owned = root.join("desktop-driver/sessions/test/home/.config/desktop-driver/chromium");
        let _ = std::fs::remove_dir_all(&root);
        ensure_private_dir(&owned).unwrap();
        let app_root = root.join("desktop-driver");
        let mut current = owned.as_path();
        loop {
            assert_eq!(
                std::fs::metadata(current).unwrap().permissions().mode() & 0o777,
                0o700,
                "{} was not private",
                current.display()
            );
            if current == app_root {
                break;
            }
            current = current.parent().unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
