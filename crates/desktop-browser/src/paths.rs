use std::path::{Path, PathBuf};

use crate::{BrowserEngine, BrowserError, BrowserResult};

#[derive(Clone, Debug)]
pub struct ProfilePaths {
    pub socket: PathBuf,
    pub user_data: PathBuf,
    pub firefox_user_data: PathBuf,
    pub downloads: PathBuf,
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
    })
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
}
