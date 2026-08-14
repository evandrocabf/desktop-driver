//! AT-SPI accessibility adapter.
//!
//! This is the one Linux backend that is *display-server independent*: it works
//! identically under X11 and Wayland, because it is D-Bus all the way down and
//! never talks to the compositor. That is why it carries window enumeration on
//! Wayland, where no window-listing protocol is available to an ordinary
//! client.
//!
//! The coordinate caveat is load-bearing and is handled in [`extents`]: under
//! Wayland a toolkit cannot know its own screen position, so
//! `GetExtents(Screen)` returns surface-relative numbers. Rather than pass
//! those off as screen coordinates, the adapter reports the space it is
//! actually working in and lets the core carry that through to the snapshot.

use std::collections::HashSet;

use atspi::{
    CoordType, Interface, ObjectRefOwned, State, StateSet,
    connection::AccessibilityConnection,
    proxy::{
        accessible::{AccessibleProxy, ObjectRefExt as _},
        action::ActionProxy,
        component::ComponentProxy,
        editable_text::EditableTextProxy,
        text::TextProxy,
        value::ValueProxy,
    },
    zbus,
};
use desktop_core::{
    errors::{DesktopError, Result},
    models::{
        app::{AppKey, Application, Window},
        element::{ElementAction, RawNode, States},
        geometry::{Bounds, CoordinateSpace},
        ids::{ProcessId, WindowId},
        path::{self, ElementPath},
        role,
        selector::Target,
        snapshot::WalkBudget,
    },
    ports::{AccessibilityPort, ResolvedTree},
};

use crate::runtime;

/// AT-SPI roles that denote a top-level window.
const WINDOW_ROLES: [&str; 4] = ["frame", "window", "dialog", "alert"];

/// How long an application gets to notice it has been focused.
const FOCUS_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);

pub struct AtSpi {
    connection: AccessibilityConnection,
    /// Which space [`Bounds`] returned by this adapter are measured in.
    space: CoordinateSpace,
    window_relative: bool,
    /// Carried so a refusal can name the environment that refused.
    info: desktop_core::models::backend::BackendInfo,
    /// How to raise a window, where the display server permits it at all.
    activator: Option<Box<dyn crate::x11::WindowActivator>>,
}

impl AtSpi {
    /// Connects to an accessibility bus: the session's own when `address` is
    /// `None`, a named one otherwise.
    ///
    /// `window_relative` reflects the display server: under Wayland the
    /// toolkit's idea of "screen coordinates" is really surface-relative, so
    /// the adapter asks for window coordinates explicitly and labels them
    /// honestly instead of pretending they are absolute.
    ///
    /// An agent session runs its own `at-spi-bus-launcher`, whose registry the
    /// user's applications are not on. Addressing that bus explicitly is what
    /// makes `desktop apps` inside a session list only what the agent started —
    /// and it cannot be done by exporting `AT_SPI_BUS_ADDRESS`, because
    /// mutating this process's environment is unsound once threads are running.
    pub fn connect_to(
        address: Option<&str>,
        window_relative: bool,
        info: desktop_core::models::backend::BackendInfo,
    ) -> Result<Self> {
        let unreachable = |error: String| DesktopError::BackendUnavailable {
            backend: desktop_core::models::backend::Backend::AtSpi,
            reason: format!("cannot reach the accessibility bus: {error}"),
        };

        let connection = match address {
            None => runtime::try_block_on(AccessibilityConnection::new())?
                .map_err(|error| unreachable(error.to_string()))?,
            Some(address) => {
                let address = address.parse().map_err(|error| {
                    unreachable(format!("{address} is not an address: {error}"))
                })?;
                runtime::try_block_on(AccessibilityConnection::from_address(address))?
                    .map_err(|error| unreachable(error.to_string()))?
            }
        };

        Ok(Self {
            connection,
            space: CoordinateSpace::primary_screen(),
            window_relative,
            info,
            activator: None,
        })
    }

    /// Supplies the window manager to raise windows through.
    #[must_use]
    pub fn with_activator(
        mut self,
        activator: Option<Box<dyn crate::x11::WindowActivator>>,
    ) -> Self {
        self.activator = activator;
        self
    }

    fn conn(&self) -> &zbus::Connection {
        self.connection.connection()
    }

