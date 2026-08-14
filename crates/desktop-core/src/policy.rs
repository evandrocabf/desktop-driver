//! Access policy.
//!
//! This tool hands an agent the same reach over a desktop that the person
//! sitting at it has. The policy engine is small in this version but it is
//! *positioned*: every action funnels through [`Policy::check`] in
//! [`crate::driver::Driver`], so tightening it later needs no changes anywhere
//! else.
//!
//! Note what is deliberately *not* here: password redaction. That is not a
//! policy — it is unconditional, and it lives in the snapshot normalizer where
//! it cannot be configured off.

use serde::{Deserialize, Serialize};

use crate::{
    errors::{DesktopError, Result},
    models::{app::AppKey, role::Role},
};

/// Whether an operation observes or changes the desktop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    Observe,
    Act,
}

/// The operations policy can distinguish.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    ListApps,
    ListWindows,
    Inspect,
    Snapshot,
    Screenshot,
    Capabilities,
    Focus,
    MoveMouse,
    Click,
    Type,
    Key,
    Scroll,
    /// Setting an element's text through the accessibility API, addressed by
    /// element rather than by whatever currently has focus.
    TypeIntoElement,
}

impl Action {
    #[must_use]
    pub const fn access(self) -> Access {
        match self {
            Self::ListApps
            | Self::ListWindows
            | Self::Inspect
            | Self::Snapshot
            | Self::Screenshot
            | Self::Capabilities => Access::Observe,
            Self::Focus
            | Self::MoveMouse
            | Self::Click
            | Self::Type
            | Self::Key
            | Self::Scroll
            | Self::TypeIntoElement => Access::Act,
        }
    }

