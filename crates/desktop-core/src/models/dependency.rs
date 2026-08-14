//! System packages `desktop-driver` needs but does not ship.
//!
//! A Rust binary can be built and copied anywhere; the desktop services it
//! drives cannot. Which of them are present decides what actually works, so
//! they are modelled and checked rather than left to a README nobody reads
//! before the first confusing failure.

use serde::{Deserialize, Serialize};

/// How urgently a dependency is needed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Need {
    /// Nothing works without it.
    Required,
    /// A specific capability is unavailable without it; the rest is fine.
    Recommended,
    /// Only needed for an opt-in mode.
    Optional,
}

impl Need {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Recommended => "recommended",
            Self::Optional => "optional",
        }
    }
}

/// One external package.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct SystemDependency {
    /// The command or service looked for.
    pub name: String,
    /// What stops working without it, in the user's terms.
    pub enables: String,
    pub need: Need,
    pub present: bool,
    /// The package to install on the detected distribution, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
}

impl SystemDependency {
    #[must_use]
    pub fn new(name: &str, enables: &str, need: Need, present: bool) -> Self {
        Self {
            name: name.to_owned(),
            enables: enables.to_owned(),
            need,
            present,
            package: None,
        }
    }

    #[must_use]
    pub fn with_package(mut self, package: Option<String>) -> Self {
        self.package = package;
        self
    }
}

/// The distribution's installer, used to print a command a user can paste.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PackageManager {
    Dnf,
    Apt,
    Pacman,
    Zypper,
    Homebrew,
    Unknown,
}

impl PackageManager {
    /// Chooses the installer from `/etc/os-release` identifiers.
    ///
    /// `ID_LIKE` is consulted as well, so derivatives (Linux Mint, Manjaro,
    /// Nobara) resolve to their parent's installer instead of falling through
    /// to a message with no command in it.
    #[must_use]
    pub fn from_os_release(id: &str, id_like: &str) -> Self {
        let haystack = format!("{id} {id_like}").to_ascii_lowercase();
        for token in haystack.split_whitespace() {
            let matched = match token.trim_matches('"') {
                "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => Some(Self::Dnf),
                "debian" | "ubuntu" | "linuxmint" | "pop" => Some(Self::Apt),
                "arch" | "archlinux" | "manjaro" | "endeavouros" => Some(Self::Pacman),
                "opensuse" | "suse" | "sles" | "opensuse-tumbleweed" => Some(Self::Zypper),
                _ => None,
            };
            if let Some(manager) = matched {
                return manager;
            }
        }
        Self::Unknown
    }

    /// The command that installs `packages`, or `None` when the distribution
    /// is unrecognised — better to say nothing than to print a command that
    /// will fail.
    #[must_use]
    pub fn install_command(self, packages: &[String]) -> Option<String> {
        if packages.is_empty() {
            return None;
        }
        let list = packages.join(" ");
        Some(match self {
            Self::Dnf => format!("sudo dnf install {list}"),
            Self::Apt => format!("sudo apt install {list}"),
            Self::Pacman => format!("sudo pacman -S {list}"),
            Self::Zypper => format!("sudo zypper install {list}"),
            Self::Homebrew => format!("brew install {list}"),
            Self::Unknown => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_major_distribution_families_resolve_to_an_installer() {
        assert_eq!(
            PackageManager::from_os_release("fedora", ""),
            PackageManager::Dnf
        );
        assert_eq!(
            PackageManager::from_os_release("ubuntu", "debian"),
            PackageManager::Apt
        );
        assert_eq!(
            PackageManager::from_os_release("arch", ""),
            PackageManager::Pacman
        );
        assert_eq!(
            PackageManager::from_os_release("opensuse-tumbleweed", "suse"),
            PackageManager::Zypper
        );
    }

    #[test]
    fn a_derivative_falls_back_to_its_parents_installer() {
        // Mint reports ID=linuxmint ID_LIKE=ubuntu; without ID_LIKE it would
        // print no command at all.
        assert_eq!(
            PackageManager::from_os_release("linuxmint", "ubuntu debian"),
            PackageManager::Apt
        );
        assert_eq!(
            PackageManager::from_os_release("nobara", "fedora"),
            PackageManager::Dnf
        );
    }

    #[test]
    fn os_release_quoting_is_tolerated() {
        assert_eq!(
            PackageManager::from_os_release("\"fedora\"", "\"\""),
            PackageManager::Dnf
        );
    }

    #[test]
    fn an_unknown_distribution_prints_no_command_rather_than_a_wrong_one() {
        let unknown = PackageManager::from_os_release("plan9", "");
        assert_eq!(unknown, PackageManager::Unknown);
        assert_eq!(unknown.install_command(&["Xvfb".to_owned()]), None);
    }

    #[test]
    fn install_commands_name_every_missing_package_in_one_line() {
        let packages = vec!["xorg-x11-server-Xvfb".to_owned(), "openbox".to_owned()];
        assert_eq!(
            PackageManager::Dnf.install_command(&packages),
            Some("sudo dnf install xorg-x11-server-Xvfb openbox".to_owned())
        );
    }

    #[test]
    fn nothing_missing_means_no_command_to_run() {
        assert_eq!(PackageManager::Dnf.install_command(&[]), None);
    }

    #[test]
    fn a_dependency_serializes_with_the_fields_an_agent_would_branch_on() {
        let dependency = SystemDependency::new(
            "Xvfb",
            "an isolated display the agent can drive without touching yours",
            Need::Optional,
            false,
        )
        .with_package(Some("xorg-x11-server-Xvfb".to_owned()));

        let json = serde_json::to_value(&dependency).expect("serializes");
        assert_eq!(json["name"], "Xvfb");
        assert_eq!(json["need"], "optional");
        assert_eq!(json["present"], false);
        assert_eq!(json["package"], "xorg-x11-server-Xvfb");
    }
}