    /// Waits for the application to agree that the window is active.
    ///
    /// The window manager confirming an activation only means the *manager* is
    /// done. The application finds out afterwards, and keystrokes sent in that
    /// gap go to whatever had focus before — observed with Firefox, where a
    /// `focus` immediately followed by `ctrl+l` left the chord half-applied and
    /// the following text typed into the page instead of the address bar.
    ///
    /// Returning after the timeout rather than failing: the window manager did
    /// its part, and some applications never expose an active state at all.
    fn settle(&self, window: &ObjectRefOwned) {
        let deadline = std::time::Instant::now() + FOCUS_SETTLE_TIMEOUT;
        while std::time::Instant::now() < deadline {
            if runtime::block_on(self.is_active(window)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    }

    /// Whether a window is the one the compositor considers active.
    async fn is_active(&self, object: &ObjectRefOwned) -> bool {
        let Ok(proxy) = object.as_accessible_proxy(self.conn()).await else {
            return false;
        };
        proxy
            .get_state()
            .await
            .is_ok_and(|set| set.contains(State::Active))
    }

    async fn root(&self) -> Result<AccessibleProxy<'_>> {
        self.connection
            .root_accessible_on_registry()
            .await
            .map_err(|error| {
                DesktopError::backend(format!("cannot read the a11y registry: {error}"))
            })
    }

    /// The applications registered on the accessibility bus.
    ///
    /// Every per-application read tolerates failure: an application that exits
    /// between the listing and the call is a normal race, not a reason to fail
    /// the whole command.
    async fn applications(&self) -> Result<Vec<AppEntry>> {
        let root = self.root().await?;
        let children = root
            .get_children()
            .await
            .map_err(|error| DesktopError::backend(format!("cannot list applications: {error}")))?;

        let mut entries = Vec::new();
        for object in children {
            if object.is_null() {
                continue;
            }
            let Ok(proxy) = object.as_accessible_proxy(self.conn()).await else {
                continue;
            };
            let name = proxy.name().await.unwrap_or_default();
            let child_count = proxy.child_count().await.unwrap_or(0);
            let bus_name = object.name_as_str().unwrap_or_default().to_owned();
            let pid = self.pid_for(&bus_name).await;

            entries.push(AppEntry {
                object: object.clone(),
                bus_name,
                name,
                pid,
                child_count: u32::try_from(child_count).unwrap_or(0),
            });
        }
        Ok(entries)
    }

    /// AT-SPI does not expose a pid, so it is resolved from the owning D-Bus
    /// connection instead.
    async fn pid_for(&self, bus_name: &str) -> ProcessId {
        let fallback = ProcessId::new(0);
        if bus_name.is_empty() {
            return fallback;
        }
        let Ok(proxy) = zbus::fdo::DBusProxy::new(self.conn()).await else {
            return fallback;
        };
        let Ok(name) = bus_name.try_into() else {
            return fallback;
        };
        proxy
            .get_connection_unix_process_id(name)
            .await
            .ok()
            .and_then(|pid| i32::try_from(pid).ok())
            .map_or(fallback, ProcessId::new)
    }

    /// Top-level windows belonging to one application.
    async fn windows_of(&self, app: &AppEntry) -> Result<Vec<WindowEntry>> {
        let Ok(proxy) = app.object.as_accessible_proxy(self.conn()).await else {
            return Ok(Vec::new());
        };
        let Ok(children) = proxy.get_children().await else {
            return Ok(Vec::new());
        };

        let mut windows = Vec::new();
        for (index, object) in children.into_iter().enumerate() {
            if object.is_null() {
                continue;
            }
            let Ok(child) = object.as_accessible_proxy(self.conn()).await else {
                continue;
            };
            let role_name = child.get_role_name().await.unwrap_or_default();
            if !WINDOW_ROLES.contains(&role_name.to_ascii_lowercase().as_str()) {
                continue;
            }
            let title = child.name().await.ok().filter(|name| !name.is_empty());
            let states = child.get_state().await.ok();
            let focused = states.is_some_and(|set| set.contains(State::Active));

            windows.push(WindowEntry {
                object,
                title,
                focused,
                index: u16::try_from(index).unwrap_or(u16::MAX),
                app: app.key(),
            });
        }
        Ok(windows)
    }

    /// Finds the window a target designates.
    async fn locate(&self, target: &Target) -> Result<(AppEntry, WindowEntry)> {
        let apps = self.applications().await?;

        let candidates: Vec<&AppEntry> = match target {
            Target::App(needle) => apps
                .iter()
                .filter(|app| app.key().matches(needle))
                .collect(),
            Target::Focused | Target::Window(_) => apps.iter().collect(),
        };

        if candidates.is_empty() {
            return Err(DesktopError::TargetNotFound {
                target: target.describe(),
            });
        }

        let mut all = Vec::new();
        for app in candidates {
            for window in self.windows_of(app).await? {
                all.push((app.clone(), window));
            }
        }

        match target {
            Target::Window(id) => {
                all.into_iter()
                    .nth(id.get() as usize)
                    .ok_or_else(|| DesktopError::TargetNotFound {
                        target: target.describe(),
                    })
            }
            Target::Focused | Target::App(_) => all
                .iter()
                .find(|(_, window)| window.focused)
                .cloned()
                .or_else(|| all.first().cloned())
                .ok_or_else(|| DesktopError::TargetNotFound {
                    target: target.describe(),
                }),
        }
    }

    /// Depth-first walk building the normalized tree.
    ///
    /// Guards against cycles. AT-SPI trees are supposed to be acyclic, but a
    /// buggy toolkit can report a parent as its own descendant, and an infinite
    /// walk would hang the CLI rather than fail it.
    ///
    /// Text is read through the `Text` interface before `Value`, because
    /// `Value` is the *numeric* one — sliders and progress bars — and reading a
    /// text field through it yields nothing. A text box whose contents are
    /// invisible cannot be verified after writing to it.
    ///
    /// Each node records its AT-SPI reference, which is a stable, serializable
    /// handle and can therefore be reused directly — unlike macOS, where
    /// re-walking the tree is the only option.
    async fn walk(
        &self,
        object: &ObjectRefOwned,
        budget: WalkBudget,
        depth: usize,
        visited: &mut usize,
        seen: &mut HashSet<String>,
    ) -> Option<RawNode> {
        if *visited >= budget.max_nodes || depth > budget.max_depth {
            return None;
        }
        let key = format!(
            "{}{}",
            object.name_as_str().unwrap_or_default(),
            object.path_as_str()
        );
        if !seen.insert(key.clone()) {
            return None;
        }
        *visited += 1;

        let proxy = object.as_accessible_proxy(self.conn()).await.ok()?;

        let role_name = proxy.get_role_name().await.unwrap_or_default();
        let role = role::from_atspi(&role_name);
        let name = proxy.name().await.ok().filter(|n| !n.is_empty());
        let description = proxy.description().await.ok().filter(|d| !d.is_empty());
        let interfaces = proxy.get_interfaces().await.ok();
        let states = proxy.get_state().await.ok();

        let has = |interface: Interface| interfaces.is_some_and(|set| set.contains(interface));

        let bounds = if has(Interface::Component) {
            self.extents(object).await
        } else {
            None
        };
        let actions = if has(Interface::Action) {
            self.actions(object).await
        } else {
            Vec::new()
        };
        let value = if has(Interface::Text) {
            self.text(object).await.filter(|text| !text.is_empty())
        } else if has(Interface::Value) {
            self.number(object).await
        } else {
            None
        };

        let mut node = RawNode::new(role);
        node.name = name;
        node.description = description;
        node.value = value;
        node.bounds = bounds;
        node.actions = actions;
        node.states = translate_states(states);
        node.native = Some(key);

        let children = proxy.get_children().await.unwrap_or_default();
        for child in children {
            if child.is_null() {
                continue;
            }
            if let Some(built) = Box::pin(self.walk(&child, budget, depth + 1, visited, seen)).await
            {
                node.children.push(built);
            }
            if *visited >= budget.max_nodes {
                break;
            }
        }

        Some(node)
    }

    async fn extents(&self, object: &ObjectRefOwned) -> Option<Bounds> {
        let proxy = self.component(object).await?;
        let (x, y, width, height) = proxy
            .get_extents(coord_type(self.window_relative))
            .await
            .ok()?;
        Some(Bounds::new(x, y, width, height))
    }

    /// The element's actions, by *unlocalized* name.
    ///
    /// `GetActions` returns names translated into the user's language — a
    /// Portuguese GTK4 button reports "Clicar", not "click" — so matching on it
    /// would make element activation work only in English locales. `GetName`
    /// returns the canonical name and is what this uses instead.
    async fn actions(&self, object: &ObjectRefOwned) -> Vec<ElementAction> {
        let Some(proxy) = self.action(object).await else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (_, name) in action_names(&proxy).await {
            if let Some(normalized) = ElementAction::from_platform(&name)
                && !out.contains(&normalized)
            {
                out.push(normalized);
            }
        }
        out
    }

    /// The element's text contents, via the `Text` interface.
    ///
    /// Requested as `0..-1`, where -1 means "to the end". An empty field
    /// yields `Some("")`, deliberately distinct from `None`: a caller needs to
    /// tell "this field is empty" from "this element has no text interface at
    /// all".
    async fn text(&self, object: &ObjectRefOwned) -> Option<String> {
        let name = object.name()?.clone();
        let proxy = TextProxy::builder(self.conn())
            .destination(zbus::names::BusName::Unique(name))
            .ok()?
            .path(object.path().clone())
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()?;
        proxy.get_text(0, -1).await.ok()
    }

    async fn number(&self, object: &ObjectRefOwned) -> Option<String> {
        let name = object.name()?.clone();
        let proxy = ValueProxy::builder(self.conn())
            .destination(zbus::names::BusName::Unique(name))
            .ok()?
            .path(object.path().clone())
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()?;
        let current = proxy.current_value().await.ok()?;
        Some(format_number(current))
    }

    async fn component(&self, object: &ObjectRefOwned) -> Option<ComponentProxy<'_>> {
        let name = object.name()?.clone();
        ComponentProxy::builder(self.conn())
            .destination(zbus::names::BusName::Unique(name))
            .ok()?
            .path(object.path().clone())
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()
    }

