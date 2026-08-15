//! Test doubles for the four ports.
//!
//! Public rather than `#[cfg(test)]` because the CLI crate needs them too, and
//! because an adapter author should be able to check their wiring against the
//! same fakes the core is tested with.
//!
//! [`RecordedInput`] is the reason clicking and typing can be tested without
//! moving the real mouse.

use std::sync::{Arc, Mutex};

use crate::{
    agent::{AgentSession, SessionHost, SessionProcess, StartOptions},
    errors::{PermissionState, Result},
    models::{
        app::{AppKey, Application, Window},
        backend::{Backend, BackendInfo, DesktopEnvironment, DisplayServer, Platform},
        capability::{Capability, CapabilitySet, CapabilityState},
        chord::Chord,
        element::{ElementAction, RawNode, States},
        geometry::ScaleFactor,
        geometry::{Bounds, CoordinateSpace, Point, ScrollDelta},
        ids::{ProcessId, WindowId},
        image::Image,
        path::{self, ElementPath},
        role::Role,
        selector::Target,
        snapshot::WalkBudget,
    },
    ports::{
        AccessibilityPort, CapturePort, CaptureTarget, InputPort, MouseButton, PlatformProbe,
        Ports, ResolvedTree,
    },
};

/// Every input call the driver made, in order.
#[derive(Clone, Default)]
pub struct RecordedInput(Arc<Mutex<Recorded>>);

#[derive(Default)]
struct Recorded {
    moves: Vec<Point>,
    /// `(element name, text)` pairs written through the accessibility API.
    set_text: Vec<(String, String)>,
    clicks: Vec<(Point, MouseButton, u8)>,
    typed: Vec<String>,
    keys: Vec<Chord>,
    scrolls: Vec<ScrollDelta>,
}

impl RecordedInput {
    fn lock(&self) -> std::sync::MutexGuard<'_, Recorded> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[must_use]
    pub fn moves(&self) -> Vec<Point> {
        self.lock().moves.clone()
    }

    #[must_use]
    pub fn clicks(&self) -> Vec<(Point, MouseButton, u8)> {
        self.lock().clicks.clone()
    }

    #[must_use]
    pub fn typed(&self) -> Vec<String> {
        self.lock().typed.clone()
    }

    #[must_use]
    pub fn keys(&self) -> Vec<Chord> {
        self.lock().keys.clone()
    }

    #[must_use]
    pub fn scrolls(&self) -> Vec<ScrollDelta> {
        self.lock().scrolls.clone()
    }

    /// Text written straight into an element, bypassing the keyboard entirely.
    #[must_use]
    pub fn set_text(&self) -> Vec<(String, String)> {
        self.lock().set_text.clone()
    }

    /// True when the driver never reached the input backend at all — the
    /// assertion that proves a gate fired before dispatch.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let recorded = self.lock();
        recorded.moves.is_empty()
            && recorded.clicks.is_empty()
            && recorded.typed.is_empty()
            && recorded.keys.is_empty()
            && recorded.scrolls.is_empty()
            && recorded.set_text.is_empty()
    }
}

/// Builder for a fully-faked platform.
pub struct FakePorts {
    apps: Vec<String>,
    root: RawNode,
    capabilities: CapabilitySet,
    info: BackendInfo,
    recorded: RecordedInput,
}

impl FakePorts {
    #[must_use]
    pub fn new() -> Self {
        Self {
            apps: vec!["Fixture".to_owned()],
            root: RawNode::new(Role::Window)
                .with_name("Main")
                .with_bounds(Bounds::new(0, 0, 800, 600)),
            capabilities: all_supported(),
            info: BackendInfo {
                platform: Platform::Linux,
                display_server: DisplayServer::X11,
                desktop_environment: DesktopEnvironment::Gnome,
                accessibility: Backend::AtSpi,
                windows: Backend::Ewmh,
                screenshot: Backend::X11,
                input: Backend::X11,
            },
            recorded: RecordedInput::default(),
        }
    }

