//! Opaque identity newtypes.
//!
//! These are driver-assigned handles, deliberately not platform values. A
//! `WindowId` is not an `XID` and not a `CGWindowID`; the platform value lives
//! in the adapter's own table. Leaking one would make a snapshot taken under
//! X11 look interchangeable with one taken under Wayland.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! opaque_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
            schemars::JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(u32);

        impl $name {
            #[must_use]
            pub const fn new(value: u32) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}", self.0)
            }
        }
    };
}

opaque_id! {
    /// Identifies a monitor within the current session.
    DisplayId
}

opaque_id! {
    /// Identifies a window within the current session.
    WindowId
}

opaque_id! {
    /// The small integer an agent sees in a snapshot: `[42] button "Save"`.
    ///
    /// Only meaningful together with the snapshot that issued it; resolving one
    /// always re-walks the live tree via its
    /// [`ElementPath`](crate::models::path::ElementPath).
    ElementId
}

impl DisplayId {
    pub const PRIMARY: Self = Self(0);
}

/// A process identifier. Both platforms key application identity on this.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct ProcessId(i32);

impl ProcessId {
    #[must_use]
    pub const fn new(value: i32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_ids_serialize_as_bare_integers_not_wrapper_objects() {
        assert_eq!(
            serde_json::to_string(&WindowId::new(123)).expect("serializes"),
            "123"
        );
        assert_eq!(
            serde_json::to_string(&ElementId::new(42)).expect("serializes"),
            "42"
        );
        assert_eq!(
            serde_json::to_string(&ProcessId::new(-1)).expect("serializes"),
            "-1"
        );
    }

    #[test]
    fn opaque_ids_round_trip_through_json() {
        let id = WindowId::new(9001);
        let json = serde_json::to_string(&id).expect("serializes");
        let back: WindowId = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, id);
    }

    #[test]
    fn display_renders_the_bare_number_for_use_in_error_messages() {
        assert_eq!(WindowId::new(123).to_string(), "123");
        assert_eq!(ProcessId::new(4242).to_string(), "4242");
    }
}