    async fn editable_text(&self, object: &ObjectRefOwned) -> Option<EditableTextProxy<'_>> {
        let name = object.name()?.clone();
        EditableTextProxy::builder(self.conn())
            .destination(zbus::names::BusName::Unique(name))
            .ok()?
            .path(object.path().clone())
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()
    }

    async fn action(&self, object: &ObjectRefOwned) -> Option<ActionProxy<'_>> {
        let name = object.name()?.clone();
        ActionProxy::builder(self.conn())
            .destination(zbus::names::BusName::Unique(name))
            .ok()?
            .path(object.path().clone())
            .ok()?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .ok()
    }

    /// Re-walks the window a path names and follows its steps.
    async fn resolve_path(&self, target: &ElementPath) -> Result<RawNode> {
        let (_, window) = self
            .locate(&Target::App(target.app.name.clone()))
            .await
            .map_err(|_| DesktopError::TargetNotFound {
                target: format!("application {:?}", target.app.name),
            })?;

        let mut visited = 0;
        let mut seen = HashSet::new();
        let root = self
            .walk(
                &window.object,
                WalkBudget::default(),
                0,
                &mut visited,
                &mut seen,
            )
            .await
            .ok_or_else(|| DesktopError::TargetNotFound {
                target: format!("window of {:?}", target.app.name),
            })?;

        path::resolve(&root, &target.steps)
            .cloned()
            .map_err(|reason| DesktopError::ElementStale {
                element: desktop_core::models::ids::ElementId::new(0),
                reason,
            })
    }
}

