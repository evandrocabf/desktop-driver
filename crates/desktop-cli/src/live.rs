//! End-to-end smoke tests against a real desktop.
//!
//! `#[ignore]`d, because they launch an application and drive it. A launched
//! application appears on whatever display the test process inherited, so run
//! from an ordinary terminal these open a window over the user's work and
//! compete with them for keyboard focus. Give them a display of their own:
//!
//! ```text
//! desktop session start --headless
//! eval "$(desktop session env)"
//! cargo test --workspace -- --ignored
//! ```
//!
//! `session env` rather than `session run`, because `run` launches detached and
//! the test results would never come back.
//!
//! [`Calculator::launch`] refuses to start anything otherwise, and says which
//! of the two displays the one in hand is.
//!
//! The same test shape runs on both platforms — `gnome-calculator` on Linux and
//! `Calculator.app` on macOS — because if the normalization layer is doing its
//! job, one sequence of commands should produce one result everywhere.

#![cfg(test)]

use std::{
    process::Command,
    sync::{Mutex, MutexGuard},
    thread::sleep,
    time::Duration,
};

use desktop_core::{
    Driver, SessionStore,
    models::{
        role::Role,
        selector::{ActivationMode, Selector, Target},
        snapshot::WalkBudget,
    },
    policy::Policy,
    ports::MouseButton,
};

#[cfg(target_os = "linux")]
const CALCULATOR_COMMAND: &str = "gnome-calculator";
#[cfg(target_os = "linux")]
const CALCULATOR_APP: &str = "gnome-calculator";

#[cfg(target_os = "macos")]
const CALCULATOR_COMMAND: &str = "/System/Applications/Calculator.app/Contents/MacOS/Calculator";
#[cfg(target_os = "macos")]
const CALCULATOR_APP: &str = "Calculator";

/// How long to give the application to appear on the accessibility bus.
///
/// Generous because the slowest case is the one that matters: the first launch
/// inside a fresh agent session renders through llvmpipe with no DRI3 and a
/// cold page cache, and takes several times what the same application takes on
/// the user's accelerated desktop. A tight bound here does not fail, it skips —
/// which reads as "the test ran" while nothing was checked.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(45);

/// Serializes the tests that drive the calculator.
///
/// `gnome-calculator` and `Calculator.app` are both single-instance: a second
/// launch hands its arguments to the first and exits, so two tests running at
/// once share one window, and whichever finishes first kills it out from under
/// the other. Held for the lifetime of a [`Calculator`], so the next test waits
/// rather than joining.
static ONE_CALCULATOR_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Set this to launch onto the display you are looking at anyway.
///
/// Named for what it costs rather than for what it enables, because that is the
/// question worth asking before setting it. On macOS it is the only way to run
/// these at all: there is no session mechanism, and the only display is yours.
const LAUNCH_ON_MY_DESKTOP: &str = "DESKTOP_DRIVER_LIVE_ON_MY_DESKTOP";

/// The display of the agent session, if one is running.
#[cfg(target_os = "linux")]
fn agent_session_display() -> Option<String> {
    desktop_core::agent::AgentSessionStore::at_default_path()
        .load()
        .map(|session| session.display)
}

#[cfg(not(target_os = "linux"))]
fn agent_session_display() -> Option<String> {
    None
}

/// Why launching an application here would be the wrong thing to do, if it
/// would be.
///
/// The failure this prevents is not a flaky test, it is a window appearing over
/// somebody's work and stealing the keystrokes they were in the middle of
/// typing — from a command whose name gives no hint that it drives the desktop.
/// So the default is to refuse, and the display has to be one the agent owns.
///
/// Returns `None` when launching is allowed.
fn reason_not_to_launch() -> Option<String> {
    if std::env::var_os(LAUNCH_ON_MY_DESKTOP).is_some() {
        return None;
    }

    let display = std::env::var("DISPLAY").unwrap_or_default();
    let Some(session) = agent_session_display() else {
        return Some(format!(
            "no agent session is running, so this would launch onto your own desktop. \
             Start one with `desktop session start --headless`, apply it with \
             `eval \"$(desktop session env)\"`, or set {LAUNCH_ON_MY_DESKTOP}=1 to \
             accept a window opening over your work"
        ));
    };
    if display.is_empty() || display != session {
        return Some(format!(
            "DISPLAY is `{display}` but the agent session is on `{session}`, so this would \
             launch onto your own desktop. Apply the session with \
             `eval \"$(desktop session env)\"`, or set {LAUNCH_ON_MY_DESKTOP}=1 to accept a \
             window opening over your work"
        ));
    }
    None
}

struct Calculator {
    child: std::process::Child,
    driver: Driver,
    /// Dropped last, after the window is gone, so the next test starts from no
    /// calculator rather than from this one.
    _serialized: MutexGuard<'static, ()>,
}

