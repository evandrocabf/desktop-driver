//! The external packages this adapter depends on.
//!
//! Two groups, and the distinction matters:
//!
//! * **Driving the desktop you are already looking at** needs only services a
//!   modern desktop already ships — the accessibility bus and, under Wayland,
//!   `xdg-desktop-portal`. Nothing extra to install.
//! * **Giving the agent a screen of its own** needs a virtual display, and that
//!   is not installed by default anywhere.
//!
//! The isolated display is X11 (`Xvfb`) rather than a Wayland compositor
//! because desktop-driver's X11 backend is complete: capture through
//! `GetImage`, input through XTEST, and — unlike Wayland — a window can
//! actually be raised. A nested wlroots compositor would need capture and
//! input paths that do not exist yet.

use std::collections::BTreeMap;

use desktop_core::models::{
    backend::{BackendInfo, DisplayServer},
    dependency::{Need, PackageManager, SystemDependency},
};

/// One row of the dependency table, before presence is resolved.
struct Spec {
    /// The command looked for on `PATH`, or `None` when the package ships no
    /// binary that lands there and presence is decided another way.
    binary: Option<&'static str>,
    name: &'static str,
    enables: &'static str,
    need: Need,
    /// Package name per installer. Distributions disagree, so a single string
    /// would be wrong for most users.
    packages: &'static [(PackageManager, &'static str)],
}

const AT_SPI: Spec = Spec {
    binary: None,
    name: "at-spi2-core",
    enables: "reading the accessibility tree — snapshots, selectors, element actions",
    need: Need::Required,
    packages: &[
        (PackageManager::Dnf, "at-spi2-core"),
        (PackageManager::Apt, "at-spi2-core"),
        (PackageManager::Pacman, "at-spi2-core"),
        (PackageManager::Zypper, "at-spi2-core"),
    ],
};

const PORTAL: Spec = Spec {
    binary: None,
    name: "xdg-desktop-portal",
    enables: "screenshots and input under Wayland",
    need: Need::Recommended,
    packages: &[
        (
            PackageManager::Dnf,
            "xdg-desktop-portal xdg-desktop-portal-gnome",
        ),
        (
            PackageManager::Apt,
            "xdg-desktop-portal xdg-desktop-portal-gnome",
        ),
        (
            PackageManager::Pacman,
            "xdg-desktop-portal xdg-desktop-portal-gnome",
        ),
        (
            PackageManager::Zypper,
            "xdg-desktop-portal xdg-desktop-portal-gnome",
        ),
    ],
};

const XVFB: Spec = Spec {
    binary: Some("Xvfb"),
    name: "Xvfb",
    enables: "`desktop session` — a display of the agent's own, where screenshots \
              contain only the agent's windows and input does not fight you for \
              the pointer",
    need: Need::Optional,
    packages: &[
        (PackageManager::Dnf, "xorg-x11-server-Xvfb"),
        (PackageManager::Apt, "xvfb"),
        (PackageManager::Pacman, "xorg-server-xvfb"),
        (PackageManager::Zypper, "xorg-x11-server-Xvfb"),
    ],
};

const WINDOW_MANAGER: Spec = Spec {
    binary: Some("openbox"),
    name: "openbox",
    enables: "window management inside the agent's display, so `desktop focus` \
              and window stacking behave the way applications expect",
    need: Need::Optional,
    packages: &[
        (PackageManager::Dnf, "openbox"),
        (PackageManager::Apt, "openbox"),
        (PackageManager::Pacman, "openbox"),
        (PackageManager::Zypper, "openbox"),
    ],
};

/// Watching is optional, but a user who cannot see what an agent is doing has
/// to take its word for it.
///
/// Every package name below was verified by installing it and checking that an
/// Xephyr binary appeared; openSUSE in particular does not follow the naming
/// the other three share.
const XEPHYR: Spec = Spec {
    binary: Some("Xephyr"),
    name: "Xephyr",
    enables: "`desktop session start --visible` — the agent's screen in a window you \
              can watch, and click into to take over",
    need: Need::Optional,
    packages: &[
        (PackageManager::Dnf, "xorg-x11-server-Xephyr"),
        (PackageManager::Apt, "xserver-xephyr"),
        (PackageManager::Pacman, "xorg-server-xephyr"),
        (PackageManager::Zypper, "xorg-x11-server-extra"),
    ],
};