#[derive(Clone)]
struct AppEntry {
    object: ObjectRefOwned,
    bus_name: String,
    name: String,
    pid: ProcessId,
    child_count: u32,
}

impl AppEntry {
    fn key(&self) -> AppKey {
        AppKey::new(self.pid, &self.name).with_identifier(&self.bus_name)
    }
}

#[derive(Clone)]
struct WindowEntry {
    object: ObjectRefOwned,
    title: Option<String>,
    focused: bool,
    index: u16,
    app: AppKey,
}

impl AccessibilityPort for AtSpi {
    fn list_apps(&self) -> Result<Vec<Application>> {
        runtime::try_block_on(async {
            let apps = self.applications().await?;
            Ok(apps
                .into_iter()
                .map(|app| Application {
                    pid: app.pid,
                    name: app.name.clone(),
                    identifier: Some(app.bus_name.clone()),
                    active: false,
                    window_count: app.child_count,
                })
                .collect())
        })?
    }

    /// The windows of one application, or of all of them.
    ///
    /// Bounds come from AT-SPI, which under Wayland reports them
    /// window-relative — meaningless as a screen position. The driver reports
    /// the coordinate space alongside them so nothing downstream misreads
    /// them as absolute.
    fn list_windows(&self, app: Option<&AppKey>) -> Result<Vec<Window>> {
        runtime::try_block_on(async {
            let apps = self.applications().await?;
            let mut out = Vec::new();
            let mut next_id = 0u32;

            for entry in apps {
                if let Some(filter) = app
                    && !entry.key().matches(&filter.name)
                {
                    continue;
                }
                for window in self.windows_of(&entry).await? {
                    let bounds = self.extents(&window.object).await;
                    out.push(Window {
                        id: WindowId::new(next_id),
                        title: window.title.clone(),
                        app: window.app.clone(),
                        bounds,
                        focused: window.focused,
                        minimized: false,
                        index: window.index,
                    });
                    next_id += 1;
                }
            }
            Ok(out)
        })?
    }

