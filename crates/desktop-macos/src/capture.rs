//! Screen capture through ScreenCaptureKit.
//!
//! `CGWindowListCreateImage` is *obsoleted* in the macOS 15 SDK — a hard error
//! for C callers, and on borrowed time for Rust FFI, which bypasses the
//! availability annotation. ScreenCaptureKit is the supported replacement, so
//! this is the only capture path and the minimum supported macOS is 14.
//!
//! ScreenCaptureKit is completion-handler based. The ports are synchronous, so
//! each call is bridged back with a semaphore — the same containment strategy
//! the Linux adapter uses for its async D-Bus clients.

use std::{
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicPtr, Ordering},
    },
    time::Duration,
};

use block2::RcBlock;
use dispatch2::{DispatchSemaphore, DispatchTime};
use objc2::AnyThread as _;
use objc2_core_foundation::{CFRetained, CGRect};
use objc2_core_graphics::{CGImage, CGImageAlphaInfo};
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
    SCWindow,
};

use desktop_core::{
    errors::{DesktopError, Permission, Result},
    models::{
        geometry::{CoordinateSpace, ScaleFactor},
        ids::WindowId,
        image::Image,
    },
    ports::{CapturePort, CaptureTarget},
};

/// How long to wait for ScreenCaptureKit before giving up.
///
/// The first call after a permission change can be slow, but an unbounded wait
/// would hang the CLI when the user never answers the prompt.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ScreenCaptureKit;

impl ScreenCaptureKit {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Enumerates capturable content, blocking on the completion handler.
    ///
    /// The completion block runs on ScreenCaptureKit's own dispatch queue, not
    /// this thread. `Retained<T>` is deliberately `!Send`, so the block hands
    /// back a raw pointer through an `AtomicPtr` — after taking a reference,
    /// which is thread-safe — and ownership is reconstructed here.
    fn shareable_content() -> Result<objc2::rc::Retained<SCShareableContent>> {
        let slot = Arc::new(AtomicPtr::<SCShareableContent>::new(ptr::null_mut()));
        let failed = Arc::new(AtomicBool::new(false));
        let semaphore = DispatchSemaphore::new(0);

        {
            let slot = Arc::clone(&slot);
            let failed = Arc::clone(&failed);
            let semaphore = semaphore.clone();
            let handler = RcBlock::new(
                move |content: *mut SCShareableContent, error: *mut NSError| {
                    if content.is_null() || !error.is_null() {
                        failed.store(true, Ordering::SeqCst);
                    } else {
                        // SAFETY: the object is live for the duration of the
                        // block; `retain` is atomic and safe to call from any
                        // thread, and balances the `from_raw` below.
                        let retained = unsafe { objc2::rc::Retained::retain(content) };
                        slot.store(
                            retained.map_or(ptr::null_mut(), objc2::rc::Retained::into_raw),
                            Ordering::SeqCst,
                        );
                    }
                    semaphore.signal();
                },
            );
            // SAFETY: the handler outlives the call because the semaphore below
            // blocks until it has run.
            unsafe {
                SCShareableContent::getShareableContentWithCompletionHandler(&handler);
            }
        }

        if !wait_for(&semaphore) {
            return Err(DesktopError::backend(
                "ScreenCaptureKit did not respond; the Screen Recording prompt may be waiting",
            ));
        }
        if failed.load(Ordering::SeqCst) {
            return Err(screen_recording_denied());
        }

        let raw = slot.swap(ptr::null_mut(), Ordering::SeqCst);
        // SAFETY: the block stored a +1 reference; taking it back here balances
        // that exactly once, because `swap` leaves null behind.
        unsafe { objc2::rc::Retained::from_raw(raw) }.ok_or_else(screen_recording_denied)
    }