/// The private bus is what separates the agent's accessibility tree from the
/// user's; without it the two sets of applications share one registry.
const DBUS_DAEMON: Spec = Spec {
    binary: Some("dbus-daemon"),
    name: "dbus-daemon",
    enables: "the agent display's private session bus, which is what keeps its \
              accessibility tree separate from yours",
    need: Need::Optional,
    packages: &[
        (PackageManager::Dnf, "dbus-daemon"),
        (PackageManager::Apt, "dbus-bin"),
        (PackageManager::Pacman, "dbus"),
        (PackageManager::Zypper, "dbus-1-daemon"),
    ],
};

const SPECS: [Spec; 6] = [AT_SPI, PORTAL, XVFB, XEPHYR, WINDOW_MANAGER, DBUS_DAEMON];

/// The dependency table with presence resolved against this machine.
///
/// Presence means *installed*, never *working*. The two come apart constantly —
/// a package can be present with its service down, and saying "missing" then
/// sends the user to install what they already have. Verified in a bare Debian
/// container: at-spi2-core was installed while `org.a11y.Bus` was unreachable,
/// because there was no session bus to reach it on. Whether the bus answers is
/// a separate question, and `desktop doctor` asks it in the diagnostics.
#[must_use]
pub fn dependencies(info: &BackendInfo) -> Vec<SystemDependency> {
    let manager = package_manager();

    SPECS
        .iter()
        .filter(|spec| applies(spec, info))
        .map(|spec| {
            let present = match spec.binary {
                Some(binary) => on_path(binary),
                None if spec.name == "at-spi2-core" => {
                    crate::session::helper_installed("at-spi-bus-launcher")
                }
                None => info.screenshot != desktop_core::models::backend::Backend::None,
            };
            SystemDependency::new(spec.name, spec.enables, spec.need, present)
                .with_package(package_for(spec, manager))
        })
        .collect()
}