    fn tree(&self, target: &Target, budget: WalkBudget) -> Result<ResolvedTree> {
        runtime::try_block_on(async {
            let (app, window) = self.locate(target).await?;
            let mut visited = 0;
            let mut seen = HashSet::new();
            let root = self
                .walk(&window.object, budget, 0, &mut visited, &mut seen)
                .await
                .ok_or_else(|| DesktopError::TargetNotFound {
                    target: target.describe(),
                })?;

            let space = if self.window_relative {
                CoordinateSpace::Window(WindowId::new(u32::from(window.index)))
            } else {
                self.space
            };

            Ok(ResolvedTree {
                app: app.key(),
                window: Window {
                    id: WindowId::new(u32::from(window.index)),
                    title: window.title.clone(),
                    app: window.app.clone(),
                    bounds: root.bounds,
                    focused: window.focused,
                    minimized: false,
                    index: window.index,
                },
                root,
                space,
            })
        })?
    }

    fn resolve(&self, target: &ElementPath) -> Result<RawNode> {
        runtime::try_block_on(self.resolve_path(target))?
    }

    fn perform(&self, target: &ElementPath, action: ElementAction) -> Result<()> {
        runtime::try_block_on(async {
            let node = self.resolve_path(target).await?;
            let native = node.native.clone().ok_or_else(|| {
                DesktopError::internal("resolved element carries no accessibility handle")
            })?;
            let object = parse_native(&native)
                .ok_or_else(|| DesktopError::internal("accessibility handle is malformed"))?;

            let proxy = self.action(&object).await.ok_or_else(|| {
                DesktopError::invalid_argument("element does not support accessibility actions")
            })?;
            let index = action_names(&proxy)
                .await
                .into_iter()
                .find(|(_, name)| ElementAction::from_platform(name) == Some(action))
                .map(|(index, _)| index)
                .ok_or_else(|| {
                    DesktopError::invalid_argument(format!(
                        "element does not offer the {} action",
                        action.as_str()
                    ))
                })?;

            let performed = proxy
                .do_action(index)
                .await
                .map_err(|error| DesktopError::backend(format!("action failed: {error}")))?;
            if performed {
                Ok(())
            } else {
                Err(DesktopError::backend(
                    "the application refused the accessibility action",
                ))
            }
        })?
    }

    /// Replaces an element's text, and proves it happened.
    ///
    /// The write replaces rather than appends, so the result does not depend on
    /// whatever the field happened to contain.
    ///
    /// It is then read back, because success from `SetTextContents` does not
    /// mean the text landed: Firefox's URL bar returns success and does
    /// nothing, its accessibility interface not being wired to the underlying
    /// XUL input. Trusting the return value would report a write that never
    /// happened — the confidently-wrong outcome this tool exists to avoid.
    /// An element with no `Text` interface offers nothing to verify against, so
    /// there the application's word is all there is.
    fn set_text(&self, target: &ElementPath, text: &str) -> Result<()> {
        runtime::try_block_on(async {
            let node = self.resolve_path(target).await?;
            let native = node.native.clone().ok_or_else(|| {
                DesktopError::internal("resolved element carries no accessibility handle")
            })?;
            let object = parse_native(&native)
                .ok_or_else(|| DesktopError::internal("accessibility handle is malformed"))?;

            let proxy = self.editable_text(&object).await.ok_or_else(|| {
                DesktopError::invalid_argument(
                    "this element does not accept text through the accessibility API; \
                     focus it and use `desktop type` instead",
                )
            })?;

            let written = proxy.set_text_contents(text).await.map_err(|error| {
                DesktopError::backend(format!("cannot set element text: {error}"))
            })?;
            if !written {
                return Err(DesktopError::backend(
                    "the application refused to set the element's text",
                ));
            }

            match self.text(&object).await {
                Some(actual) if actual == text => Ok(()),
                None if text.is_empty() => Ok(()),
                None => Err(DesktopError::backend(
                    "the application accepted the text but exposes no way to read it \
                     back, so the write could not be confirmed",
                )),
                Some(actual) => Err(DesktopError::backend(format!(
                    "the application accepted the text but did not apply it (field now \
                     reads {actual:?}). Some widgets — Firefox's address bar among them \
                     — ignore accessibility text writes; focus the field and use \
                     `desktop type` instead."
                ))),
            }
        })?
    }