    #[must_use]
    pub fn recorded(&self) -> RecordedInput {
        self.recorded.clone()
    }

    #[must_use]
    pub fn with_apps(mut self, names: &[&str]) -> Self {
        self.apps = names.iter().map(|name| (*name).to_owned()).collect();
        self
    }

    #[must_use]
    pub fn with_root(mut self, root: RawNode) -> Self {
        self.root = root;
        self
    }

    #[must_use]
    pub fn with_button(self, name: &str) -> Self {
        self.with_buttons(&[name])
    }

    #[must_use]
    pub fn with_buttons(mut self, names: &[&str]) -> Self {
        let children = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                RawNode::new(Role::Button)
                    .with_name(name)
                    .with_bounds(Bounds::new(
                        10,
                        10 + i32::try_from(index).unwrap_or(0) * 40,
                        80,
                        32,
                    ))
                    .with_actions(&[ElementAction::Press])
            })
            .collect();
        self.root = self.root.with_children(children);
        self
    }

    /// A text field that supports focus-free text setting.
    #[must_use]
    pub fn with_text_box(mut self, name: &str) -> Self {
        self.root = self.root.with_children(vec![
            RawNode::new(Role::TextBox)
                .with_name(name)
                .with_bounds(Bounds::new(10, 10, 200, 24))
                .with_actions(&[ElementAction::Focus]),
        ]);
        self
    }

    #[must_use]
    pub fn with_password_field(mut self, name: &str) -> Self {
        let mut states = States::usable();
        states.protected = true;
        self.root = self.root.with_children(vec![
            RawNode::new(Role::PasswordField)
                .with_name(name)
                .with_value("hunter2")
                .with_states(states)
                .with_bounds(Bounds::new(10, 10, 200, 24))
                .with_actions(&[ElementAction::Focus]),
        ]);
        self
    }

    #[must_use]
    pub fn without_capability(mut self, capability: Capability) -> Self {
        self.capabilities
            .set(capability, CapabilityState::not_implemented());
        self
    }

    /// A GNOME Wayland session with no input backend — the honest shape when
    /// the RemoteDesktop portal is unavailable.
    #[must_use]
    pub fn as_wayland_without_input(mut self) -> Self {
        self.info.display_server = DisplayServer::Wayland;
        self.info.input = Backend::None;
        self.info.windows = Backend::AtSpi;
        self.capabilities
            .set(Capability::Mouse, CapabilityState::not_implemented());
        self.capabilities
            .set(Capability::Keyboard, CapabilityState::not_implemented());
        self
    }

    #[must_use]
    pub fn into_ports(self) -> Ports {
        let shared = Arc::new(FakeState {
            apps: self.apps,
            root: self.root,
            capabilities: self.capabilities,
            info: self.info,
        });
        Ports {
            accessibility: Box::new(FakeAccessibility {
                state: Arc::clone(&shared),
                recorded: self.recorded.clone(),
            }),
            capture: Box::new(FakeCapture {
                state: Arc::clone(&shared),
            }),
            input: Box::new(FakeInput {
                recorded: self.recorded,
            }),
            probe: Box::new(FakeProbe { state: shared }),
        }
    }
}

impl Default for FakePorts {
    fn default() -> Self {
        Self::new()
    }
}

#[must_use]
pub fn all_supported() -> CapabilitySet {
    let mut set = CapabilitySet::new();
    for capability in Capability::ALL {
        set.set(capability, CapabilityState::Supported);
    }
    set
}

struct FakeState {
    apps: Vec<String>,
    root: RawNode,
    capabilities: CapabilitySet,
    info: BackendInfo,
}

impl FakeState {
    fn primary_app(&self) -> AppKey {
        AppKey::new(
            ProcessId::new(1),
            self.apps.first().map_or("Fixture", String::as_str),
        )
    }