impl Calculator {
    /// Starts the calculator and waits for its tree to appear.
    ///
    /// Returns `None` — a skip, not a failure — and prints which of the four
    /// reasons it was: no display the agent owns to start on, no backend on
    /// this target, the program not installed, or a window that never reached
    /// the accessibility bus. Callers do not add a reason of their own, because
    /// only this function knows which one happened.
    ///
    /// Everything that can refuse does so *before* the spawn, and the one
    /// remaining `None` after it kills the process first. Dropping a
    /// [`std::process::Child`] does not kill it, so an early return past a live
    /// one leaves a window that nothing will ever close.
    ///
    /// Polls rather than sleeping a fixed time: application start-up varies by
    /// an order of magnitude between a warm and a cold page cache.
    fn launch() -> Option<Self> {
        if let Some(reason) = reason_not_to_launch() {
            eprintln!("skipping: {reason}");
            return None;
        }

        let serialized = ONE_CALCULATOR_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let Some(ports) = platform_ports() else {
            eprintln!("skipping: no platform backend on this target");
            return None;
        };

        let mut child = match Command::new(CALCULATOR_COMMAND)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("skipping: cannot start {CALCULATOR_COMMAND}: {error}");
                return None;
            }
        };

        let store = SessionStore::new(
            std::env::temp_dir()
                .join(format!("desktop-driver-live-{}", std::process::id()))
                .join("snapshot.json"),
        );
        let driver = Driver::new(ports, Policy::permissive(), store);

        let deadline = std::time::Instant::now() + LAUNCH_TIMEOUT;
        while std::time::Instant::now() < deadline {
            if driver
                .snapshot(
                    &Target::App(CALCULATOR_APP.to_owned()),
                    WalkBudget::default(),
                    false,
                )
                .is_ok_and(|snapshot| !snapshot.elements.is_empty())
            {
                return Some(Self {
                    child,
                    driver,
                    _serialized: serialized,
                });
            }
            sleep(Duration::from_millis(250));
        }

        let _ = child.kill();
        let _ = child.wait();
        let _ = driver.store().clear();
        eprintln!(
            "skipping: {CALCULATOR_COMMAND} started but never reached the accessibility bus \
             within {LAUNCH_TIMEOUT:?}"
        );
        None
    }

    fn press(&self, label: &str) -> bool {
        if self
            .driver
            .snapshot(
                &Target::App(CALCULATOR_APP.to_owned()),
                WalkBudget::default(),
                false,
            )
            .is_err()
        {
            return false;
        }
        let selector = Selector::by_role(Role::Button).with_name(label);
        let Ok(element) = self.driver.find(&selector) else {
            return false;
        };
        self.driver
            .click_element(element.id, ActivationMode::Auto, MouseButton::Left, 1)
            .is_ok()
    }

    /// Whether any visible text in the window contains `needle`.
    fn displays(&self, needle: &str) -> bool {
        let Ok(snapshot) = self.driver.snapshot(
            &Target::App(CALCULATOR_APP.to_owned()),
            WalkBudget::default(),
            false,
        ) else {
            return false;
        };
        snapshot.elements.iter().any(|element| {
            element
                .name
                .as_deref()
                .is_some_and(|name| name.contains(needle))
                || element
                    .value
                    .as_deref()
                    .is_some_and(|value| value.contains(needle))
        })
    }
}

impl Drop for Calculator {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.driver.store().clear();
    }
}

#[cfg(target_os = "linux")]
fn platform_ports() -> Option<desktop_core::ports::Ports> {
    desktop_linux::build_ports().ok()
}

#[cfg(target_os = "macos")]
fn platform_ports() -> Option<desktop_core::ports::Ports> {
    desktop_macos::build_ports().ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_ports() -> Option<desktop_core::ports::Ports> {
    None
}

#[test]
#[ignore = "launches a real application"]
/// Presses the keys and reads the answer back out of the tree, because an
/// agent that cannot verify what it did is not much use.
fn the_calculator_computes_seven_plus_three_through_accessibility_actions() {
    let Some(calculator) = Calculator::launch() else {
        return;
    };

    assert!(calculator.press("7"), "could not press 7");
    assert!(calculator.press("+"), "could not press +");
    assert!(calculator.press("3"), "could not press 3");
    assert!(calculator.press("="), "could not press =");

    sleep(Duration::from_millis(300));
    assert!(
        calculator.displays("10"),
        "the calculator does not show the result"
    );
}

#[test]
#[ignore = "launches a real application"]
/// The whole point of pruning: fewer elements than nodes walked.
fn a_snapshot_of_a_real_application_is_smaller_than_its_raw_tree() {
    let Some(calculator) = Calculator::launch() else {
        return;
    };

    let snapshot = calculator
        .driver
        .snapshot(
            &Target::App(CALCULATOR_APP.to_owned()),
            WalkBudget::default(),
            false,
        )
        .expect("snapshots");

    assert!(!snapshot.elements.is_empty(), "snapshot is empty");
    assert!(
        snapshot.elements.len() < snapshot.visited_nodes,
        "pruning kept {} of {} nodes, which is not a saving",
        snapshot.elements.len(),
        snapshot.visited_nodes
    );
}

#[test]
#[ignore = "reads the live environment"]
/// The claim and the behaviour must not disagree in either direction.
fn capabilities_agree_with_what_the_backends_actually_do() {
    let Some(ports) = platform_ports() else {
        eprintln!("skipping: no platform backend on this target");
        return;
    };
    let store = SessionStore::new(
        std::env::temp_dir()
            .join(format!("desktop-driver-live-caps-{}", std::process::id()))
            .join("snapshot.json"),
    );
    let driver = Driver::new(ports, Policy::permissive(), store);

    let capabilities = driver.capabilities();
    let accessibility_claimed =
        capabilities.is_available(desktop_core::models::capability::Capability::Accessibility);

    let apps = driver.list_apps();
    assert_eq!(
        accessibility_claimed,
        apps.is_ok(),
        "capabilities claim accessibility={accessibility_claimed} but list_apps gave {apps:?}"
    );

    let _ = driver.store().clear();
}