    /// Raises a window and gives it the keyboard.
    ///
    /// The window manager is tried first where there is one: `_NET_ACTIVE_WINDOW`
    /// both raises and focuses, and — unlike `GrabFocus` — its result is
    /// observable, so success is verified rather than assumed.
    ///
    /// `GrabFocus` is the fallback, and it returns success whether or not
    /// anything happened. Under Wayland there is no client-initiated raise at
    /// all, so on GNOME it is reliably a no-op; reporting a focus change that
    /// did not occur would send every later keystroke to the wrong window,
    /// which is why the window is checked for the active state afterwards.
    fn focus(&self, target: &Target) -> Result<()> {
        let (app, window) = runtime::try_block_on(self.locate(target))??;

        if let Some(activator) = &self.activator
            && activator.activate(u32::try_from(app.pid.get()).ok(), window.title.as_deref())?
        {
            self.settle(&window.object);
            return Ok(());
        }

        runtime::try_block_on(async {
            let proxy = self.component(&window.object).await.ok_or_else(|| {
                DesktopError::backend("window does not expose a component interface")
            })?;
            proxy
                .grab_focus()
                .await
                .map_err(|error| DesktopError::backend(format!("cannot focus window: {error}")))?;

            if !self.is_active(&window.object).await {
                return Err(DesktopError::unsupported(
                    desktop_core::models::capability::Capability::Focus,
                    desktop_core::models::backend::Backend::AtSpi,
                    &self.info,
                ));
            }
            Ok(())
        })?
    }
}

/// Enumerates `(index, unlocalized_name)` for an element's actions.
async fn action_names(proxy: &ActionProxy<'_>) -> Vec<(i32, String)> {
    let Ok(count) = proxy.n_actions().await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for index in 0..count {
        if let Ok(name) = proxy.get_name(index).await {
            out.push((index, name));
        }
    }
    out
}

/// Recovers an object reference from the handle recorded during a walk.
fn parse_native(native: &str) -> Option<ObjectRefOwned> {
    let split = native.find('/')?;
    let (name, path) = native.split_at(split);
    let name = atspi::zbus::names::UniqueName::try_from(name.to_owned()).ok()?;
    let path = atspi::zbus::zvariant::ObjectPath::try_from(path.to_owned()).ok()?;
    Some(atspi::ObjectRef::new_owned(name, path))
}

/// Maps an AT-SPI state set onto the normalized states.
///
/// Three of these are judgement calls rather than lookups.
///
/// **Visible** excludes `Defunct`, which is a dangling reference to something
/// already destroyed; reporting it as present would offer an agent an element
/// it can never act on.
///
/// **Enabled** accepts `Sensitive` *or* `Enabled`. Probed on GTK4
/// (gnome-calculator, GNOME 49): live buttons report `SENSITIVE|SHOWING|VISIBLE`
/// and never set `ENABLED`, so requiring both marks an entire application
/// disabled. A genuinely insensitive widget clears `SENSITIVE`, which is what
/// actually distinguishes the two.
///
/// **Protected** is always false. AT-SPI has no password state — the
/// `password text` role is the signal, and the normalizer keys redaction off
/// that instead.
fn translate_states(set: Option<StateSet>) -> States {
    let Some(set) = set else {
        return States::usable();
    };
    let alive = !set.contains(State::Defunct);
    States {
        enabled: set.contains(State::Sensitive) || set.contains(State::Enabled),
        focused: set.contains(State::Focused),
        focusable: set.contains(State::Focusable),
        selected: set.contains(State::Selected),
        checked: set.contains(State::Checked),
        expanded: set.contains(State::Expanded),
        visible: alive && set.contains(State::Visible),
        showing: alive && set.contains(State::Showing),
        protected: false,
    }
}