    /// Captures one image through a content filter.
    ///
    /// Uses the same raw-pointer handoff as the display enumeration above,
    /// because the completion block runs on another thread.
    fn capture_with(filter: &SCContentFilter, width: isize, height: isize) -> Result<Image> {
        // SAFETY: a freshly allocated configuration; the setters below take
        // plain integers.
        let configuration = unsafe { SCStreamConfiguration::new() };
        // SAFETY: the configuration is uniquely owned here.
        unsafe {
            configuration.setWidth(width.max(1) as usize);
            configuration.setHeight(height.max(1) as usize);
            configuration.setShowsCursor(false);
        }

        let slot = Arc::new(AtomicPtr::<CGImage>::new(ptr::null_mut()));
        let failed = Arc::new(AtomicBool::new(false));
        let semaphore = DispatchSemaphore::new(0);

        {
            let slot = Arc::clone(&slot);
            let failed = Arc::clone(&failed);
            let semaphore = semaphore.clone();
            let handler = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
                if image.is_null() || !error.is_null() {
                    failed.store(true, Ordering::SeqCst);
                } else if let Some(pointer) = ptr::NonNull::new(image) {
                    // SAFETY: the callback delivers a +0 reference, so it is
                    // retained before outliving the block. CFRetain is atomic.
                    let retained = unsafe { CFRetained::retain(pointer) };
                    slot.store(CFRetained::into_raw(retained).as_ptr(), Ordering::SeqCst);
                }
                semaphore.signal();
            });
            // SAFETY: the block is kept alive until the semaphore is signalled.
            unsafe {
                SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                    filter,
                    &configuration,
                    Some(&handler),
                );
            }
        }

        if !wait_for(&semaphore) {
            return Err(DesktopError::backend("ScreenCaptureKit timed out"));
        }
        if failed.load(Ordering::SeqCst) {
            return Err(screen_recording_denied());
        }

        let raw = slot.swap(ptr::null_mut(), Ordering::SeqCst);
        let pointer = ptr::NonNull::new(raw)
            .ok_or_else(|| DesktopError::backend("ScreenCaptureKit returned no image"))?;
        // SAFETY: the block stored a +1 reference and `swap` leaves null, so
        // ownership is taken back exactly once.
        let image = unsafe { CFRetained::from_raw(pointer) };

        to_rgba(&image)
    }
}

impl CapturePort for ScreenCaptureKit {
    fn capture(&self, target: &CaptureTarget) -> Result<Image> {
        let content = Self::shareable_content()?;

        match target {
            CaptureTarget::Screen => {
                // SAFETY: reading a property of a live object.
                let displays = unsafe { content.displays() };
                let display: objc2::rc::Retained<SCDisplay> = displays
                    .firstObject()
                    .ok_or_else(|| DesktopError::backend("no displays are available to capture"))?;
                // SAFETY: `display` is live; an empty exclusion list is valid.
                let filter = unsafe {
                    SCContentFilter::initWithDisplay_excludingWindows(
                        SCContentFilter::alloc(),
                        &display,
                        &objc2_foundation::NSArray::new(),
                    )
                };
                // SAFETY: property reads on a live display.
                let (width, height) = unsafe { (display.width(), display.height()) };
                Self::capture_with(&filter, width, height)
            }
            CaptureTarget::Window(id) => {
                // SAFETY: reading a property of a live object.
                let windows = unsafe { content.windows() };
                let window: objc2::rc::Retained<SCWindow> = windows
                    .to_vec()
                    .into_iter()
                    // SAFETY: property read on a live window.
                    .find(|candidate| unsafe { candidate.windowID() } == id.get())
                    .ok_or_else(|| DesktopError::TargetNotFound {
                        target: format!("window {id}"),
                    })?;
                // SAFETY: `window` is live.
                let filter = unsafe {
                    SCContentFilter::initWithDesktopIndependentWindow(
                        SCContentFilter::alloc(),
                        &window,
                    )
                };
                // SAFETY: property read on a live window.
                let frame: CGRect = unsafe { window.frame() };
                let mut image = Self::capture_with(
                    &filter,
                    frame.size.width as isize,
                    frame.size.height as isize,
                )?;
                image.space = CoordinateSpace::Window(WindowId::new(id.get()));
                Ok(image)
            }
        }
    }
}

/// Blocks until the completion handler signals, or the timeout elapses.
///
/// Returns `false` on timeout, so the caller can report a stuck prompt rather
/// than waiting forever.
fn wait_for(semaphore: &DispatchSemaphore) -> bool {
    let deadline = DispatchTime::NOW.time(CAPTURE_TIMEOUT.as_nanos() as i64);
    semaphore.try_acquire(deadline).is_ok()
}

