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
        Arc, Mutex,
        atomic::{AtomicPtr, Ordering},
    },
    time::Duration,
};

use block2::RcBlock;
use dispatch2::{DispatchSemaphore, DispatchTime};
use objc2::AnyThread as _;
use objc2_core_foundation::{CFRetained, CGRect};
use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode, CGImage, CGMainDisplayID};
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

/// ScreenCaptureKit's documented SDR format: packed little-endian BGRA8.
const BGRA_PIXEL_FORMAT: u32 = u32::from_be_bytes(*b"BGRA");

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
        if !crate::probe::has_screen_recording() {
            return Err(screen_recording_denied());
        }
        let slot = Arc::new(AtomicPtr::<SCShareableContent>::new(ptr::null_mut()));
        let failure = Arc::new(Mutex::new(None::<String>));
        let semaphore = DispatchSemaphore::new(0);

        {
            let slot = Arc::clone(&slot);
            let failure = Arc::clone(&failure);
            let semaphore = semaphore.clone();
            let handler = RcBlock::new(
                move |content: *mut SCShareableContent, error: *mut NSError| {
                    if content.is_null() || !error.is_null() {
                        *failure.lock().expect("capture error mutex poisoned") = Some(
                            error_description(error, "ScreenCaptureKit returned no content"),
                        );
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
        if let Some(message) = failure.lock().expect("capture error mutex poisoned").take() {
            return Err(DesktopError::backend(message));
        }

        let raw = slot.swap(ptr::null_mut(), Ordering::SeqCst);
        // SAFETY: the block stored a +1 reference; taking it back here balances
        // that exactly once, because `swap` leaves null behind.
        unsafe { objc2::rc::Retained::from_raw(raw) }
            .ok_or_else(|| DesktopError::backend("ScreenCaptureKit returned no content"))
    }

    /// Captures one image through a content filter.
    ///
    /// Uses the same raw-pointer handoff as the display enumeration above,
    /// because the completion block runs on another thread.
    fn capture_with(
        filter: &SCContentFilter,
        width: isize,
        height: isize,
        scale: ScaleFactor,
        space: CoordinateSpace,
    ) -> Result<Image> {
        // SAFETY: a freshly allocated configuration; the setters below take
        // plain integers.
        let configuration = unsafe { SCStreamConfiguration::new() };
        // SAFETY: the configuration is uniquely owned here.
        unsafe {
            configuration.setWidth(width.max(1) as usize);
            configuration.setHeight(height.max(1) as usize);
            configuration.setPixelFormat(BGRA_PIXEL_FORMAT);
            configuration.setScalesToFit(true);
            configuration.setPreservesAspectRatio(true);
            configuration.setShowsCursor(false);
            // A window's SCWindow frame excludes its shadow. Match those
            // dimensions exactly and retain portions that sit off-screen.
            configuration.setIgnoreShadowsSingleWindow(true);
            configuration.setIgnoreGlobalClipSingleWindow(true);
        }

        let slot = Arc::new(AtomicPtr::<CGImage>::new(ptr::null_mut()));
        let failure = Arc::new(Mutex::new(None::<String>));
        let semaphore = DispatchSemaphore::new(0);

        {
            let slot = Arc::clone(&slot);
            let failure = Arc::clone(&failure);
            let semaphore = semaphore.clone();
            let handler = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
                if image.is_null() || !error.is_null() {
                    *failure.lock().expect("capture error mutex poisoned") = Some(
                        error_description(error, "ScreenCaptureKit returned no image"),
                    );
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
        if let Some(message) = failure.lock().expect("capture error mutex poisoned").take() {
            return Err(DesktopError::backend(message));
        }

        let raw = slot.swap(ptr::null_mut(), Ordering::SeqCst);
        let pointer = ptr::NonNull::new(raw)
            .ok_or_else(|| DesktopError::backend("ScreenCaptureKit returned no image"))?;
        // SAFETY: the block stored a +1 reference and `swap` leaves null, so
        // ownership is taken back exactly once.
        let image = unsafe { CFRetained::from_raw(pointer) };

        to_rgba(&image, scale, space)
    }
}

impl CapturePort for ScreenCaptureKit {
    fn resolve_app(&self, needle: &str) -> Result<Option<desktop_core::models::app::AppKey>> {
        Ok(crate::process::running_applications()
            .into_iter()
            .find(|app| app.matches(needle)))
    }

    fn resolve_window_app(
        &self,
        id: WindowId,
    ) -> Result<Option<desktop_core::models::app::AppKey>> {
        let pid = crate::process::windows()
            .into_iter()
            .find(|window| window.id == id)
            .map(|window| window.pid);
        Ok(pid.and_then(|pid| {
            crate::process::running_applications()
                .into_iter()
                .find(|app| app.pid == pid)
        }))
    }

    fn capture(&self, target: &CaptureTarget) -> Result<Image> {
        let content = Self::shareable_content()?;

        match target {
            CaptureTarget::Screen => {
                // SAFETY: reading a property of a live object.
                let displays = unsafe { content.displays() };
                let display: objc2::rc::Retained<SCDisplay> = displays
                    .to_vec()
                    .into_iter()
                    // SAFETY: property read on a live display.
                    .find(|display| unsafe { display.displayID() } == CGMainDisplayID())
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
                let scale = filter_scale(&filter, display_scale(unsafe { display.displayID() }));
                Self::capture_with(
                    &filter,
                    scaled(width, scale),
                    scaled(height, scale),
                    scale,
                    CoordinateSpace::primary_screen(),
                )
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
                let scale = scale_for_filter_and_frame(&filter, &content, frame);
                Self::capture_with(
                    &filter,
                    scaled(frame.size.width as isize, scale),
                    scaled(frame.size.height as isize, scale),
                    scale,
                    CoordinateSpace::Window(*id),
                )
            }
            CaptureTarget::App(needle) => {
                // Resolve once through the same application identity used by
                // app-scoped policy, then select by pid. Two independently
                // chosen windows with the same display name could otherwise
                // authorize one process and capture another.
                let target_app =
                    self.resolve_app(needle)?
                        .ok_or_else(|| DesktopError::TargetNotFound {
                            target: format!("application {needle:?}"),
                        })?;
                // SAFETY: reading properties of live ScreenCaptureKit objects.
                let mut matching: Vec<_> = unsafe { content.windows() }
                    .to_vec()
                    .into_iter()
                    .filter(|window| unsafe {
                        window.windowLayer() == 0
                            && window.isOnScreen()
                            && window.owningApplication().is_some_and(|candidate| {
                                candidate.processID() == target_app.pid.get()
                            })
                    })
                    .collect();
                let position = matching
                    .iter()
                    .position(|window| unsafe { window.isActive() })
                    .unwrap_or(0);
                if matching.is_empty() {
                    return Err(DesktopError::TargetNotFound {
                        target: format!("application {needle:?}"),
                    });
                }
                let window = matching.swap_remove(position);
                let id = WindowId::new(unsafe { window.windowID() });
                let frame = unsafe { window.frame() };
                let filter = unsafe {
                    SCContentFilter::initWithDesktopIndependentWindow(
                        SCContentFilter::alloc(),
                        &window,
                    )
                };
                let scale = scale_for_filter_and_frame(&filter, &content, frame);
                Self::capture_with(
                    &filter,
                    scaled(frame.size.width as isize, scale),
                    scaled(frame.size.height as isize, scale),
                    scale,
                    CoordinateSpace::Window(id),
                )
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
/// The configuration explicitly requests BGRA8 on Apple silicon and Intel, so
/// red and blue are swapped on the way out and the window's alpha is retained.
fn to_rgba(image: &CGImage, scale: ScaleFactor, space: CoordinateSpace) -> Result<Image> {
    let (width, height, bytes_per_row, bits_per_component, bits_per_pixel) = (
        CGImage::width(Some(image)),
        CGImage::height(Some(image)),
        CGImage::bytes_per_row(Some(image)),
        CGImage::bits_per_component(Some(image)),
        CGImage::bits_per_pixel(Some(image)),
    );
    if bits_per_component != 8 || bits_per_pixel != 32 {
        return Err(DesktopError::backend(format!(
            "unexpected capture format: {bits_per_component} bits per component, {bits_per_pixel} bits per pixel"
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

    let pixels = bgra_rows_to_rgba(bytes, width, height, bytes_per_row)?;

    let width = u32::try_from(width)
        .map_err(|_| DesktopError::backend("capture width does not fit in the image model"))?;
    let height = u32::try_from(height)
        .map_err(|_| DesktopError::backend("capture height does not fit in the image model"))?;
    Image::new(width, height, scale, space, pixels)
        .map_err(|error| DesktopError::backend(error.to_string()))
}

fn bgra_rows_to_rgba(
    bytes: &[u8],
    width: usize,
    height: usize,
    bytes_per_row: usize,
) -> Result<Vec<u8>> {
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| DesktopError::backend("capture row dimensions overflow"))?;
    let capacity = row_bytes
        .checked_mul(height)
        .ok_or_else(|| DesktopError::backend("capture dimensions overflow"))?;
    if bytes_per_row < row_bytes {
        return Err(DesktopError::backend(
            "capture row stride is shorter than its pixel width",
        ));
    }

    let mut pixels = Vec::with_capacity(capacity);
    for row in 0..height {
        let start = row
            .checked_mul(bytes_per_row)
            .ok_or_else(|| DesktopError::backend("capture row offset overflow"))?;
        let end = start
            .checked_add(row_bytes)
            .ok_or_else(|| DesktopError::backend("capture row offset overflow"))?;
        if end > bytes.len() {
            return Err(DesktopError::backend(
                "capture buffer is shorter than its stated dimensions",
            ));
        }
        for chunk in bytes[start..end].chunks_exact(4) {
            pixels.extend_from_slice(&[chunk[2], chunk[1], chunk[0], chunk[3]]);
        }
    }
    Ok(pixels)
}

fn scaled(points: isize, scale: ScaleFactor) -> isize {
    ((points.max(1) as f64) * scale.get()).round().max(1.0) as isize
}

fn display_scale(id: u32) -> ScaleFactor {
    let Some(mode) = CGDisplayCopyDisplayMode(id) else {
        return ScaleFactor::ONE;
    };
    let points = CGDisplayMode::width(Some(&mode));
    if points == 0 {
        return ScaleFactor::ONE;
    }
    ScaleFactor::new(CGDisplayMode::pixel_width(Some(&mode)) as f64 / points as f64)
}

fn scale_for_frame(content: &SCShareableContent, frame: CGRect) -> ScaleFactor {
    // AppKit associates a spanning window with the screen containing the
    // largest part of it. Mirroring that rule avoids selecting the wrong
    // backing scale when the window centre lies just across a display edge.
    let displays = unsafe { content.displays() };
    displays
        .to_vec()
        .into_iter()
        .filter_map(|display| {
            let area = intersection_area(frame, unsafe { display.frame() });
            (area > 0.0).then_some((area, display))
        })
        .max_by(|(left, _), (right, _)| left.total_cmp(right))
        .map(|(_, display)| display_scale(unsafe { display.displayID() }))
        .unwrap_or(ScaleFactor::ONE)
}

fn filter_scale(filter: &SCContentFilter, fallback: ScaleFactor) -> ScaleFactor {
    // Available on the minimum supported macOS (14.0). It reflects the exact
    // filter ScreenCaptureKit will render and is more authoritative than
    // reconstructing the scale from the current display mode.
    let scale = f64::from(unsafe { filter.pointPixelScale() });
    valid_scale_or(scale, fallback)
}

fn scale_for_filter_and_frame(
    filter: &SCContentFilter,
    content: &SCShareableContent,
    frame: CGRect,
) -> ScaleFactor {
    filter_scale(filter, scale_for_frame(content, frame))
}

fn valid_scale_or(scale: f64, fallback: ScaleFactor) -> ScaleFactor {
    if scale.is_finite() && scale > 0.0 {
        ScaleFactor::new(scale)
    } else {
        fallback
    }
}

fn intersection_area(left: CGRect, right: CGRect) -> f64 {
    let left_x2 = left.origin.x + left.size.width;
    let left_y2 = left.origin.y + left.size.height;
    let right_x2 = right.origin.x + right.size.width;
    let right_y2 = right.origin.y + right.size.height;
    let width = left_x2.min(right_x2) - left.origin.x.max(right.origin.x);
    let height = left_y2.min(right_y2) - left.origin.y.max(right.origin.y);
    width.max(0.0) * height.max(0.0)
}

fn error_description(error: *mut NSError, fallback: &str) -> String {
    if error.is_null() {
        return fallback.to_owned();
    }
    // SAFETY: NSError is borrowed and live for the duration of the completion
    // callback; all Objective-C values are copied into owned Rust strings.
    let error = unsafe { &*error };
    format!(
        "ScreenCaptureKit failed ({} {}): {}",
        error.domain(),
        error.code(),
        error.localizedDescription()
    )
}

/// The actionable error for a failed Screen Recording preflight.
///
/// Once preflight succeeds, callback failures preserve NSError domain, code
/// and description rather than being mislabeled as permission denials.
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
    fn a_failed_permission_preflight_has_an_actionable_remedy() {
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

    #[test]
    fn display_selection_can_compare_spanning_window_area() {
        let window = CGRect::new(
            objc2_core_foundation::CGPoint { x: 900.0, y: 0.0 },
            objc2_core_foundation::CGSize {
                width: 400.0,
                height: 500.0,
            },
        );
        let left = CGRect::new(
            objc2_core_foundation::CGPoint { x: 0.0, y: 0.0 },
            objc2_core_foundation::CGSize {
                width: 1_000.0,
                height: 800.0,
            },
        );
        let right = CGRect::new(
            objc2_core_foundation::CGPoint { x: 1_000.0, y: 0.0 },
            objc2_core_foundation::CGSize {
                width: 1_000.0,
                height: 800.0,
            },
        );
        assert!(intersection_area(window, right) > intersection_area(window, left));
    }

    #[test]
    fn bgra_conversion_preserves_transparency_and_skips_row_padding() {
        let bytes = [10, 20, 30, 40, 50, 60, 70, 80, 0xaa, 0xbb, 0xcc, 0xdd];
        let rgba = bgra_rows_to_rgba(&bytes, 2, 1, 12).expect("valid BGRA row");
        assert_eq!(rgba, [30, 20, 10, 40, 70, 60, 50, 80]);
    }

    #[test]
    fn malformed_capture_strides_fail_instead_of_shearing_the_image() {
        assert!(bgra_rows_to_rgba(&[0; 8], 2, 1, 7).is_err());
        assert!(bgra_rows_to_rgba(&[0; 7], 2, 1, 8).is_err());
    }

    #[test]
    fn invalid_filter_scales_use_the_display_fallback() {
        let fallback = ScaleFactor::new(2.0);
        assert_eq!(valid_scale_or(0.0, fallback), fallback);
        assert_eq!(valid_scale_or(f64::NAN, fallback), fallback);
        assert_eq!(valid_scale_or(1.5, fallback), ScaleFactor::new(1.5));
    }
}
