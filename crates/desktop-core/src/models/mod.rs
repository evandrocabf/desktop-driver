//! Platform-independent domain models.
//!
//! Nothing here knows whether a value came from `AXUIElement` or AT-SPI. Where
//! a platform concept genuinely has no counterpart — an absolute screen
//! coordinate under Wayland — the model says so explicitly with an `Option`
//! rather than inventing a plausible number.

pub mod app;
pub mod backend;
pub mod capability;
pub mod chord;
pub mod dependency;
pub mod element;
pub mod geometry;
pub mod ids;
pub mod image;
pub mod path;
pub mod role;
pub mod selector;
pub mod snapshot;

pub use app::{AppKey, Application, Window, WindowKey};
pub use backend::{
    Backend, BackendInfo, DesktopEnvironment, DisplayServer, Platform, SessionFacts,
};
pub use capability::{Capability, CapabilitySet, CapabilityState, UnsupportedReason};
pub use chord::{Chord, ChordParseError, Key, Modifiers, NamedKey};
pub use dependency::{Need, PackageManager, SystemDependency};
pub use element::{Element, ElementAction, RawNode, States};
pub use geometry::{Bounds, CoordinateSpace, Point, ScaleFactor, ScrollDelta};
pub use ids::{DisplayId, ElementId, ProcessId, WindowId};
pub use image::{Image, ImageError, ScreenshotMetadata};
pub use path::{ElementPath, PathStep, StaleReason};
pub use role::Role;
pub use selector::{ActivationMode, ClickTarget, Selector, Target};
pub use snapshot::{Snapshot, WalkBudget};