/// The command that installs everything missing, or `None` when nothing is.
///
/// Packages are deduplicated while keeping the declared order, so the command
/// reads the way the table does.
#[must_use]
pub fn install_command(info: &BackendInfo) -> Option<String> {
    let manager = package_manager();
    let missing: Vec<String> = dependencies(info)
        .into_iter()
        .filter(|dependency| !dependency.present)
        .filter_map(|dependency| dependency.package)
        .flat_map(|package| {
            package
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();

    let mut seen = BTreeMap::new();
    let mut ordered = Vec::new();
    for package in missing {
        if seen.insert(package.clone(), ()).is_none() {
            ordered.push(package);
        }
    }
    manager.install_command(&ordered)
}

/// Portals are Wayland-only; recommending them under X11 is noise.
fn applies(spec: &Spec, info: &BackendInfo) -> bool {
    if spec.name == "xdg-desktop-portal" {
        return info.display_server == DisplayServer::Wayland;
    }
    true
}

fn package_for(spec: &Spec, manager: PackageManager) -> Option<String> {
    spec.packages
        .iter()
        .find(|(candidate, _)| *candidate == manager)
        .map(|(_, package)| (*package).to_owned())
}

/// Reads `/etc/os-release` to pick the installer.
#[must_use]
pub fn package_manager() -> PackageManager {
    let Ok(contents) = std::fs::read_to_string("/etc/os-release") else {
        return PackageManager::Unknown;
    };
    let value = |key: &str| -> String {
        contents
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap_or_default()
            .trim_matches('"')
            .to_owned()
    };
    PackageManager::from_os_release(&value("ID="), &value("ID_LIKE="))
}

/// Whether a command exists on `PATH`.
fn on_path(binary: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| directory.join(binary).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_core::models::backend::{Backend, DesktopEnvironment, Platform};

    fn info(display_server: DisplayServer) -> BackendInfo {
        BackendInfo {
            platform: Platform::Linux,
            display_server,
            desktop_environment: DesktopEnvironment::Gnome,
            accessibility: Backend::AtSpi,
            windows: Backend::AtSpi,
            screenshot: Backend::PortalScreenCast,
            input: Backend::RemoteDesktopPortal,
        }
    }

    #[test]
    fn the_accessibility_bus_is_required_because_nothing_works_without_it() {
        let table = dependencies(&info(DisplayServer::Wayland));
        let atspi = table
            .iter()
            .find(|d| d.name == "at-spi2-core")
            .expect("at-spi2-core is always listed");
        assert_eq!(atspi.need, Need::Required);
        assert!(atspi.present, "the probe said the bus was reachable");
    }

    #[test]
    fn portals_are_only_mentioned_under_wayland() {
        let wayland = dependencies(&info(DisplayServer::Wayland));
        assert!(wayland.iter().any(|d| d.name == "xdg-desktop-portal"));

        // Under X11 they are irrelevant, and listing them would send a user
        // installing packages that change nothing.
        let x11 = dependencies(&info(DisplayServer::X11));
        assert!(!x11.iter().any(|d| d.name == "xdg-desktop-portal"));
    }

    #[test]
    fn the_virtual_display_is_optional_and_listed_on_every_display_server() {
        // It is what an isolated agent session needs, which is a choice the
        // user makes rather than a prerequisite for the tool running at all.
        for server in [DisplayServer::Wayland, DisplayServer::X11] {
            let table = dependencies(&info(server));
            let xvfb = table
                .iter()
                .find(|d| d.name == "Xvfb")
                .expect("Xvfb is always listed");
            assert_eq!(xvfb.need, Need::Optional);
        }
    }

    #[test]
    fn everything_an_agent_display_needs_is_listed_together() {
        // A session that starts four services out of five leaves an orphaned X
        // server and no working tree, so all five have to be checkable up
        // front rather than discovered one failure at a time.
        let table = dependencies(&info(DisplayServer::Wayland));
        for required in ["Xvfb", "openbox", "dbus-daemon", "at-spi2-core"] {
            assert!(
                table.iter().any(|d| d.name == required),
                "{required} is needed for `desktop session` but is not listed"
            );
        }
    }

    #[test]
    fn every_dependency_names_what_it_enables_in_the_users_terms() {
        for dependency in dependencies(&info(DisplayServer::Wayland)) {
            assert!(
                !dependency.enables.is_empty(),
                "{} must explain itself",
                dependency.name
            );
            assert!(
                !dependency.enables.starts_with("lib"),
                "{} explains a library, not a capability",
                dependency.name
            );
        }
    }

    #[test]
    fn a_missing_dependency_carries_the_package_for_this_distribution() {
        let table = dependencies(&info(DisplayServer::Wayland));
        let xvfb = table.iter().find(|d| d.name == "Xvfb").expect("listed");
        if package_manager() != PackageManager::Unknown {
            assert!(
                xvfb.package.is_some(),
                "a recognised distribution should name the package"
            );
        }
    }

    #[test]
    fn the_install_command_lists_each_package_once() {
        let command = install_command(&info(DisplayServer::Wayland));
        if let Some(command) = command {
            let packages: Vec<&str> = command.split_whitespace().skip(3).collect();
            let mut unique = packages.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(
                packages.len(),
                unique.len(),
                "duplicate package in {command}"
            );
        }
    }

    #[test]
    fn nothing_missing_yields_no_install_command() {
        // Presence is resolved live, so this asserts the shape rather than a
        // particular machine's state.
        let table = dependencies(&info(DisplayServer::Wayland));
        if table.iter().all(|d| d.present) {
            assert_eq!(install_command(&info(DisplayServer::Wayland)), None);
        }
    }
}