    fn window(&self) -> Window {
        Window {
            id: WindowId::new(1),
            title: self.root.name.clone(),
            app: self.primary_app(),
            bounds: self.root.bounds,
            focused: true,
            minimized: false,
            accessible: true,
            index: 0,
        }
    }
}

struct FakeAccessibility {
    state: Arc<FakeState>,
    recorded: RecordedInput,
}

impl AccessibilityPort for FakeAccessibility {
    fn list_apps(&self) -> Result<Vec<Application>> {
        Ok(self
            .state
            .apps
            .iter()
            .enumerate()
            .map(|(index, name)| Application {
                pid: ProcessId::new(i32::try_from(index).unwrap_or(0) + 1),
                name: name.clone(),
                identifier: None,
                active: index == 0,
                window_count: 1,
            })
            .collect())
    }

    fn list_windows(&self, _app: Option<&AppKey>) -> Result<Vec<Window>> {
        Ok(vec![self.state.window()])
    }

    fn tree(&self, _target: &Target, _budget: WalkBudget) -> Result<ResolvedTree> {
        Ok(ResolvedTree {
            app: self.state.primary_app(),
            window: self.state.window(),
            root: self.state.root.clone(),
            space: CoordinateSpace::primary_screen(),
        })
    }

    fn resolve(&self, path: &ElementPath) -> Result<RawNode> {
        path::resolve(&self.state.root, &path.steps)
            .cloned()
            .map_err(|reason| crate::errors::DesktopError::ElementStale {
                element: crate::models::ids::ElementId::new(0),
                reason,
            })
    }

    fn perform(&self, _path: &ElementPath, _action: ElementAction) -> Result<()> {
        Ok(())
    }

    fn set_text(&self, path: &ElementPath, text: &str) -> Result<()> {
        let node = path::resolve(&self.state.root, &path.steps).map_err(|reason| {
            crate::errors::DesktopError::ElementStale {
                element: crate::models::ids::ElementId::new(0),
                reason,
            }
        })?;
        self.recorded
            .lock()
            .set_text
            .push((node.name.clone().unwrap_or_default(), text.to_owned()));
        Ok(())
    }

    fn focus(&self, _target: &Target) -> Result<()> {
        Ok(())
    }
}

struct FakeCapture {
    state: Arc<FakeState>,
}

impl CapturePort for FakeCapture {
    fn capture(&self, target: &CaptureTarget) -> Result<Image> {
        let space = match target {
            CaptureTarget::Screen => CoordinateSpace::primary_screen(),
            CaptureTarget::Window(id) => CoordinateSpace::Window(*id),
        };
        let _ = &self.state;
        Image::new(2, 2, ScaleFactor::ONE, space, vec![0; 16])
            .map_err(|error| crate::errors::DesktopError::backend(error.to_string()))
    }
}

struct FakeInput {
    recorded: RecordedInput,
}

impl InputPort for FakeInput {
    fn move_mouse(&self, point: Point, _space: &CoordinateSpace) -> Result<()> {
        self.recorded.lock().moves.push(point);
        Ok(())
    }

    fn click(
        &self,
        point: Point,
        _space: &CoordinateSpace,
        button: MouseButton,
        count: u8,
    ) -> Result<()> {
        self.recorded.lock().clicks.push((point, button, count));
        Ok(())
    }

    fn type_text(&self, text: &str) -> Result<()> {
        self.recorded.lock().typed.push(text.to_owned());
        Ok(())
    }

    fn key(&self, chord: &Chord) -> Result<()> {
        self.recorded.lock().keys.push(*chord);
        Ok(())
    }

    fn scroll(&self, delta: ScrollDelta, _space: &CoordinateSpace) -> Result<()> {
        self.recorded.lock().scrolls.push(delta);
        Ok(())
    }
}

struct FakeProbe {
    state: Arc<FakeState>,
}

impl PlatformProbe for FakeProbe {
    fn info(&self) -> BackendInfo {
        self.state.info.clone()
    }

    fn capabilities(&self) -> CapabilitySet {
        self.state.capabilities.clone()
    }