/// Converts a `CGImage` into tightly packed RGBA8.
///
/// The row stride is almost never `width * 4` — Core Graphics pads rows for
/// alignment — so copying the buffer wholesale produces a sheared image.
///
/// ScreenCaptureKit delivers BGRA on Apple silicon and Intel alike, so the red
/// and blue channels are swapped on the way out.
fn to_rgba(image: &CGImage) -> Result<Image> {
    let (width, height, bytes_per_row, bits_per_pixel) = (
        CGImage::width(Some(image)),
        CGImage::height(Some(image)),
        CGImage::bytes_per_row(Some(image)),
        CGImage::bits_per_pixel(Some(image)),
    );
    if bits_per_pixel != 32 {
        return Err(DesktopError::backend(format!(
            "unexpected capture format: {bits_per_pixel} bits per pixel"
        )));
    }

    let provider = CGImage::data_provider(Some(image))
        .ok_or_else(|| DesktopError::backend("capture has no pixel data"))?;
    let data = objc2_core_graphics::CGDataProvider::data(Some(&provider))
        .ok_or_else(|| DesktopError::backend("capture pixel data is unreadable"))?;
    // SAFETY: the CFData is live and its length is queried from itself.
    let bytes = unsafe {
        let pointer = objc2_core_foundation::CFData::byte_ptr(&data);
        let length = objc2_core_foundation::CFData::length(&data) as usize;
        std::slice::from_raw_parts(pointer, length)
    };

    let alpha = CGImage::alpha_info(Some(image));
    let swap_red_and_blue = matches!(
        alpha,
        CGImageAlphaInfo::First
            | CGImageAlphaInfo::PremultipliedFirst
            | CGImageAlphaInfo::NoneSkipFirst
    );

    let mut pixels = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let start = row * bytes_per_row;
        let end = start + width * 4;
        if end > bytes.len() {
            return Err(DesktopError::backend(
                "capture buffer is shorter than its stated dimensions",
            ));
        }
        for chunk in bytes[start..end].chunks_exact(4) {
            if swap_red_and_blue {
                pixels.extend_from_slice(&[chunk[2], chunk[1], chunk[0], 0xff]);
            } else {
                pixels.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 0xff]);
            }
        }
    }

    Image::new(
        u32::try_from(width).unwrap_or(0),
        u32::try_from(height).unwrap_or(0),
        ScaleFactor::ONE,
        CoordinateSpace::primary_screen(),
        pixels,
    )
    .map_err(|error| DesktopError::backend(error.to_string()))
}

/// The error every capture failure funnels into.
///
/// ScreenCaptureKit does not distinguish "denied" from "failed", and denial is
/// overwhelmingly the reason — including after macOS 15's periodic
/// re-authorisation silently revokes a previously working grant.
fn screen_recording_denied() -> DesktopError {
    DesktopError::PermissionRequired {
        permission: Permission::ScreenRecording,
        platform: desktop_core::models::backend::Platform::Macos,
        remedy: crate::probe::screen_recording_remedy(),
    }
}

impl Default for ScreenCaptureKit {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_capture_timeout_is_bounded_so_an_unanswered_prompt_cannot_hang_the_cli() {
        assert!(CAPTURE_TIMEOUT.as_secs() > 0);
        assert!(CAPTURE_TIMEOUT.as_secs() <= 30);
    }

    #[test]
    fn a_capture_failure_is_reported_as_a_permission_problem_with_a_remedy() {
        // ScreenCaptureKit cannot tell denial from failure, and denial is the
        // overwhelmingly common cause — including after macOS 15 silently
        // revokes a grant on its re-authorisation schedule.
        let error = screen_recording_denied();
        match error {
            DesktopError::PermissionRequired {
                permission, remedy, ..
            } => {
                assert_eq!(permission, Permission::ScreenRecording);
                assert!(!remedy.is_empty());
            }
            other => panic!("expected a permission error, got {other:?}"),
        }
    }
}
