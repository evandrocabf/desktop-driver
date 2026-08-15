//! AT-SPI accessibility adapter.
//!
//! This is the one Linux backend that is *display-server independent*: it works
//! identically under X11 and Wayland, because it is D-Bus all the way down and
//! never talks to the compositor. That is why it carries window enumeration on
//! Wayland, where no window-listing protocol is available to an ordinary
//! client.
//!
//! Under X11 it is the *junior* partner for that one job: the window manager
//! knows about every window, including those belonging to applications with no
//! accessibility support at all, and knows their stacking order and their real
//! screen position. So the two are joined — see [`AtSpi::window_table`] — with
//! EWMH deciding which windows exist and AT-SPI supplying the tree behind each.
//!
//! The coordinate caveat is load-bearing and is handled in [`extents`]: under
//! Wayland a toolkit cannot know its own screen position, so
//! `GetExtents(Screen)` returns surface-relative numbers. Rather than pass
//! those off as screen coordinates, the adapter reports the space it is
//! actually working in and lets the core carry that through to the snapshot.

use std::collections::{HashMap, HashSet};

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
    /// The window manager, where the display server has one this can talk to.
    /// Supplies the window list and the only reliable way to raise a window.
    windows: Option<Box<dyn crate::x11::WindowSource>>,
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
            windows: None,
        })
    }

    /// Supplies the window manager to enumerate and raise windows through.
    #[must_use]
    pub fn with_window_source(
        mut self,
        windows: Option<Box<dyn crate::x11::WindowSource>>,
    ) -> Self {
        self.windows = windows;
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

    /// Every window on this display, in one order, numbered once.
    ///
    /// This is the single answer to "which windows are there": `desktop
    /// windows` prints it and `--window N` indexes it. They used to be two
    /// separate walks numbered independently, so `desktop windows --app
    /// Firefox` printed ids that `--window` then resolved against a different,
    /// unfiltered list — the second window of the second application answered
    /// to whichever id its position in the *filtered* list happened to give it.
    ///
    /// Where a window manager is available it decides which windows exist, in
    /// what order, and where they are; AT-SPI supplies the tree behind each.
    /// That is what makes an application with no accessibility support appear
    /// here at all, and the row says so — see [`Window::accessible`].
    ///
    /// The EWMH rows come first and in the window manager's order, and nothing
    /// may reorder them: capture resolves a `WindowId` back to an `XID` by
    /// recomputing that list in another process, with no access to this one's
    /// AT-SPI join. Frames the window manager did not account for follow after.
    /// Under Wayland that is every window; under X11 it is an application whose
    /// accessibility frame outlived its window, or one that was never managed.
    ///
    /// Windows with no frame are numbered from past the AT-SPI indices already
    /// in use for their application, so the two cannot collide in a
    /// [`WindowKey`](desktop_core::models::app::WindowKey).
    async fn window_table(&self) -> Result<Vec<Row>> {
        let mut frames: Vec<Option<(AppEntry, WindowEntry)>> = Vec::new();
        for app in self.applications().await? {
            for window in self.windows_of(&app).await? {
                frames.push(Some((app.clone(), window)));
            }
        }

        let managed = match &self.windows {
            Some(source) if self.info.windows == desktop_core::models::backend::Backend::Ewmh => {
                source.toplevels()?
            }
            _ => Vec::new(),
        };

        let mut ordinals: HashMap<i32, u16> =
            frames
                .iter()
                .flatten()
                .fold(HashMap::new(), |mut counts, (app, _)| {
                    *counts.entry(app.pid.get()).or_insert(0) += 1;
                    counts
                });

        let mut rows = Vec::new();
        for window in &managed {
            let frame = claim_frame(&mut frames, window);
            let app = match &frame {
                Some((app, _)) => app.key(),
                None => fallback_key(window),
            };
            let index = match &frame {
                Some((_, entry)) => entry.index,
                None => {
                    let next = ordinals.entry(app.pid.get()).or_insert(0);
                    let index = *next;
                    *next = next.saturating_add(1);
                    index
                }
            };
            rows.push(Row {
                id: WindowId::new(0),
                title: window
                    .title
                    .clone()
                    .or_else(|| frame.as_ref().and_then(|(_, entry)| entry.title.clone())),
                app,
                bounds: window.bounds,
                focused: window.focused,
                minimized: window.minimized,
                index,
                frame,
            });
        }

        for frame in frames.into_iter().flatten() {
            let bounds = self.extents(&frame.1.object).await;
            rows.push(Row {
                id: WindowId::new(0),
                title: frame.1.title.clone(),
                app: frame.1.app.clone(),
                bounds,
                focused: frame.1.focused,
                minimized: false,
                index: frame.1.index,
                frame: Some(frame),
            });
        }

        for (position, row) in rows.iter_mut().enumerate() {
            row.id = WindowId::new(u32::try_from(position).unwrap_or(u32::MAX));
        }
        Ok(rows)
    }

    /// Finds the window a target designates.
    ///
    /// Only rows with a tree can be returned, because every caller needs one.
    /// A window that exists without a tree is reported as exactly that rather
    /// than as missing: the difference is whether the agent should look for
    /// another window or stop using the accessibility route for this one.
    async fn locate(&self, target: &Target) -> Result<Row> {
        let table = self.window_table().await?;
        let candidates: Vec<Row> = match target {
            Target::Window(id) => table.into_iter().filter(|row| row.id == *id).collect(),
            Target::App(needle) => table
                .into_iter()
                .filter(|row| row.app.matches(needle))
                .collect(),
            Target::Focused => table,
        };

        let chosen = candidates
            .iter()
            .position(|row| row.focused && row.frame.is_some())
            .or_else(|| candidates.iter().position(|row| row.frame.is_some()));

        match chosen {
            Some(position) => Ok(candidates
                .into_iter()
                .nth(position)
                .expect("position came from this list")),
            None if candidates.is_empty() => Err(DesktopError::TargetNotFound {
                target: target.describe(),
            }),
            None => Err(DesktopError::TargetNotFound {
                target: format!(
                    "an accessibility tree for {} (the window manager reports it, but {} \
                     exposes none, so it can only be screenshotted or clicked by coordinate)",
                    target.describe(),
                    candidates
                        .first()
                        .map_or("its application", |row| row.app.name.as_str())
                ),
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
            })?
            .frame
            .expect("locate returns rows with a tree");

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

/// One row of the window table.
struct Row {
    id: WindowId,
    title: Option<String>,
    app: AppKey,
    bounds: Option<Bounds>,
    focused: bool,
    minimized: bool,
    index: u16,
    /// The AT-SPI frame behind this window, absent where the application
    /// exposes none.
    frame: Option<(AppEntry, WindowEntry)>,
}

/// Takes the AT-SPI frame describing the same window as `window`, if one is
/// still unclaimed.
///
/// Pid *and* title first, since one application has a single pid across all of
/// its windows and the title is what tells them apart. Pid alone next, in list
/// order, which is the best available guess when the two reads caught a title
/// mid-change. Title alone last, for an application that publishes no
/// `_NET_WM_PID` — Java toolkits and anything running over an X11 forward.
fn claim_frame(
    frames: &mut [Option<(AppEntry, WindowEntry)>],
    window: &crate::x11::ManagedWindow,
) -> Option<(AppEntry, WindowEntry)> {
    let pid = window.pid.and_then(|pid| i32::try_from(pid).ok());
    let same_pid = |entry: &(AppEntry, WindowEntry)| pid == Some(entry.0.pid.get());
    let same_title =
        |entry: &(AppEntry, WindowEntry)| window.title.is_some() && entry.1.title == window.title;

    let position = frames
        .iter()
        .position(|slot| {
            slot.as_ref()
                .is_some_and(|entry| same_pid(entry) && same_title(entry))
        })
        .or_else(|| {
            frames
                .iter()
                .position(|slot| slot.as_ref().is_some_and(&same_pid))
        })
        .or_else(|| {
            frames
                .iter()
                .position(|slot| slot.as_ref().is_some_and(&same_title))
        })?;
    frames[position].take()
}

/// Names an application that is on screen but not on the accessibility bus.
///
/// `WM_CLASS` is what a window manager and a taskbar use for the same purpose.
/// A pid of 0 means the window published none — the same convention the AT-SPI
/// side already uses when D-Bus cannot resolve one.
fn fallback_key(window: &crate::x11::ManagedWindow) -> AppKey {
    let pid = window
        .pid
        .and_then(|pid| i32::try_from(pid).ok())
        .unwrap_or_default();
    let name = window
        .class
        .as_deref()
        .or(window.title.as_deref())
        .unwrap_or("(unknown)");
    AppKey::new(ProcessId::new(pid), name)
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
    /// Filtering happens *after* the table is numbered, so an id printed under
    /// `--app` is the same id `--window` resolves.
    ///
    /// Bounds come from the window manager where there is one, and from AT-SPI
    /// otherwise — which under Wayland reports them window-relative, meaningless
    /// as a screen position. The driver reports the coordinate space alongside
    /// them so nothing downstream misreads them as absolute.
    fn list_windows(&self, app: Option<&AppKey>) -> Result<Vec<Window>> {
        runtime::try_block_on(async {
            let table = self.window_table().await?;
            Ok(table
                .into_iter()
                .filter(|row| app.is_none_or(|filter| row.app.matches(&filter.name)))
                .map(|row| Window {
                    id: row.id,
                    title: row.title,
                    app: row.app,
                    bounds: row.bounds,
                    focused: row.focused,
                    minimized: row.minimized,
                    accessible: row.frame.is_some(),
                    index: row.index,
                })
                .collect())
        })?
    }

    /// The window a target designates, plus its tree.
    ///
    /// The window a coordinate is relative to is named by its table id, which
    /// is what `desktop windows` printed and what `--window` accepts. It used
    /// to be the window's ordinal within its own application, so
    /// `{"window": 1}` on a snapshot of the second application's second window
    /// named the wrong window entirely.
    fn tree(&self, target: &Target, budget: WalkBudget) -> Result<ResolvedTree> {
        runtime::try_block_on(async {
            let row = self.locate(target).await?;
            let (app, window) = row.frame.clone().expect("locate returns rows with a tree");
            let mut visited = 0;
            let mut seen = HashSet::new();
            let root = self
                .walk(&window.object, budget, 0, &mut visited, &mut seen)
                .await
                .ok_or_else(|| DesktopError::TargetNotFound {
                    target: target.describe(),
                })?;

            let space = if self.window_relative {
                CoordinateSpace::Window(row.id)
            } else {
                self.space
            };

            Ok(ResolvedTree {
                app: app.key(),
                window: Window {
                    id: row.id,
                    title: row.title.clone(),
                    app: row.app.clone(),
                    bounds: root.bounds,
                    focused: row.focused,
                    minimized: row.minimized,
                    accessible: true,
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
    /// Three routes, each verified rather than trusted, in descending order of
    /// how much authority the thing being asked actually has.
    ///
    /// The window manager first, where there is one: `_NET_ACTIVE_WINDOW` both
    /// raises and focuses, and its result is observable on the root window.
    ///
    /// Then the application itself, which is all Wayland leaves —
    /// `org.freedesktop.Application.Activate`, asking the program to present
    /// its own window. It reaches only applications exporting that interface,
    /// and the compositor may answer a present by marking the window as
    /// demanding attention instead, so what it returns is "the application was
    /// asked", not "the window is now focused". The difference is settled by
    /// looking.
    ///
    /// `GrabFocus` last, which returns success whether or not anything
    /// happened. Reporting a focus change that did not occur would send every
    /// later keystroke to a window the caller never looked at, so the window is
    /// checked for the active state afterwards and a refusal is reported as
    /// one.
    fn focus(&self, target: &Target) -> Result<()> {
        let row = runtime::try_block_on(self.locate(target))??;
        let (app, window) = row.frame.clone().expect("locate returns rows with a tree");

        if let Some(activator) = &self.windows
            && activator.activate(u32::try_from(app.pid.get()).ok(), window.title.as_deref())?
        {
            self.settle(&window.object);
            return Ok(());
        }

        if crate::activate::present_application(app.pid.get()).unwrap_or(false) {
            self.settle(&window.object);
            if runtime::block_on(self.is_active(&window.object)) {
                return Ok(());
            }
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
    use crate::x11::ManagedWindow;

    fn frame(pid: i32, app: &str, title: Option<&str>, index: u16) -> (AppEntry, WindowEntry) {
        let object = parse_native(":1.7/org/a11y/atspi/accessible/1").expect("a valid handle");
        (
            AppEntry {
                object: object.clone(),
                bus_name: ":1.7".to_owned(),
                name: app.to_owned(),
                pid: ProcessId::new(pid),
                child_count: 1,
            },
            WindowEntry {
                object,
                title: title.map(str::to_owned),
                focused: false,
                index,
                app: AppKey::new(ProcessId::new(pid), app),
            },
        )
    }

    fn managed(pid: Option<u32>, title: Option<&str>, class: Option<&str>) -> ManagedWindow {
        ManagedWindow {
            xid: 0x40_0001,
            pid,
            title: title.map(str::to_owned),
            class: class.map(str::to_owned),
            bounds: None,
            minimized: false,
            focused: false,
        }
    }

    /// One application has a single pid across every window it owns, so the
    /// title is the only thing telling them apart.
    #[test]
    fn a_frame_is_claimed_by_pid_and_title_before_pid_alone() {
        let mut frames = vec![
            Some(frame(42, "Firefox", Some("GitHub"), 0)),
            Some(frame(42, "Firefox", Some("Crates.io"), 1)),
        ];
        let claimed = claim_frame(&mut frames, &managed(Some(42), Some("Crates.io"), None))
            .expect("a frame matches");
        assert_eq!(claimed.1.index, 1);
    }

    #[test]
    fn a_claimed_frame_is_not_offered_to_the_next_window() {
        let mut frames = vec![
            Some(frame(42, "Firefox", Some("GitHub"), 0)),
            Some(frame(42, "Firefox", Some("GitHub"), 1)),
        ];
        let first = claim_frame(&mut frames, &managed(Some(42), Some("GitHub"), None));
        let second = claim_frame(&mut frames, &managed(Some(42), Some("GitHub"), None));
        let third = claim_frame(&mut frames, &managed(Some(42), Some("GitHub"), None));
        assert_eq!(first.expect("first matches").1.index, 0);
        assert_eq!(second.expect("second matches").1.index, 1);
        assert!(third.is_none(), "there were only two frames");
    }

    /// Java toolkits and anything running over an X11 forward publish no
    /// `_NET_WM_PID`, which leaves the title as the only join.
    #[test]
    fn a_window_with_no_pid_falls_back_to_matching_on_title() {
        let mut frames = vec![Some(frame(42, "IntelliJ", Some("Main.java"), 0))];
        let claimed =
            claim_frame(&mut frames, &managed(None, Some("Main.java"), None)).expect("matches");
        assert_eq!(claimed.0.name, "IntelliJ");
    }

    #[test]
    fn a_window_whose_application_exposes_nothing_claims_no_frame() {
        let mut frames = vec![Some(frame(42, "Firefox", Some("GitHub"), 0))];
        assert!(claim_frame(&mut frames, &managed(Some(99), Some("xclock"), None)).is_none());
        assert!(
            frames[0].is_some(),
            "the unrelated frame must be left alone"
        );
    }

    /// Two windows with neither a pid nor a title in common must not be joined
    /// on the strength of both being untitled.
    #[test]
    fn an_untitled_window_does_not_match_an_untitled_frame_by_default() {
        let mut frames = vec![Some(frame(42, "Firefox", None, 0))];
        assert!(claim_frame(&mut frames, &managed(None, None, None)).is_none());
    }

    #[test]
    fn an_application_with_no_tree_is_named_by_its_wm_class() {
        let key = fallback_key(&managed(Some(1234), Some("xclock"), Some("XClock")));
        assert_eq!(key.name, "XClock");
        assert_eq!(key.pid.get(), 1234);
    }

    #[test]
    fn a_window_with_neither_class_nor_pid_still_produces_a_usable_key() {
        let key = fallback_key(&managed(None, None, None));
        assert_eq!(key.name, "(unknown)");
        assert_eq!(key.pid.get(), 0);
    }

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
