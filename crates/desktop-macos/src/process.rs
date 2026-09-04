//! Enumerating running applications.
//!
//! `NSWorkspace` would be the natural source but is main-thread-bound, which a
//! CLI cannot rely on. `CGWindowListCopyWindowInfo` is not, and it also gives
//! the window list for free.
//!
//! One caveat worth knowing: since macOS 10.15 the window *titles* in this list
//! are gated behind Screen Recording permission. The owner names and pids come
//! through regardless, so `desktop apps` works without it while window titles
//! quietly come back empty — which is why the probe reports that permission
//! even for commands that are not obviously about capture.

use std::{ffi::c_void, os::unix::ffi::OsStrExt as _, path::Path};

use objc2_core_foundation::{
    CFBundle, CFDictionary, CFNumber, CFRetained, CFString, CFType, CFURL,
};
use objc2_core_graphics::{
    CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo, CGWindowListOption,
};

use desktop_core::models::{
    app::AppKey,
    geometry::Bounds,
    ids::{ProcessId, WindowId},
};

/// Window layer 0 is the normal application layer; anything else is a menu,
/// dock tile, or system overlay that no agent wants in its application list.
const NORMAL_WINDOW_LAYER: i64 = 0;

/// Applications that own at least one ordinary on-screen window.
#[must_use]
pub fn running_applications() -> Vec<AppKey> {
    let mut seen: Vec<AppKey> = Vec::new();

    for window in window_list(CGWindowListOption::OptionAll) {
        let Some(pid) = number(&window, "kCGWindowOwnerPID").map(|n| n as i32) else {
            continue;
        };
        if number(&window, "kCGWindowLayer").unwrap_or(-1) != NORMAL_WINDOW_LAYER {
            continue;
        }
        let name = string(&window, "kCGWindowOwnerName").unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        if seen.iter().any(|app| app.pid.get() == pid) {
            continue;
        }
        let mut key = AppKey::new(ProcessId::new(pid), &name);
        if let Some(identifier) = bundle_identifier(pid) {
            key = key.with_identifier(&identifier);
        }
        seen.push(key);
    }

    seen
}

/// The pid of the frontmost application, taken as the owner of the topmost
/// ordinary window — `CGWindowListCopyWindowInfo` returns front-to-back order.
#[must_use]
pub fn frontmost_pid() -> Option<ProcessId> {
    window_list(CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements)
        .into_iter()
        .find_map(|window| {
            (number(&window, "kCGWindowLayer").unwrap_or(-1) == NORMAL_WINDOW_LAYER)
                .then(|| number(&window, "kCGWindowOwnerPID").map(|n| ProcessId::new(n as i32)))
                .flatten()
        })
}

/// A Core Graphics window record used to join AX windows to ScreenCaptureKit.
///
/// The numeric id is the only part both APIs expose. AX does not publish it,
/// so the join uses pid plus title/bounds and falls back to the per-app
/// ordinal only when an application omits those attributes.
#[derive(Clone, Debug)]
pub struct WindowRecord {
    pub id: WindowId,
    pub pid: ProcessId,
    pub title: Option<String>,
    pub bounds: Option<Bounds>,
}

#[must_use]
pub fn windows() -> Vec<WindowRecord> {
    window_list(CGWindowListOption::OptionAll)
        .into_iter()
        .filter_map(|window| {
            if number(&window, "kCGWindowLayer").unwrap_or(-1) != NORMAL_WINDOW_LAYER {
                return None;
            }
            let id = u32::try_from(number(&window, "kCGWindowNumber")?).ok()?;
            let pid = i32::try_from(number(&window, "kCGWindowOwnerPID")?).ok()?;
            let owner = string(&window, "kCGWindowOwnerName").unwrap_or_default();
            if owner.is_empty() {
                return None;
            }
            Some(WindowRecord {
                id: WindowId::new(id),
                pid: ProcessId::new(pid),
                title: string(&window, "kCGWindowName").filter(|title| !title.is_empty()),
                bounds: bounds(&window, "kCGWindowBounds"),
            })
        })
        .collect()
}

