//! What the current environment can actually do.
//!
//! Two states are not enough to be honest. Capturing a named window on GNOME
//! Wayland works — but only after a human has picked that window in a portal
//! dialog. Reporting that as "supported" misleads an agent into a hang;
//! reporting it as "unsupported" wastes a capability that does work. Hence
//! [`CapabilityState::Degraded`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Accessibility,
    Windows,
    Screenshots,
    WindowScreenshots,
    Mouse,
    Keyboard,
    Scroll,
    Focus,
    ElementActions,
    /// Setting an element's text without keystrokes, focus changes or pointer
    /// movement.
    ElementText,
    /// Giving the agent a display of its own, rather than sharing the user's.
    AgentSession,
}

impl Capability {
    pub const ALL: [Self; 11] = [
        Self::Accessibility,
        Self::Windows,
        Self::Screenshots,
        Self::WindowScreenshots,
        Self::Mouse,
        Self::Keyboard,
        Self::Scroll,
        Self::Focus,
        Self::ElementActions,
        Self::ElementText,
        Self::AgentSession,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accessibility => "accessibility",
            Self::Windows => "windows",
            Self::Screenshots => "screenshots",
            Self::WindowScreenshots => "window_screenshots",
            Self::Mouse => "mouse",
            Self::Keyboard => "keyboard",
            Self::Scroll => "scroll",
            Self::Focus => "focus",
            Self::ElementActions => "element_actions",
            Self::ElementText => "element_text",
            Self::AgentSession => "agent_session",
        }
    }
}

/// Why a capability is unavailable, in terms an agent can branch on.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedReason {
    /// The compositor or desktop provides no mechanism at all.
    NoBackendMechanism,
    /// A required service is not reachable (the a11y bus, a portal).
    ServiceUnavailable { service: String },
    /// The mechanism exists but this build does not implement it yet.
    NotImplemented,
    /// An OS permission is missing. Distinct from the others because the user
    /// can fix it.
    PermissionMissing { permission: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    /// Works, with a caveat the caller must know about.
    Degraded {
        note: String,
    },
    Unsupported {
        #[serde(flatten)]
        reason: UnsupportedReason,
    },
}

impl CapabilityState {
    #[must_use]
    pub fn degraded(note: &str) -> Self {
        Self::Degraded {
            note: note.to_owned(),
        }
    }

    #[must_use]
    pub const fn unsupported(reason: UnsupportedReason) -> Self {
        Self::Unsupported { reason }
    }

    #[must_use]
    pub const fn not_implemented() -> Self {
        Self::Unsupported {
            reason: UnsupportedReason::NotImplemented,
        }
    }

    /// Whether an operation may proceed. Degraded still means yes — the caveat
    /// is surfaced through `capabilities`, not by refusing the call.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Supported | Self::Degraded { .. })
    }

    /// The glyph used by `desktop capabilities` in human mode.
    #[must_use]
    pub const fn glyph(&self) -> char {
        match self {
            Self::Supported => '✓',
            Self::Degraded { .. } => '~',
            Self::Unsupported { .. } => '✗',
        }
    }
}

/// The full capability picture for the selected backends.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct CapabilitySet(BTreeMap<Capability, CapabilityState>);

impl CapabilitySet {
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    #[must_use]
    pub fn with(mut self, capability: Capability, state: CapabilityState) -> Self {
        self.0.insert(capability, state);
        self
    }

    pub fn set(&mut self, capability: Capability, state: CapabilityState) {
        self.0.insert(capability, state);
    }

    /// Anything never declared is unsupported. A backend that forgets to
    /// mention a capability therefore fails closed.
    #[must_use]
    pub fn get(&self, capability: Capability) -> CapabilityState {
        self.0
            .get(&capability)
            .cloned()
            .unwrap_or_else(|| CapabilityState::unsupported(UnsupportedReason::NoBackendMechanism))
    }

    #[must_use]
    pub fn is_available(&self, capability: Capability) -> bool {
        self.get(capability).is_available()
    }

    pub fn iter(&self) -> impl Iterator<Item = (Capability, &CapabilityState)> {
        self.0.iter().map(|(cap, state)| (*cap, state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_undeclared_capability_fails_closed() {
        // A backend that forgets to declare something must not accidentally
        // appear to support it.
        let set = CapabilitySet::new();
        assert!(!set.is_available(Capability::Mouse));
        assert_eq!(
            set.get(Capability::Mouse),
            CapabilityState::unsupported(UnsupportedReason::NoBackendMechanism)
        );
    }

    #[test]
    fn degraded_capabilities_are_still_available_to_call() {
        let set = CapabilitySet::new().with(
            Capability::WindowScreenshots,
            CapabilityState::degraded("requires the portal window picker on first use"),
        );
        assert!(set.is_available(Capability::WindowScreenshots));
    }

    #[test]
    fn unsupported_capabilities_are_not_available() {
        let set = CapabilitySet::new().with(Capability::Mouse, CapabilityState::not_implemented());
        assert!(!set.is_available(Capability::Mouse));
    }

    #[test]
    fn glyphs_distinguish_all_three_states_in_human_output() {
        assert_eq!(CapabilityState::Supported.glyph(), '✓');
        assert_eq!(CapabilityState::degraded("x").glyph(), '~');
        assert_eq!(CapabilityState::not_implemented().glyph(), '✗');
    }

    #[test]
    fn capability_state_serializes_with_a_flattened_reason() {
        let json = serde_json::to_value(CapabilityState::unsupported(
            UnsupportedReason::ServiceUnavailable {
                service: "org.a11y.Bus".to_owned(),
            },
        ))
        .expect("serializes");
        assert_eq!(json["state"], "unsupported");
        assert_eq!(json["service_unavailable"]["service"], "org.a11y.Bus");
    }

    #[test]
    fn capability_set_iterates_in_a_stable_order_so_output_is_diffable() {
        let set = CapabilitySet::new()
            .with(Capability::Scroll, CapabilityState::Supported)
            .with(Capability::Accessibility, CapabilityState::Supported)
            .with(Capability::Mouse, CapabilityState::Supported);
        let order: Vec<Capability> = set.iter().map(|(cap, _)| cap).collect();
        assert_eq!(
            order,
            vec![
                Capability::Accessibility,
                Capability::Mouse,
                Capability::Scroll
            ]
        );
    }
}
