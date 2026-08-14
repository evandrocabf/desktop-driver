//! The composed driver the CLI talks to.
//!
//! Every operation passes through the same two gates before reaching a
//! backend: does this environment support it, and does policy allow it. Both
//! live here rather than in the adapters, so a backend that forgets to handle
//! an unsupported case cannot produce a confidently wrong result — the worst
//! failure mode this tool has.

use crate::{
    errors::{DesktopError, PermissionState, Result},
    models::{
        app::{AppKey, Application, Window},
        backend::{Backend, BackendInfo, DisplayServer},
        capability::{Capability, CapabilitySet},
        chord::Chord,
        element::{Element, ElementAction, RawNode},
        geometry::{CoordinateSpace, Point, ScrollDelta},
        ids::ElementId,
        image::Image,
        path::ElementPath,
        role::Role,
        selector::{ActivationMode, Selector, Target},
        snapshot::{Snapshot, WalkBudget},
    },
    normalize::{self, SnapshotContext},
    policy::{Action, Policy},
    ports::{CaptureTarget, Diagnostic, MouseButton, Ports},
    session::SessionStore,
};

pub struct Driver {
    ports: Ports,
    policy: Policy,
    store: SessionStore,
}

impl Driver {
    #[must_use]
    pub fn new(ports: Ports, policy: Policy, store: SessionStore) -> Self {
        Self {
            ports,
            policy,
            store,
        }
    }

    #[must_use]
    pub fn info(&self) -> BackendInfo {
        self.ports.probe.info()
    }

    #[must_use]
    pub fn capabilities(&self) -> CapabilitySet {
        self.ports.probe.capabilities()
    }

    #[must_use]
    pub fn permissions(&self) -> Vec<PermissionState> {
        self.ports.probe.permissions()
    }