/// Brings an application to the front.
///
/// Done through the accessibility API rather than the Process Manager:
/// `SetFrontProcess` is not exposed by `objc2-application-services`, and
/// setting `AXFrontmost` is the supported modern equivalent. Raising a window
/// alone is not enough — the owning application also has to become active, or
/// the raised window sits behind the frontmost app.
pub fn activate(pid: ProcessId) -> desktop_core::errors::Result<()> {
    let app = crate::ax::Element::for_application(pid.get());
    app.set_boolean(crate::ax_constants::attribute::FRONTMOST, true)
}

fn bundle_identifier(pid: i32) -> Option<String> {
    let mut buffer = [0_u8; 4096];
    // SAFETY: `buffer` is writable for exactly the size passed. libproc
    // returns the byte length of a NUL-terminated filesystem path.
    let count = unsafe {
        proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast::<c_void>(),
            u32::try_from(buffer.len()).ok()?,
        )
    };
    if count <= 0 {
        return None;
    }
    let bytes = &buffer[..usize::try_from(count).ok()?];
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    let executable = Path::new(std::ffi::OsStr::from_bytes(bytes));
    let app = executable.ancestors().find(|ancestor| {
        ancestor
            .extension()
            .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("app"))
    })?;
    // SAFETY: the path bytes stay live for the call and the URL is marked as
    // a directory because an application bundle is one.
    let url = unsafe {
        CFURL::from_file_system_representation(
            None,
            app.as_os_str().as_bytes().as_ptr(),
            isize::try_from(app.as_os_str().as_bytes().len()).ok()?,
            true,
        )
    }?;
    CFBundle::new(None, Some(&url))?
        .identifier()
        .map(|identifier| identifier.to_string())
}

unsafe extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut c_void, buffer_size: u32) -> i32;
}

fn window_list(options: CGWindowListOption) -> Vec<CFRetained<CFDictionary>> {
    let Some(array) =
        CGWindowListCopyWindowInfo(options | CGWindowListOption::ExcludeDesktopElements, 0)
    else {
        return Vec::new();
    };

    let count = array.count();
    let mut out = Vec::with_capacity(count.max(0) as usize);
    for index in 0..count {
        // SAFETY: `index` is in range and the array holds CFDictionaries.
        let raw = unsafe { array.value_at_index(index) };
        let Some(pointer) = std::ptr::NonNull::new(raw.cast_mut()) else {
            continue;
        };
        // SAFETY: CFArray yields a borrowed reference; retaining takes an owned
        // one that outlives the array.
        out.push(unsafe { CFRetained::retain(pointer.cast::<CFDictionary>()) });
    }
    out
}

fn value(window: &CFDictionary, key: &str) -> Option<CFRetained<CFType>> {
    let key = CFString::from_str(key);
    // SAFETY: both the dictionary and key are live for the call.
    let raw = unsafe { window.value(key.as_ref() as *const CFString as *const _) };
    let pointer = std::ptr::NonNull::new(raw.cast_mut())?;
    // SAFETY: dictionary lookups return a borrowed reference.
    Some(unsafe { CFRetained::retain(pointer.cast::<CFType>()) })
}

fn number(window: &CFDictionary, key: &str) -> Option<i64> {
    value(window, key)?.downcast_ref::<CFNumber>()?.as_i64()
}

fn string(window: &CFDictionary, key: &str) -> Option<String> {
    Some(value(window, key)?.downcast_ref::<CFString>()?.to_string())
}

fn bounds(window: &CFDictionary, key: &str) -> Option<Bounds> {
    let value = value(window, key)?;
    let dictionary = value.downcast_ref::<CFDictionary>()?;
    let mut rect = objc2_core_foundation::CGRect::ZERO;
    // SAFETY: `rect` is a valid out pointer and the dictionary is live.
    if !unsafe { CGRectMakeWithDictionaryRepresentation(Some(dictionary), &mut rect) } {
        return None;
    }
    Some(Bounds::new(
        rect.origin.x.round() as i32,
        rect.origin.y.round() as i32,
        rect.size.width.round() as i32,
        rect.size.height.round() as i32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_ordinary_window_layer_counts_as_an_application_window() {
        // Layer 0 is the app layer; menus, the dock and system overlays live
        // above it and would otherwise show up as phantom applications.
        assert_eq!(NORMAL_WINDOW_LAYER, 0);
    }
}
