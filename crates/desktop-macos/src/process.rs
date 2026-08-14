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

use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_graphics::{CGWindowListCopyWindowInfo, CGWindowListOption};

use desktop_core::models::{app::AppKey, ids::ProcessId};

/// Window layer 0 is the normal application layer; anything else is a menu,
/// dock tile, or system overlay that no agent wants in its application list.
const NORMAL_WINDOW_LAYER: i64 = 0;

/// Applications that own at least one ordinary on-screen window.
#[must_use]
pub fn running_applications() -> Vec<AppKey> {
    let mut seen: Vec<AppKey> = Vec::new();

    for window in window_list() {
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
        seen.push(AppKey::new(ProcessId::new(pid), &name));
    }

    seen
}

/// The pid of the frontmost application, taken as the owner of the topmost
/// ordinary window — `CGWindowListCopyWindowInfo` returns front-to-back order.
#[must_use]
pub fn frontmost_pid() -> Option<ProcessId> {
    window_list().into_iter().find_map(|window| {
        (number(&window, "kCGWindowLayer").unwrap_or(-1) == NORMAL_WINDOW_LAYER)
            .then(|| number(&window, "kCGWindowOwnerPID").map(|n| ProcessId::new(n as i32)))
            .flatten()
    })
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

fn window_list() -> Vec<CFRetained<CFDictionary>> {
    let Some(array) = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        0,
    ) else {
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