    #[must_use]
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.ports.probe.diagnostics()
    }

    #[must_use]
    pub fn dependencies(&self) -> Vec<crate::models::dependency::SystemDependency> {
        self.ports.probe.dependencies()
    }

    #[must_use]
    pub fn install_command(&self) -> Option<String> {
        self.ports.probe.install_command()
    }

    /// Asks the platform to show any outstanding permission prompt.
    #[must_use]
    pub fn request_permissions(&self) -> Vec<PermissionState> {
        self.ports.probe.request_permissions()
    }

    #[must_use]
    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    /// The gate. Refuses with a fully-populated structured error naming the
    /// environment, so an agent can tell "not here" from "not ever".
    fn require(&self, capability: Capability, backend: Backend) -> Result<()> {
        if self.capabilities().is_available(capability) {
            return Ok(());
        }
        Err(DesktopError::unsupported(capability, backend, &self.info()))
    }

    pub fn list_apps(&self) -> Result<Vec<Application>> {
        self.policy.check(Action::ListApps, None)?;
        self.require(Capability::Accessibility, self.info().accessibility)?;
        let apps = self.ports.accessibility.list_apps()?;
        Ok(apps
            .into_iter()
            .filter(|app| {
                self.policy
                    .check(Action::ListApps, Some(&app.key()))
                    .is_ok()
            })
            .collect())
    }

    pub fn list_windows(&self, app: Option<&AppKey>) -> Result<Vec<Window>> {
        self.policy.check(Action::ListWindows, app)?;
        self.require(Capability::Windows, self.info().windows)?;
        let windows = self.ports.accessibility.list_windows(app)?;
        Ok(windows
            .into_iter()
            .filter(|w| self.policy.check(Action::ListWindows, Some(&w.app)).is_ok())
            .collect())
    }

    /// The raw tree, for `desktop inspect`.
    pub fn inspect(&self, target: &Target, budget: WalkBudget) -> Result<(AppKey, RawNode)> {
        self.policy.check(Action::Inspect, None)?;
        self.require(Capability::Accessibility, self.info().accessibility)?;
        let resolved = self.ports.accessibility.tree(target, budget)?;
        self.policy.check(Action::Inspect, Some(&resolved.app))?;
        Ok((resolved.app, resolved.root))
    }

    /// Takes a snapshot and persists it so a later process can resolve
    /// `--element N`.
    pub fn snapshot(
        &self,
        target: &Target,
        budget: WalkBudget,
        include_offscreen: bool,
    ) -> Result<Snapshot> {
        self.policy.check(Action::Snapshot, None)?;
        self.require(Capability::Accessibility, self.info().accessibility)?;

        let resolved = self.ports.accessibility.tree(target, budget)?;
        self.policy.check(Action::Snapshot, Some(&resolved.app))?;

        let context =
            SnapshotContext::new(resolved.app.clone(), resolved.window.key(), resolved.space)
                .with_title(resolved.window.title.as_deref())
                .with_budget(budget)
                .including_offscreen(include_offscreen);

        let snapshot = normalize::snapshot(&resolved.root, &context);
        self.store.save(&snapshot)?;
        Ok(snapshot)
    }

    pub fn screenshot(&self, target: &CaptureTarget) -> Result<Image> {
        self.policy.check(Action::Screenshot, None)?;
        let capability = match target {
            CaptureTarget::Screen => Capability::Screenshots,
            CaptureTarget::Window(_) => Capability::WindowScreenshots,
        };
        self.require(capability, self.info().screenshot)?;
        self.ports.capture.capture(target)
    }

    pub fn focus(&self, target: &Target) -> Result<()> {
        self.policy.check(Action::Focus, None)?;
        self.policy.check_exclusive_input(Action::Focus)?;
        self.require(Capability::Focus, self.info().windows)?;
        self.ports.accessibility.focus(target)
    }

    pub fn move_mouse(&self, point: Point, space: &CoordinateSpace) -> Result<()> {
        self.policy.check(Action::MoveMouse, None)?;
        self.policy.check_exclusive_input(Action::MoveMouse)?;
        self.require(Capability::Mouse, self.info().input)?;
        self.ports.input.move_mouse(point, space)
    }

    /// Clicks a coordinate, which is pointer synthesis by definition and so
    /// always subject to the exclusive-input gate.
    pub fn click_point(
        &self,
        point: Point,
        space: &CoordinateSpace,
        button: MouseButton,
        count: u8,
    ) -> Result<()> {
        self.policy.check(Action::Click, None)?;
        self.policy.check_pointer_fallback()?;
        self.require(Capability::Mouse, self.info().input)?;
        self.ports.input.click(point, space, button, count)
    }

    /// Clicks a previously-snapshotted element.
    ///
    /// Resolution happens against the *live* tree, so an element that has moved
    /// is still found and one that has been replaced is reported stale rather
    /// than clicked at its former position.
    pub fn click_element(
        &self,
        id: ElementId,
        mode: ActivationMode,
        button: MouseButton,
        count: u8,
    ) -> Result<Activation> {
        self.policy.check(Action::Click, None)?;
        let path = self.store.lookup(id)?;
        self.policy.check(Action::Click, Some(&path.app))?;

        let node = self.ports.accessibility.resolve(&path)?;
        self.policy.check_role(Action::Click, &node.role)?;

        self.activate(&path, &node, mode, button, count)
    }

    /// Finds a single element matching `selector` in the current snapshot.
    pub fn find(&self, selector: &Selector) -> Result<Element> {
        if selector.is_empty() {
            return Err(DesktopError::invalid_argument(
                "a selector needs at least one of --role, --name or --text",
            ));
        }
        let snapshot = self.store.load()?;
        let matches: Vec<&Element> = snapshot
            .elements
            .iter()
            .filter(|element| selector.matches(element))
            .collect();

        match matches.as_slice() {
            [only] => Ok((*only).clone()),
            [] => Err(DesktopError::ElementNotFound {
                selector: selector.describe(),
            }),
            many => Err(DesktopError::AmbiguousSelector {
                selector: selector.describe(),
                matches: many.len(),
                candidates: many.iter().map(|element| element.id).collect(),
            }),
        }
    }

    pub fn find_all(&self, selector: &Selector) -> Result<Vec<Element>> {
        if selector.is_empty() {
            return Err(DesktopError::invalid_argument(
                "a selector needs at least one of --role, --name or --text",
            ));
        }
        let snapshot = self.store.load()?;
        Ok(snapshot
            .elements
            .into_iter()
            .filter(|element| selector.matches(element))
            .collect())
    }

    /// Activates an element, through its accessibility action or the pointer.
    ///
    /// The action is preferred because it cannot miss: no cursor movement, no
    /// portal session, and no dependence on the window still being where the
    /// snapshot said it was. The pointer is reached only when no action was
    /// available or the caller asked for it explicitly, and it needs bounds —
    /// under Wayland an element without them cannot be pointed at, and guessing
    /// a position would click something arbitrary.
    fn activate(
        &self,
        path: &ElementPath,
        node: &RawNode,
        mode: ActivationMode,
        button: MouseButton,
        count: u8,
    ) -> Result<Activation> {
        let preferred = preferred_action(&node.role, &node.actions);

        let use_action = match mode {
            ActivationMode::Action => true,
            ActivationMode::Pointer => false,
            ActivationMode::Auto => preferred.is_some(),
        };

        if use_action {
            let action = preferred.ok_or_else(|| {
                DesktopError::invalid_argument(format!(
                    "element {} advertises no activatable action; retry with --via pointer",
                    node.name.as_deref().unwrap_or("<unnamed>")
                ))
            })?;
            self.require(Capability::ElementActions, self.info().accessibility)?;
            self.ports.accessibility.perform(path, action)?;
            return Ok(Activation::Action(action));
        }

        self.policy.check_pointer_fallback()?;
        self.require(Capability::Mouse, self.info().input)?;
        let bounds = node.bounds.ok_or_else(|| {
            DesktopError::invalid_argument(
                "element has no reported bounds, so it cannot be clicked by pointer",
            )
        })?;
        if bounds.is_empty() {
            return Err(DesktopError::invalid_argument(
                "element has zero area, so it cannot be clicked by pointer",
            ));
        }

        let resolved = self.ports.accessibility.tree(
            &Target::Focused,
            WalkBudget {
                max_nodes: 1,
                max_depth: 0,
            },
        );
        let space = resolved.map_or_else(|_| CoordinateSpace::primary_screen(), |tree| tree.space);

        let point = bounds.center();
        self.ports.input.click(point, &space, button, count)?;
        Ok(Activation::Pointer(point))
    }

    /// Types into whatever currently has focus.
    ///
    /// Racy on a shared desktop by nature — focus can change between the
    /// snapshot and the keystroke. Prefer [`Driver::type_into_element`].
    pub fn type_text(&self, text: &str) -> Result<()> {
        self.policy.check(Action::Type, None)?;
        self.policy.check_exclusive_input(Action::Type)?;
        self.require(Capability::Keyboard, self.info().input)?;
        self.ports.input.type_text(text)
    }

    /// Writes text straight into an element through the accessibility API.
    ///
    /// No keystrokes, no focus change, no pointer movement: the text goes to
    /// the element the agent actually looked at, and the person using the
    /// machine keeps their cursor and their keyboard.
    pub fn type_into_element(&self, id: ElementId, text: &str) -> Result<()> {
        self.policy.check(Action::TypeIntoElement, None)?;
        let path = self.store.lookup(id)?;
        self.policy
            .check(Action::TypeIntoElement, Some(&path.app))?;

        let node = self.ports.accessibility.resolve(&path)?;
        self.policy
            .check_role(Action::TypeIntoElement, &node.role)?;
        self.require(Capability::ElementText, self.info().accessibility)?;

        self.ports.accessibility.set_text(&path, text)
    }

    pub fn key(&self, chord: &Chord) -> Result<()> {
        self.policy.check(Action::Key, None)?;
        self.policy.check_exclusive_input(Action::Key)?;
        self.require(Capability::Keyboard, self.info().input)?;
        self.ports.input.key(chord)
    }

    pub fn scroll(&self, delta: ScrollDelta, space: &CoordinateSpace) -> Result<()> {
        self.policy.check(Action::Scroll, None)?;
        self.policy.check_exclusive_input(Action::Scroll)?;
        self.require(Capability::Scroll, self.info().input)?;
        self.ports.input.scroll(delta, space)
    }

    /// Rejects raw coordinates in a session where they have no meaning.
    ///
    /// Under Wayland a client cannot learn any window's screen position, so a
    /// bare `--x/--y` can only be interpreted against a monitor. When the
    /// backend has no monitor-relative input path, saying so beats clicking
    /// somewhere arbitrary.
    pub fn coordinate_space_for_point(&self) -> Result<CoordinateSpace> {
        let info = self.info();
        if info.display_server == DisplayServer::Wayland && info.input == Backend::None {
            return Err(DesktopError::CoordinatesRequireWindow {
                display_server: info.display_server,
            });
        }
        Ok(CoordinateSpace::primary_screen())
    }
}