    /// Whether this operation seizes a device the user is also holding.
    ///
    /// A desktop has one keyboard focus, one pointer and one screen. An agent
    /// that moves the pointer or types into "whatever is focused" is competing
    /// with the person sitting there, and loses races in both directions:
    /// keystrokes land in the wrong window, and the user's cursor jumps.
    ///
    /// Element-addressed operations go through the accessibility API instead,
    /// which touches none of those, so they stay allowed.
    ///
    /// Bare `type` and `key` do take over: they go wherever focus happens to be
    /// *now*, which is not necessarily where the agent looked a moment ago.
    /// `click` is conditional — through an accessibility action it is safe,
    /// through the pointer it is not — so the driver re-checks once it knows
    /// which it will use.
    #[must_use]
    pub const fn takes_over_input(self) -> bool {
        match self {
            Self::Focus | Self::MoveMouse | Self::Type | Self::Key | Self::Scroll => true,
            Self::Click => false,
            Self::ListApps
            | Self::ListWindows
            | Self::Inspect
            | Self::Snapshot
            | Self::Screenshot
            | Self::Capabilities
            | Self::TypeIntoElement => false,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ListApps => "apps",
            Self::ListWindows => "windows",
            Self::Inspect => "inspect",
            Self::Snapshot => "snapshot",
            Self::Screenshot => "screenshot",
            Self::Capabilities => "capabilities",
            Self::Focus => "focus",
            Self::MoveMouse => "move",
            Self::Click => "click",
            Self::Type => "type",
            Self::Key => "key",
            Self::Scroll => "scroll",
            Self::TypeIntoElement => "type",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Policy {
    /// Refuse everything that changes the desktop.
    #[serde(default)]
    pub read_only: bool,
    /// When non-empty, only these applications may be touched at all.
    #[serde(default)]
    pub allow_apps: Vec<String>,
    /// Always refused, even if also allowed. Deny wins.
    #[serde(default)]
    pub deny_apps: Vec<String>,
    /// Elements with these roles may not be acted on.
    #[serde(default)]
    pub deny_roles: Vec<Role>,
    /// Refuse anything that would seize the pointer or the keyboard focus the
    /// user is also using. Element-addressed work still goes through.
    #[serde(default)]
    pub no_steal_focus: bool,
}

impl Policy {
    #[must_use]
    pub fn permissive() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn read_only() -> Self {
        Self {
            read_only: true,
            ..Self::default()
        }
    }

    /// Refuses to contend with the user for the shared input devices.
    #[must_use]
    pub fn no_steal_focus() -> Self {
        Self {
            no_steal_focus: true,
            ..Self::default()
        }
    }

    /// Gate for an operation that would seize the pointer or keyboard focus.
    ///
    /// Called separately from [`Policy::check`] because whether a click steals
    /// input is only known once the driver has resolved how it will deliver it.
    pub fn check_exclusive_input(&self, action: Action) -> Result<()> {
        if self.no_steal_focus && action.takes_over_input() {
            return Err(DesktopError::PolicyDenied {
                action: action.as_str().to_owned(),
                subject: "--no-steal-focus: this would take the pointer or keyboard \
                          focus away from the user"
                    .to_owned(),
            });
        }
        Ok(())
    }

    /// The same gate for a click that has resolved to pointer synthesis.
    pub fn check_pointer_fallback(&self) -> Result<()> {
        if self.no_steal_focus {
            return Err(DesktopError::PolicyDenied {
                action: "click".to_owned(),
                subject: "--no-steal-focus: this element offers no accessibility \
                          action, so clicking it would move the user's pointer"
                    .to_owned(),
            });
        }
        Ok(())
    }

    /// Gate for an operation, optionally scoped to an application.
    ///
    /// Deny is evaluated first and is absolute: naming an application in both
    /// lists must not grant access to it.
    pub fn check(&self, action: Action, app: Option<&AppKey>) -> Result<()> {
        if self.read_only && action.access() == Access::Act {
            return Err(DesktopError::PolicyDenied {
                action: action.as_str().to_owned(),
                subject: "read-only mode".to_owned(),
            });
        }

        if let Some(app) = app {
            if self.deny_apps.iter().any(|pattern| app.matches(pattern)) {
                return Err(DesktopError::PolicyDenied {
                    action: action.as_str().to_owned(),
                    subject: app.name.clone(),
                });
            }
            if !self.allow_apps.is_empty()
                && !self.allow_apps.iter().any(|pattern| app.matches(pattern))
            {
                return Err(DesktopError::PolicyDenied {
                    action: action.as_str().to_owned(),
                    subject: app.name.clone(),
                });
            }
        }

        Ok(())
    }

    /// Gate for acting on a specific element.
    pub fn check_role(&self, action: Action, role: &Role) -> Result<()> {
        if self.deny_roles.contains(role) {
            return Err(DesktopError::PolicyDenied {
                action: action.as_str().to_owned(),
                subject: format!("role {}", role.as_str()),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ids::ProcessId;

    fn app(name: &str) -> AppKey {
        AppKey::new(ProcessId::new(1), name)
    }

    #[test]
    fn observation_and_action_are_partitioned_with_no_overlap() {
        let observe = [
            Action::ListApps,
            Action::ListWindows,
            Action::Inspect,
            Action::Snapshot,
            Action::Screenshot,
            Action::Capabilities,
        ];
        let act = [
            Action::Focus,
            Action::MoveMouse,
            Action::Click,
            Action::Type,
            Action::Key,
            Action::Scroll,
        ];
        for action in observe {
            assert_eq!(action.access(), Access::Observe, "{action:?}");
        }
        for action in act {
            assert_eq!(action.access(), Access::Act, "{action:?}");
        }
        assert_eq!(Action::TypeIntoElement.access(), Access::Act);
    }

    #[test]
    fn operations_that_seize_the_shared_devices_are_identified() {
        for action in [
            Action::Focus,
            Action::MoveMouse,
            Action::Type,
            Action::Key,
            Action::Scroll,
        ] {
            assert!(action.takes_over_input(), "{action:?} seizes input");
        }
    }

    #[test]
    fn element_addressed_work_does_not_seize_the_shared_devices() {
        // These go through the accessibility API: no keystrokes, no pointer,
        // no focus change. They are what makes sharing a desktop workable.
        for action in [
            Action::TypeIntoElement,
            Action::Snapshot,
            Action::Screenshot,
            Action::ListApps,
        ] {
            assert!(
                !action.takes_over_input(),
                "{action:?} must not seize input"
            );
        }
    }

    #[test]
    fn no_steal_focus_refuses_bare_typing_but_allows_addressed_typing() {
        let policy = Policy::no_steal_focus();
        assert!(policy.check_exclusive_input(Action::Type).is_err());
        assert!(policy.check_exclusive_input(Action::Key).is_err());
        assert!(policy.check_exclusive_input(Action::MoveMouse).is_err());
        assert!(policy.check_exclusive_input(Action::Focus).is_err());
        assert!(
            policy
                .check_exclusive_input(Action::TypeIntoElement)
                .is_ok()
        );
    }

    #[test]
    fn no_steal_focus_refuses_a_click_that_falls_back_to_the_pointer() {
        let policy = Policy::no_steal_focus();
        let error = policy.check_pointer_fallback().expect_err("must deny");
        let json = serde_json::to_value(&error).expect("serializes");
        assert_eq!(json["error"], "policy_denied");
        assert!(
            json["subject"]
                .as_str()
                .unwrap_or_default()
                .contains("pointer"),
            "the refusal should explain itself: {json}"
        );
    }

    #[test]
    fn the_default_policy_lets_the_agent_use_the_shared_devices() {
        let policy = Policy::permissive();
        assert!(policy.check_exclusive_input(Action::Type).is_ok());
        assert!(policy.check_pointer_fallback().is_ok());
    }

    #[test]
    fn read_only_permits_observation_and_refuses_every_action() {
        let policy = Policy::read_only();
        assert!(policy.check(Action::Snapshot, None).is_ok());
        assert!(policy.check(Action::Screenshot, None).is_ok());
        assert!(policy.check(Action::Click, None).is_err());
        assert!(policy.check(Action::Type, None).is_err());
        assert!(policy.check(Action::Focus, None).is_err());
    }

    #[test]
    fn a_denied_app_is_refused_even_when_it_is_also_allowed() {
        // Deny must be absolute, otherwise a broad allow-list silently
        // re-grants access to the one app the user meant to protect.
        let policy = Policy {
            allow_apps: vec!["1Password".to_owned()],
            deny_apps: vec!["1Password".to_owned()],
            ..Policy::default()
        };
        assert!(
            policy
                .check(Action::Snapshot, Some(&app("1Password")))
                .is_err()
        );
    }

    #[test]
    fn an_empty_allow_list_means_no_restriction_rather_than_deny_everything() {
        let policy = Policy::permissive();
        assert!(policy.check(Action::Click, Some(&app("Anything"))).is_ok());
    }

    #[test]
    fn a_non_empty_allow_list_excludes_everything_else() {
        let policy = Policy {
            allow_apps: vec!["Visual Studio Code".to_owned()],
            ..Policy::default()
        };
        assert!(
            policy
                .check(Action::Click, Some(&app("Visual Studio Code")))
                .is_ok()
        );
        assert!(policy.check(Action::Click, Some(&app("Firefox"))).is_err());
    }

    #[test]
    fn denied_roles_are_refused_for_actions() {
        let policy = Policy {
            deny_roles: vec![Role::PasswordField],
            ..Policy::default()
        };
        assert!(
            policy
                .check_role(Action::Click, &Role::PasswordField)
                .is_err()
        );
        assert!(policy.check_role(Action::Click, &Role::Button).is_ok());
    }

    #[test]
    fn a_policy_denial_reports_the_action_and_subject_it_refused() {
        let policy = Policy {
            deny_apps: vec!["1Password".to_owned()],
            ..Policy::default()
        };
        let error = policy
            .check(Action::Click, Some(&app("1Password")))
            .expect_err("must deny");
        let json = serde_json::to_value(&error).expect("serializes");
        assert_eq!(json["error"], "policy_denied");
        assert_eq!(json["action"], "click");
        assert_eq!(json["subject"], "1Password");
    }

    #[test]
    fn policy_round_trips_through_its_config_representation() {
        let policy = Policy {
            read_only: true,
            allow_apps: vec!["Code".to_owned()],
            deny_apps: vec!["1Password".to_owned()],
            deny_roles: vec![Role::PasswordField],
            no_steal_focus: true,
        };
        let json = serde_json::to_string(&policy).expect("serializes");
        let back: Policy = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, policy);
    }
}
