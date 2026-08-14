//! Turning policy flags into a [`Policy`].

use desktop_core::{models::role::Role, policy::Policy};

use crate::cli::Cli;

/// Builds the policy for this invocation.
///
/// Roles are parsed leniently: an unrecognised `--deny-role` token becomes
/// [`Role::Other`], which still matches a platform role of that exact name.
/// Rejecting it instead would make the flag useless for roles the normalizer
/// has not folded yet — and a deny-list that silently narrows is worse than
/// one that occasionally matches nothing.
#[must_use]
pub fn from_cli(cli: &Cli) -> Policy {
    Policy {
        read_only: cli.read_only,
        allow_apps: cli.allow_app.clone(),
        deny_apps: cli.deny_app.clone(),
        deny_roles: cli
            .deny_role
            .iter()
            .map(|token| Role::parse(token))
            .collect(),
        no_steal_focus: cli.no_steal_focus,
    }
}

/// Deny-role tokens that name no role this build knows.
///
/// Not an error, because [`from_cli`] keeps them on purpose — but worth saying
/// out loud. A misspelled entry in a deny-list produces a policy that protects
/// nothing, and the whole point of the flag is that the user believes it is
/// protecting something.
#[must_use]
pub fn unrecognised_roles(cli: &Cli) -> Vec<String> {
    cli.deny_role
        .iter()
        .filter(|token| matches!(Role::parse(token), Role::Other(_)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    fn policy_from(argv: &[&str]) -> Policy {
        from_cli(&Cli::try_parse_from(argv).expect("parses"))
    }

    #[test]
    fn the_default_policy_restricts_nothing() {
        let policy = policy_from(&["desktop", "snapshot"]);
        assert!(!policy.read_only);
        assert!(policy.allow_apps.is_empty());
        assert!(policy.deny_apps.is_empty());
        assert!(policy.deny_roles.is_empty());
        assert!(!policy.no_steal_focus);
    }

    #[test]
    fn read_only_is_carried_through_from_the_flag() {
        assert!(policy_from(&["desktop", "--read-only", "snapshot"]).read_only);
    }

    #[test]
    fn no_steal_focus_is_carried_through_from_the_flag() {
        assert!(policy_from(&["desktop", "--no-steal-focus", "snapshot"]).no_steal_focus);
        assert!(!policy_from(&["desktop", "snapshot"]).no_steal_focus);
    }

    #[test]
    fn repeated_app_flags_accumulate() {
        let policy = policy_from(&[
            "desktop",
            "--deny-app",
            "1Password",
            "--deny-app",
            "Keychain Access",
            "--allow-app",
            "Code",
            "snapshot",
        ]);
        assert_eq!(policy.deny_apps.len(), 2);
        assert_eq!(policy.allow_apps, vec!["Code".to_owned()]);
    }

    #[test]
    fn a_known_role_name_is_normalized() {
        let policy = policy_from(&["desktop", "--deny-role", "password", "snapshot"]);
        assert_eq!(policy.deny_roles, vec![Role::PasswordField]);
    }

    #[test]
    fn a_misspelled_deny_role_is_reported_rather_than_silently_protecting_nothing() {
        let cli = Cli::try_parse_from([
            "desktop",
            "--deny-role",
            "pasword",
            "--deny-role",
            "password",
            "snapshot",
        ])
        .expect("parses");
        // A typo matches nothing at all, and nothing says so without this.
        // Note "password_field" is *not* a typo: role names are compared with
        // punctuation stripped, so it resolves to the same role as "password".
        assert_eq!(unrecognised_roles(&cli), vec!["pasword".to_owned()]);
    }

    #[test]
    fn a_correct_deny_list_produces_no_noise() {
        let cli =
            Cli::try_parse_from(["desktop", "--deny-role", "button", "snapshot"]).expect("parses");
        assert!(unrecognised_roles(&cli).is_empty());
    }

    #[test]
    fn an_unknown_role_is_preserved_verbatim_rather_than_dropped() {
        // Dropping it would silently widen the policy, which is the dangerous
        // direction for a deny-list.
        let policy = policy_from(&["desktop", "--deny-role", "AXMysteryThing", "snapshot"]);
        assert_eq!(
            policy.deny_roles,
            vec![Role::Other("AXMysteryThing".to_owned())]
        );
    }
}