/// How a click was ultimately delivered. Reported so an agent can tell a
/// deterministic activation from a positional one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Activation {
    Action(ElementAction),
    Pointer(Point),
}

/// Picks the action that best corresponds to "click this".
///
/// Text fields are excluded: their only action is usually "focus", and
/// activating that instead of clicking would put the caret somewhere
/// unpredictable.
fn preferred_action(role: &Role, actions: &[ElementAction]) -> Option<ElementAction> {
    const PREFERENCE: [ElementAction; 4] = [
        ElementAction::Press,
        ElementAction::Toggle,
        ElementAction::Select,
        ElementAction::Expand,
    ];
    if matches!(
        role,
        Role::TextBox | Role::PasswordField | Role::SearchField
    ) {
        return None;
    }
    PREFERENCE
        .into_iter()
        .find(|candidate| actions.contains(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        models::{ids::ProcessId, role::Role},
        testing::{FakePorts, RecordedInput},
    };

    /// Each test gets its own store. Sharing one path lets tests running in
    /// parallel clear each other's snapshots, which fails intermittently and
    /// looks like a bug in the driver.
    fn driver_with(tag: &str, ports: FakePorts, policy: Policy) -> (Driver, RecordedInput) {
        let recorded = ports.recorded();
        let store = SessionStore::new(
            std::env::temp_dir()
                .join(format!("desktop-driver-test-{}-{tag}", std::process::id()))
                .join("snapshot.json"),
        );
        let _ = store.clear();
        (Driver::new(ports.into_ports(), policy, store), recorded)
    }

    #[test]
    fn an_unsupported_capability_is_refused_before_the_backend_is_ever_called() {
        // The backend here would happily "succeed"; the gate must stop it.
        let ports = FakePorts::new().without_capability(Capability::Mouse);
        let (driver, recorded) = driver_with("gate-mouse", ports, Policy::permissive());

        let error = driver
            .move_mouse(Point::new(10, 10), &CoordinateSpace::primary_screen())
            .expect_err("must refuse");
        assert!(matches!(error, DesktopError::UnsupportedCapability { .. }));
        assert!(recorded.is_empty(), "backend was called despite the gate");
    }

    #[test]
    fn read_only_mode_blocks_input_before_the_backend_is_called() {
        let (driver, recorded) =
            driver_with("read-only-input", FakePorts::new(), Policy::read_only());
        assert!(driver.type_text("hello").is_err());
        assert!(driver.key(&Chord::parse("cmd+s").expect("parses")).is_err());
        assert!(recorded.is_empty(), "backend was called in read-only mode");
    }

    #[test]
    fn read_only_mode_still_permits_observation() {
        let (driver, _) = driver_with("read-only-observe", FakePorts::new(), Policy::read_only());
        assert!(driver.list_apps().is_ok());
    }

    #[test]
    fn a_denied_app_is_filtered_out_of_the_application_list() {
        let policy = Policy {
            deny_apps: vec!["1Password".to_owned()],
            ..Policy::default()
        };
        let ports = FakePorts::new().with_apps(&["Firefox", "1Password", "Code"]);
        let (driver, _) = driver_with("deny-app-list", ports, policy);
        let names: Vec<String> = driver
            .list_apps()
            .expect("lists")
            .into_iter()
            .map(|app| app.name)
            .collect();
        assert_eq!(names, vec!["Firefox".to_owned(), "Code".to_owned()]);
    }

    #[test]
    fn typing_reaches_the_backend_when_policy_and_capabilities_allow_it() {
        let (driver, recorded) = driver_with("typing", FakePorts::new(), Policy::permissive());
        driver.type_text("Hello world").expect("types");
        assert_eq!(recorded.typed(), vec!["Hello world".to_owned()]);
    }

    #[test]
    fn clicking_a_button_prefers_the_accessibility_action_over_the_pointer() {
        // No cursor movement means nothing to miss, and no portal dialog.
        let ports = FakePorts::new().with_button("Save");
        let (driver, recorded) = driver_with("click-action", ports, Policy::permissive());
        driver
            .snapshot(&Target::Focused, WalkBudget::default(), false)
            .expect("snapshots");

        let activation = driver
            .click_element(
                ElementId::new(1),
                ActivationMode::Auto,
                MouseButton::Left,
                1,
            )
            .expect("clicks");
        assert_eq!(activation, Activation::Action(ElementAction::Press));
        assert!(
            recorded.clicks().is_empty(),
            "pointer was used unnecessarily"
        );
        let _ = driver.store().clear();
    }

    #[test]
    fn requesting_pointer_activation_explicitly_moves_the_pointer() {
        let ports = FakePorts::new().with_button("Save");
        let (driver, recorded) = driver_with("click-pointer", ports, Policy::permissive());
        driver
            .snapshot(&Target::Focused, WalkBudget::default(), false)
            .expect("snapshots");

        let activation = driver
            .click_element(
                ElementId::new(1),
                ActivationMode::Pointer,
                MouseButton::Left,
                1,
            )
            .expect("clicks");
        assert!(matches!(activation, Activation::Pointer(_)));
        assert_eq!(recorded.clicks().len(), 1);
        let _ = driver.store().clear();
    }

    #[test]
    fn a_text_box_is_clicked_by_pointer_because_its_only_action_is_focus() {
        assert_eq!(
            preferred_action(&Role::TextBox, &[ElementAction::Focus]),
            None
        );
        assert_eq!(
            preferred_action(&Role::Button, &[ElementAction::Focus, ElementAction::Press]),
            Some(ElementAction::Press)
        );
    }

    #[test]
    fn clicking_without_a_prior_snapshot_reports_no_snapshot() {
        let (driver, _) = driver_with("no-snapshot", FakePorts::new(), Policy::permissive());
        let _ = driver.store().clear();
        let error = driver
            .click_element(
                ElementId::new(1),
                ActivationMode::Auto,
                MouseButton::Left,
                1,
            )
            .expect_err("must fail");
        assert_eq!(error, DesktopError::NoSnapshot);
    }

    #[test]
    fn find_reports_ambiguity_rather_than_silently_choosing_the_first_match() {
        let ports = FakePorts::new().with_buttons(&["Save", "Save"]);
        let (driver, _) = driver_with("ambiguous", ports, Policy::permissive());
        driver
            .snapshot(&Target::Focused, WalkBudget::default(), false)
            .expect("snapshots");

        let error = driver
            .find(&Selector::by_name("Save"))
            .expect_err("must be ambiguous");
        match error {
            DesktopError::AmbiguousSelector { matches, .. } => assert_eq!(matches, 2),
            other => panic!("expected ambiguity, got {other:?}"),
        }
        let _ = driver.store().clear();
    }

    #[test]
    fn find_returns_the_single_match_when_there_is_exactly_one() {
        let ports = FakePorts::new().with_buttons(&["Save", "Run"]);
        let (driver, _) = driver_with("find-one", ports, Policy::permissive());
        driver
            .snapshot(&Target::Focused, WalkBudget::default(), false)
            .expect("snapshots");
        let found = driver.find(&Selector::by_name("Run")).expect("finds");
        assert_eq!(found.name.as_deref(), Some("Run"));
        let _ = driver.store().clear();
    }

    #[test]
    fn an_empty_selector_is_rejected_as_an_invalid_argument() {
        let (driver, _) = driver_with("empty-selector", FakePorts::new(), Policy::permissive());
        let error = driver.find(&Selector::default()).expect_err("must reject");
        assert!(matches!(error, DesktopError::InvalidArgument { .. }));
    }

    #[test]
    fn a_denied_role_cannot_be_clicked_even_when_it_is_present() {
        let policy = Policy {
            deny_roles: vec![Role::PasswordField],
            ..Policy::default()
        };
        let ports = FakePorts::new().with_password_field("Password");
        let (driver, recorded) = driver_with("deny-role", ports, policy);
        driver
            .snapshot(&Target::Focused, WalkBudget::default(), false)
            .expect("snapshots");

        let error = driver
            .click_element(
                ElementId::new(1),
                ActivationMode::Auto,
                MouseButton::Left,
                1,
            )
            .expect_err("must deny");
        assert!(matches!(error, DesktopError::PolicyDenied { .. }));
        assert!(recorded.is_empty());
        let _ = driver.store().clear();
    }

    #[test]
    fn snapshotting_persists_the_result_for_the_next_process() {
        let ports = FakePorts::new().with_button("Save");
        let (driver, _) = driver_with("persist", ports, Policy::permissive());
        let taken = driver
            .snapshot(&Target::Focused, WalkBudget::default(), false)
            .expect("snapshots");
        let reloaded = driver.store().load().expect("reloads");
        assert_eq!(reloaded, taken);
        let _ = driver.store().clear();
    }

    #[test]
    fn bare_coordinates_are_refused_when_wayland_has_no_input_backend() {
        let ports = FakePorts::new().as_wayland_without_input();
        let (driver, _) = driver_with("wayland-coords", ports, Policy::permissive());
        let error = driver
            .coordinate_space_for_point()
            .expect_err("must refuse");
        assert!(matches!(
            error,
            DesktopError::CoordinatesRequireWindow { .. }
        ));
    }

    #[test]
    fn a_denied_app_cannot_be_snapshotted() {
        let policy = Policy {
            deny_apps: vec!["Fixture".to_owned()],
            ..Policy::default()
        };
        let (driver, _) = driver_with("deny-snapshot", FakePorts::new(), policy);
        let error = driver
            .snapshot(&Target::Focused, WalkBudget::default(), false)
            .expect_err("must deny");
        assert!(matches!(error, DesktopError::PolicyDenied { .. }));
    }

    #[test]
    fn typing_into_an_element_never_touches_the_keyboard_or_the_pointer() {
        // This is the whole point: on a desktop shared with a person, the text
        // goes to the field the agent looked at, not to whatever has focus.
        let ports = FakePorts::new().with_text_box("Address");
        let (driver, recorded) = driver_with("addressed-type", ports, Policy::permissive());
        driver
            .snapshot(&Target::Focused, WalkBudget::default(), false)
            .expect("snapshots");

        driver
            .type_into_element(ElementId::new(1), "x.com")
            .expect("types");

        assert_eq!(
            recorded.set_text(),
            vec![("Address".to_owned(), "x.com".to_owned())]
        );
        assert!(recorded.typed().is_empty(), "no keystrokes should be sent");
        assert!(recorded.clicks().is_empty(), "no pointer should be moved");
        let _ = driver.store().clear();
    }

    #[test]
    fn no_steal_focus_permits_addressed_typing() {
        let ports = FakePorts::new().with_text_box("Address");
        let (driver, recorded) = driver_with("no-steal-addressed", ports, Policy::no_steal_focus());
        driver
            .snapshot(&Target::Focused, WalkBudget::default(), false)
            .expect("snapshots");
        driver
            .type_into_element(ElementId::new(1), "hello")
            .expect("types");
        assert_eq!(recorded.set_text().len(), 1);
        let _ = driver.store().clear();
    }

    #[test]
    fn no_steal_focus_refuses_the_operations_that_seize_shared_devices() {
        let (driver, recorded) = driver_with(
            "no-steal-refuse",
            FakePorts::new(),
            Policy::no_steal_focus(),
        );

        assert!(driver.type_text("hello").is_err());
        assert!(driver.key(&Chord::parse("cmd+s").expect("parses")).is_err());
        assert!(
            driver
                .move_mouse(Point::new(10, 10), &CoordinateSpace::primary_screen())
                .is_err()
        );
        assert!(
            driver
                .scroll(
                    ScrollDelta::new(0, -100),
                    &CoordinateSpace::primary_screen()
                )
                .is_err()
        );
        assert!(driver.focus(&Target::Focused).is_err());
        assert!(
            driver
                .click_point(
                    Point::new(1, 1),
                    &CoordinateSpace::primary_screen(),
                    MouseButton::Left,
                    1
                )
                .is_err()
        );
        assert!(
            recorded.is_empty(),
            "nothing should have reached the backend"
        );
    }

    #[test]
    fn no_steal_focus_still_allows_clicking_through_an_accessibility_action() {
        // A button with an action needs no pointer, so it stays allowed.
        let ports = FakePorts::new().with_button("Save");
        let (driver, recorded) = driver_with("no-steal-action", ports, Policy::no_steal_focus());
        driver
            .snapshot(&Target::Focused, WalkBudget::default(), false)
            .expect("snapshots");

        let activation = driver
            .click_element(
                ElementId::new(1),
                ActivationMode::Auto,
                MouseButton::Left,
                1,
            )
            .expect("clicks");
        assert_eq!(activation, Activation::Action(ElementAction::Press));
        assert!(recorded.clicks().is_empty());
        let _ = driver.store().clear();
    }

    #[test]
    fn no_steal_focus_refuses_a_click_that_would_fall_back_to_the_pointer() {
        // A text box has no press action, so Auto would reach for the pointer.
        let ports = FakePorts::new().with_text_box("Address");
        let (driver, recorded) = driver_with("no-steal-fallback", ports, Policy::no_steal_focus());
        driver
            .snapshot(&Target::Focused, WalkBudget::default(), false)
            .expect("snapshots");

        let error = driver
            .click_element(
                ElementId::new(1),
                ActivationMode::Auto,
                MouseButton::Left,
                1,
            )
            .expect_err("must refuse");
        assert!(matches!(error, DesktopError::PolicyDenied { .. }));
        assert!(recorded.is_empty());
        let _ = driver.store().clear();
    }

    #[test]
    fn addressed_typing_is_refused_for_a_denied_role() {
        let policy = Policy {
            deny_roles: vec![Role::PasswordField],
            ..Policy::default()
        };
        let ports = FakePorts::new().with_password_field("Password");
        let (driver, recorded) = driver_with("addressed-denied-role", ports, policy);
        driver
            .snapshot(&Target::Focused, WalkBudget::default(), false)
            .expect("snapshots");

        let error = driver
            .type_into_element(ElementId::new(1), "hunter2")
            .expect_err("must deny");
        assert!(matches!(error, DesktopError::PolicyDenied { .. }));
        assert!(recorded.set_text().is_empty());
        let _ = driver.store().clear();
    }

    #[test]
    fn app_key_is_carried_through_so_policy_can_scope_by_application() {
        let key = AppKey::new(ProcessId::new(1), "Fixture");
        assert!(key.matches("Fixture"));
    }
}