/// Which coordinate space to ask AT-SPI for.
///
/// Under Wayland a toolkit cannot know its own screen position, so
/// `CoordType::Screen` returns surface-relative numbers dressed up as absolute
/// ones. Asking for `Window` explicitly gets the same numbers with an honest
/// label, which the snapshot then carries.
const fn coord_type(window_relative: bool) -> CoordType {
    if window_relative {
        CoordType::Window
    } else {
        CoordType::Screen
    }
}

/// Renders an AT-SPI numeric value without a trailing `.0` for whole numbers,
/// so a slider at 5 reads as `5` rather than `5.0`.
fn format_number(value: f64) -> String {
    if value.fract().abs() < f64::EPSILON {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_roles_cover_the_shapes_toolkits_actually_report() {
        for role in ["frame", "window", "dialog", "alert"] {
            assert!(
                WINDOW_ROLES.contains(&role),
                "{role} should be a window role"
            );
        }
        assert!(!WINDOW_ROLES.contains(&"panel"));
    }

    #[test]
    fn missing_state_information_is_treated_as_usable_rather_than_broken() {
        // A toolkit that does not answer GetState should not have all its
        // elements silently pruned as invisible.
        let states = translate_states(None);
        assert!(states.enabled);
        assert!(states.visible);
        assert!(states.showing);
    }

    #[test]
    fn a_gtk4_widget_reporting_only_sensitive_is_treated_as_enabled() {
        // Probed on gnome-calculator under GNOME 49: live buttons report
        // SENSITIVE|SHOWING|VISIBLE and never ENABLED. Requiring both marked
        // every button in the application disabled.
        let gtk4 = StateSet::new(State::Sensitive | State::Showing | State::Visible);
        assert!(translate_states(Some(gtk4)).enabled);
    }

    #[test]
    fn a_widget_reporting_only_enabled_is_also_treated_as_enabled() {
        assert!(translate_states(Some(StateSet::new(State::Enabled))).enabled);
    }

    #[test]
    fn an_insensitive_widget_is_reported_as_disabled() {
        // A genuinely greyed-out control clears SENSITIVE; that is the signal
        // that actually distinguishes the two.
        let greyed = StateSet::new(State::Showing | State::Visible);
        assert!(!translate_states(Some(greyed)).enabled);
    }

    #[test]
    fn state_translation_carries_focus_and_selection_through() {
        let set =
            StateSet::new(State::Enabled | State::Sensitive | State::Focused | State::Selected);
        let states = translate_states(Some(set));
        assert!(states.focused);
        assert!(states.selected);
        assert!(states.enabled);
    }

    #[test]
    fn native_handles_round_trip_through_the_recorded_string_form() {
        let native = ":1.13/org/a11y/atspi/accessible/root";
        let parsed = parse_native(native).expect("parses");
        assert_eq!(parsed.name_as_str(), Some(":1.13"));
        assert_eq!(parsed.path_as_str(), "/org/a11y/atspi/accessible/root");
    }

    #[test]
    fn a_malformed_native_handle_is_rejected_rather_than_panicking() {
        assert!(parse_native("").is_none());
        assert!(parse_native("no-path-here").is_none());
    }

    #[test]
    fn numeric_values_render_without_a_spurious_decimal_point() {
        assert_eq!(format_number(5.0), "5");
        assert_eq!(format_number(0.0), "0");
        assert_eq!(format_number(2.5), "2.5");
    }

    #[test]
    fn wayland_sessions_ask_for_window_coordinates_because_screen_ones_are_fiction() {
        // Verified on GNOME 49: GetExtents(Screen) on a window at y=32 returns
        // y=0, so requesting Screen hands back a confident wrong number.
        assert_eq!(coord_type(true), CoordType::Window);
        assert_eq!(coord_type(false), CoordType::Screen);
    }

    #[test]
    fn a_defunct_object_is_reported_as_neither_visible_nor_showing() {
        let set = StateSet::new(State::Defunct | State::Visible | State::Showing);
        let states = translate_states(Some(set));
        assert!(!states.visible);
        assert!(!states.showing);
    }
}
