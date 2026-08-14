//! Writing captured images to disk.

use std::{
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
};

use desktop_core::{
    errors::{DesktopError, Result},
    models::image::Image,
};

/// Writes `image` as a PNG, returning where it landed.
///
/// With no path given the file goes to a uniquely-named temporary file rather
/// than a fixed one, so two concurrent captures cannot overwrite each other.
///
/// Created owner-only rather than left to the umask: a screenshot of the
/// agent's screen is whatever was on it — a logged-in page, a document, a
/// password manager — and the default location is shared with every other user
/// on the machine.
pub fn write(image: &Image, requested: Option<&str>) -> Result<PathBuf> {
    let path = match requested {
        Some(path) => PathBuf::from(path),
        None => default_path(),
    };

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| {
            DesktopError::internal(format!("cannot create {}: {error}", parent.display()))
        })?;
    }

    let buffer: image::RgbaImage =
        image::ImageBuffer::from_raw(image.width, image.height, image.pixels.clone()).ok_or_else(
            || DesktopError::internal("captured pixel buffer does not match its dimensions"),
        )?;

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| {
            DesktopError::internal(format!("cannot write {}: {error}", path.display()))
        })?;
    let mut writer = std::io::BufWriter::new(file);
    buffer
        .write_to(&mut writer, image::ImageFormat::Png)
        .map_err(|error| {
            DesktopError::internal(format!("cannot write {}: {error}", path.display()))
        })?;

    Ok(path)
}

/// Where a capture goes when the caller does not say.
///
/// The runtime directory first: it is per-user and mode 0700, whereas the
/// temporary directory is shared with every account on the machine. Falling
/// back to it is still correct — the file itself is owner-only — but the
/// directory is the better place when there is one.
///
/// The name carries the process id and a monotonic counter, so it is unique
/// within a run and across concurrent ones.
fn default_path() -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = if sequence == 0 {
        format!("desktop-driver-{}.png", std::process::id())
    } else {
        format!("desktop-driver-{}-{sequence}.png", std::process::id())
    };
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map_or_else(
            || std::env::temp_dir().join(&name),
            |base| base.join("desktop-driver").join(&name),
        )
}

/// Whether a path looks like somewhere a PNG can be written.
#[must_use]
pub fn is_writable_target(path: &Path) -> bool {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .is_none_or(Path::is_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_core::models::geometry::{CoordinateSpace, ScaleFactor};
    use std::os::unix::fs::PermissionsExt as _;

    fn red_pixel() -> Image {
        Image::new(
            1,
            1,
            ScaleFactor::ONE,
            CoordinateSpace::primary_screen(),
            vec![0xff, 0x00, 0x00, 0xff],
        )
        .expect("constructs")
    }

    #[test]
    fn an_image_round_trips_through_a_png_file_with_its_pixels_intact() {
        let path = std::env::temp_dir().join(format!(
            "desktop-driver-png-test-{}.png",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let written = write(&red_pixel(), path.to_str()).expect("writes");
        assert_eq!(written, path);

        let decoded = image::open(&path).expect("decodes").to_rgba8();
        assert_eq!(decoded.dimensions(), (1, 1));
        assert_eq!(decoded.get_pixel(0, 0).0, [0xff, 0x00, 0x00, 0xff]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn successive_default_paths_do_not_collide() {
        // Two captures in one process must not silently overwrite each other.
        let first = default_path();
        let second = default_path();
        assert_ne!(first, second);
    }

    #[test]
    fn a_default_path_lands_somewhere_private_when_there_is_such_a_place() {
        // Was the shared temporary directory, which is mode 1777 — every
        // account on the machine could read every capture the agent took.
        let path = default_path();
        let expected = std::env::var_os("XDG_RUNTIME_DIR")
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
        assert!(path.starts_with(&expected), "got {path:?}");
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
    }

    #[test]
    fn writing_into_a_missing_directory_creates_it_rather_than_failing() {
        let dir =
            std::env::temp_dir().join(format!("desktop-driver-png-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("shot.png");

        write(&red_pixel(), path.to_str()).expect("writes");
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bare_filename_is_treated_as_writable_in_the_current_directory() {
        assert!(is_writable_target(Path::new("shot.png")));
    }

    #[test]
    fn a_capture_is_readable_only_by_the_user_who_took_it() {
        // A screenshot is whatever was on the screen. The default location is
        // shared with every other account on the machine, so the file itself
        // has to be the thing that is private.
        let mut path = std::env::temp_dir();
        path.push(format!("desktop-driver-mode-{}.png", std::process::id()));
        let written = write(&red_pixel(), Some(&path.display().to_string())).expect("writes");
        let mode = std::fs::metadata(&written)
            .expect("exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
        let _ = std::fs::remove_file(&written);
    }

    #[test]
    fn the_default_location_prefers_the_per_user_runtime_directory() {
        let path = default_path();
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            assert!(
                path.starts_with(std::path::Path::new(&runtime)),
                "{} should be under the runtime directory",
                path.display()
            );
        }
        assert!(path.extension().is_some_and(|e| e == "png"));
    }
}
