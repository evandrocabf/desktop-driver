//! Coordinates are always logical units. The space they belong to is carried
//! alongside them because on GNOME Wayland a window-relative origin is the only
//! one that exists, and an untagged number there is a silent wrong answer.

use serde::{Deserialize, Serialize};

use crate::models::ids::{DisplayId, WindowId};

/// A point in logical units, meaningful only within a [`CoordinateSpace`].
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize, schemars::JsonSchema,
)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned rectangle in logical units.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize, schemars::JsonSchema,
)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Bounds {
    #[must_use]
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// A rectangle with no area carries no clickable target, and toolkits emit
    /// many such nodes for layout scaffolding.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    #[must_use]
    pub const fn center(&self) -> Point {
        Point::new(self.x + self.width / 2, self.y + self.height / 2)
    }

    #[must_use]
    pub const fn contains(&self, p: Point) -> bool {
        p.x >= self.x
            && p.y >= self.y
            && p.x < self.x.saturating_add(self.width)
            && p.y < self.y.saturating_add(self.height)
    }

    /// Translate into the coordinate space whose origin sits at `origin` in this
    /// rectangle's own space.
    #[must_use]
    pub const fn relative_to(&self, origin: Point) -> Self {
        Self {
            x: self.x - origin.x,
            y: self.y - origin.y,
            width: self.width,
            height: self.height,
        }
    }

    /// Translate out of a space whose origin sits at `origin` in the target space.
    #[must_use]
    pub const fn offset_by(&self, origin: Point) -> Self {
        Self {
            x: self.x + origin.x,
            y: self.y + origin.y,
            width: self.width,
            height: self.height,
        }
    }
}

/// Which origin a [`Point`] or [`Bounds`] is measured from.
///
/// `Window` is not a degraded form of `Screen`. Under Wayland it is the only
/// space that exists, and the ScreenCast window stream shares it exactly, so
/// element bounds map onto pointer input with an identity transform.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateSpace {
    Screen(DisplayId),
    Window(WindowId),
}

impl CoordinateSpace {
    #[must_use]
    pub const fn primary_screen() -> Self {
        Self::Screen(DisplayId::PRIMARY)
    }

    #[must_use]
    pub const fn is_window_relative(&self) -> bool {
        matches!(self, Self::Window { .. })
    }
}

/// Ratio of physical pixels to logical units. HiDPI displays report 2.0; the
/// value is never zero, so division by it is always safe.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ScaleFactor(f64);

impl ScaleFactor {
    pub const ONE: Self = Self(1.0);

    /// Non-finite and non-positive scales come from broken display drivers and
    /// would poison every downstream transform, so they collapse to 1.0.
    #[must_use]
    pub fn new(value: f64) -> Self {
        if value.is_finite() && value > 0.0 {
            Self(value)
        } else {
            Self::ONE
        }
    }

    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }

    #[must_use]
    pub fn logical_to_pixel(self, point: Point) -> Point {
        Point::new(
            (f64::from(point.x) * self.0).round() as i32,
            (f64::from(point.y) * self.0).round() as i32,
        )
    }

    #[must_use]
    pub fn pixel_to_logical(self, point: Point) -> Point {
        Point::new(
            (f64::from(point.x) / self.0).round() as i32,
            (f64::from(point.y) / self.0).round() as i32,
        )
    }

    #[must_use]
    pub fn logical_bounds_to_pixel(self, bounds: Bounds) -> Bounds {
        Bounds::new(
            (f64::from(bounds.x) * self.0).round() as i32,
            (f64::from(bounds.y) * self.0).round() as i32,
            (f64::from(bounds.width) * self.0).round() as i32,
            (f64::from(bounds.height) * self.0).round() as i32,
        )
    }
}

impl Default for ScaleFactor {
    fn default() -> Self {
        Self::ONE
    }
}

/// A scroll amount in logical units. Positive `y` scrolls content down.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize, schemars::JsonSchema,
)]
pub struct ScrollDelta {
    pub x: i32,
    pub y: i32,
}

impl ScrollDelta {
    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.x == 0 && self.y == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bounds_are_detected_for_zero_and_negative_extents() {
        assert!(Bounds::new(0, 0, 0, 10).is_empty());
        assert!(Bounds::new(0, 0, 10, 0).is_empty());
        assert!(Bounds::new(0, 0, -1, 10).is_empty());
        assert!(!Bounds::new(0, 0, 1, 1).is_empty());
    }

    #[test]
    fn center_of_bounds_lands_inside_the_rectangle() {
        let b = Bounds::new(1100, 700, 80, 32);
        let c = b.center();
        assert_eq!(c, Point::new(1140, 716));
        assert!(b.contains(c));
    }

    #[test]
    fn contains_excludes_the_far_edges_so_adjacent_rectangles_do_not_overlap() {
        let b = Bounds::new(0, 0, 10, 10);
        assert!(b.contains(Point::new(0, 0)));
        assert!(b.contains(Point::new(9, 9)));
        assert!(!b.contains(Point::new(10, 5)));
        assert!(!b.contains(Point::new(5, 10)));
    }

    #[test]
    fn relative_to_and_offset_by_are_inverse_operations() {
        let screen = Bounds::new(1230, 772, 90, 32);
        let origin = Point::new(0, 32);
        let window = screen.relative_to(origin);
        assert_eq!(window, Bounds::new(1230, 740, 90, 32));
        assert_eq!(window.offset_by(origin), screen);
    }

    #[test]
    fn scale_factor_rejects_nonsense_values_from_broken_drivers() {
        assert_eq!(ScaleFactor::new(0.0).get(), 1.0);
        assert_eq!(ScaleFactor::new(-2.0).get(), 1.0);
        assert_eq!(ScaleFactor::new(f64::NAN).get(), 1.0);
        assert_eq!(ScaleFactor::new(f64::INFINITY).get(), 1.0);
        assert_eq!(ScaleFactor::new(2.0).get(), 2.0);
    }

    #[test]
    fn hidpi_round_trip_preserves_logical_coordinates() {
        let scale = ScaleFactor::new(2.0);
        let logical = Point::new(800, 400);
        let pixel = scale.logical_to_pixel(logical);
        assert_eq!(pixel, Point::new(1600, 800));
        assert_eq!(scale.pixel_to_logical(pixel), logical);
    }

    #[test]
    fn fractional_scaling_rounds_rather_than_truncating() {
        let scale = ScaleFactor::new(1.5);
        assert_eq!(scale.logical_to_pixel(Point::new(3, 5)), Point::new(5, 8));
        assert_eq!(
            scale.logical_bounds_to_pixel(Bounds::new(1, 1, 3, 5)),
            Bounds::new(2, 2, 5, 8)
        );
    }

    #[test]
    fn window_relative_space_is_distinguishable_from_screen_space() {
        assert!(CoordinateSpace::Window(WindowId::new(3)).is_window_relative());
        assert!(!CoordinateSpace::primary_screen().is_window_relative());
    }

    #[test]
    fn coordinate_space_serializes_with_the_documented_wire_shape() {
        let space = CoordinateSpace::Window(WindowId::new(3));
        let json = serde_json::to_string(&space).expect("serializes");
        assert_eq!(json, r#"{"window":3}"#);
    }
}