    fn permissions(&self) -> Vec<PermissionState> {
        Vec::new()
    }
}

/// A session host that records instead of starting an X server.
#[derive(Default)]
pub struct FakeSessions {
    running: Mutex<Option<AgentSession>>,
    launched: Mutex<Vec<(String, Vec<String>)>>,
}

impl FakeSessions {
    #[must_use]
    pub fn idle() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn running() -> Self {
        let host = Self::idle();
        *host.running.lock().expect("not poisoned") = Some(Self::example());
        host
    }

    #[must_use]
    pub fn example() -> AgentSession {
        AgentSession {
            display: ":97".to_owned(),
            width: 1920,
            height: 1080,
            dbus_address: "unix:path=/tmp/dbus-fake".to_owned(),
            a11y_address: "unix:path=/run/user/1000/at-spi/bus_97".to_owned(),
            xauthority: std::path::PathBuf::from("/run/user/1000/desktop-driver/Xauthority"),
            cookie: "00112233445566778899aabbccddeeff".to_owned(),
            visible: true,
            home: Some(std::path::PathBuf::from(
                "/home/agent/.local/share/desktop-driver/home",
            )),
            processes: vec![SessionProcess::new("Xvfb", 4242)],
        }
    }

    #[must_use]
    pub fn launched(&self) -> Vec<(String, Vec<String>)> {
        self.launched.lock().expect("not poisoned").clone()
    }
}

impl SessionHost for FakeSessions {
    fn start(&self, options: StartOptions) -> Result<AgentSession> {
        let session = AgentSession {
            width: options.width,
            height: options.height,
            display: options
                .display
                .map_or_else(|| ":97".to_owned(), |number| format!(":{number}")),
            home: if options.share_home {
                None
            } else {
                Self::example().home
            },
            ..Self::example()
        };
        *self.running.lock().expect("not poisoned") = Some(session.clone());
        Ok(session)
    }

    fn status(&self) -> Option<AgentSession> {
        self.running.lock().expect("not poisoned").clone()
    }

    fn stop(&self) -> Result<Option<AgentSession>> {
        Ok(self.running.lock().expect("not poisoned").take())
    }

    fn launch(&self, program: &str, args: &[String]) -> Result<u32> {
        if self.status().is_none() {
            return Err(crate::errors::DesktopError::invalid_argument(
                "no agent display is running",
            ));
        }
        self.launched
            .lock()
            .expect("not poisoned")
            .push((program.to_owned(), args.to_vec()));
        Ok(9_999)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_input_starts_empty_so_gate_assertions_are_meaningful() {
        let recorded = RecordedInput::default();
        assert!(recorded.is_empty());
    }

    #[test]
    fn recorded_input_captures_calls_without_touching_the_real_desktop() {
        let ports = FakePorts::new();
        let recorded = ports.recorded();
        let ports = ports.into_ports();

        ports
            .input
            .click(
                Point::new(800, 400),
                &CoordinateSpace::primary_screen(),
                MouseButton::Left,
                2,
            )
            .expect("clicks");
        ports.input.type_text("hi").expect("types");

        assert!(!recorded.is_empty());
        assert_eq!(
            recorded.clicks(),
            vec![(Point::new(800, 400), MouseButton::Left, 2)]
        );
        assert_eq!(recorded.typed(), vec!["hi".to_owned()]);
    }

    #[test]
    fn the_default_fake_supports_every_capability() {
        let set = all_supported();
        for capability in Capability::ALL {
            assert!(set.is_available(capability), "{capability:?}");
        }
    }

    #[test]
    fn removing_a_capability_leaves_the_others_intact() {
        let ports = FakePorts::new().without_capability(Capability::Mouse);
        let probe = ports.into_ports().probe;
        let caps = probe.capabilities();
        assert!(!caps.is_available(Capability::Mouse));
        assert!(caps.is_available(Capability::Keyboard));
    }
}
