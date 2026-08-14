//! Captured images.

use serde::{Deserialize, Serialize};

use crate::models::geometry::{CoordinateSpace, ScaleFactor};

/// A captured image, in physical pixels.
///
/// The [`CoordinateSpace`] it was captured in travels with it, because on
/// Wayland a window capture and a screen capture have different origins and
/// nothing downstream can tell them apart otherwise.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub scale_factor: ScaleFactor,
    pub space: CoordinateSpace,
    /// Tightly packed RGBA8, `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

impl Image {
    /// Fails rather than truncating when the buffer does not match the stated
    /// dimensions — a stride mismatch from a capture backend would otherwise
    /// produce a silently skewed image.
    pub fn new(
        width: u32,
        height: u32,
        scale_factor: ScaleFactor,
        space: CoordinateSpace,
        pixels: Vec<u8>,
    ) -> Result<Self, ImageError> {
        let expected = usize::try_from(width)
            .ok()
            .zip(usize::try_from(height).ok())
            .and_then(|(w, h)| w.checked_mul(h))
            .and_then(|area| area.checked_mul(4))
            .ok_or(ImageError::DimensionsOverflow { width, height })?;

        if pixels.len() != expected {
            return Err(ImageError::BufferSizeMismatch {
                expected,
                actual: pixels.len(),
            });
        }

        Ok(Self {
            width,
            height,
            scale_factor,
            space,
            pixels,
        })
    }

    /// Logical size, i.e. pixels divided by the scale factor.
    #[must_use]
    pub fn logical_size(&self) -> (u32, u32) {
        let scale = self.scale_factor.get();
        (
            (f64::from(self.width) / scale).round() as u32,
            (f64::from(self.height) / scale).round() as u32,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ImageError {
    #[error("pixel buffer is {actual} bytes but {expected} were expected")]
    BufferSizeMismatch { expected: usize, actual: usize },
    #[error("image dimensions {width}x{height} overflow addressable memory")]
    DimensionsOverflow { width: u32, height: u32 },
}

/// What `desktop screenshot --json` reports.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ScreenshotMetadata {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
    pub space: CoordinateSpace,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ids::WindowId;

    #[test]
    fn a_correctly_sized_buffer_is_accepted() {
        let image = Image::new(
            2,
            2,
            ScaleFactor::ONE,
            CoordinateSpace::primary_screen(),
            vec![0; 16],
        )
        .expect("constructs");
        assert_eq!(image.width, 2);
        assert_eq!(image.pixels.len(), 16);
    }

    #[test]
    fn a_stride_mismatch_is_rejected_rather_than_producing_a_skewed_image() {
        let error = Image::new(
            2,
            2,
            ScaleFactor::ONE,
            CoordinateSpace::primary_screen(),
            vec![0; 12],
        )
        .expect_err("must reject");
        assert_eq!(
            error,
            ImageError::BufferSizeMismatch {
                expected: 16,
                actual: 12
            }
        );
    }

    #[test]
    fn absurd_dimensions_are_rejected_before_allocating() {
        let error = Image::new(
            u32::MAX,
            u32::MAX,
            ScaleFactor::ONE,
            CoordinateSpace::primary_screen(),
            Vec::new(),
        )
        .expect_err("must reject");
        assert!(matches!(error, ImageError::DimensionsOverflow { .. }));
    }

    #[test]
    fn logical_size_divides_out_hidpi_scaling() {
        let image = Image::new(
            4,
            4,
            ScaleFactor::new(2.0),
            CoordinateSpace::primary_screen(),
            vec![0; 64],
        )
        .expect("constructs");
        assert_eq!(image.logical_size(), (2, 2));
    }

    #[test]
    fn screenshot_metadata_matches_the_documented_json_shape() {
        let metadata = ScreenshotMetadata {
            path: "/tmp/desktop-driver-123.png".to_owned(),
            width: 1920,
            height: 1080,
            scale_factor: 2.0,
            space: CoordinateSpace::Window(WindowId::new(3)),
        };
        let json = serde_json::to_value(&metadata).expect("serializes");
        assert_eq!(json["path"], "/tmp/desktop-driver-123.png");
        assert_eq!(json["width"], 1920);
        assert_eq!(json["height"], 1080);
        assert_eq!(json["scale_factor"], 2.0);
        assert_eq!(json["space"]["window"], 3);
    }
}
