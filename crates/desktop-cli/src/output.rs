//! Rendering, in both human and JSON form.
//!
//! The two forms are produced from the same values rather than one being
//! derived from the other, so a field can never appear in the text and go
//! missing from the JSON.
//!
//! One invariant is enforced everywhere: in `--json` mode the process writes
//! exactly one JSON document to stdout and nothing else. An agent parsing the
//! stream must never have to skip a stray human-readable line.

use std::io::Write;

use desktop_core::{
    Activation,
    agent::AgentSession,
    errors::DesktopError,
    models::{
        app::{Application, Window},
        backend::BackendInfo,
        capability::{CapabilitySet, CapabilityState},
        dependency::SystemDependency,
        element::{Element, RawNode},
        ids::ElementId,
        image::ScreenshotMetadata,
        snapshot::Snapshot,
    },
    ports::{Diagnostic, Severity},
};
use serde_json::json;

pub struct Sink<'a> {
    out: &'a mut dyn Write,
    json: bool,
    /// The agent display everything in this run addressed, when it was not the
    /// user's own.
    display: Option<String>,
}

impl<'a> Sink<'a> {
    pub fn new(out: &'a mut dyn Write, json: bool) -> Self {
        Self {
            out,
            json,
            display: None,
        }
    }

    /// Records that this run is scoped to the agent's display.
    ///
    /// Which screen a command acted on is never allowed to be invisible: a
    /// screenshot of an empty virtual display and a screenshot of a desktop
    /// with nothing open look identical, and only one of them means the agent
    /// is pointed somewhere unexpected.
    #[must_use]
    pub fn on_agent_display(mut self, display: Option<String>) -> Self {
        self.display = display;
        self
    }

    pub const fn is_json(&self) -> bool {
        self.json
    }

    #[must_use]
    pub fn agent_display(&self) -> Option<&str> {
        self.display.as_deref()
    }

    /// Writing to a closed pipe is not a failure worth reporting — the caller
    /// stopped reading, which is their prerogative.
    pub fn line(&mut self, text: &str) {
        let _ = writeln!(self.out, "{text}");
    }

    pub fn blank(&mut self) {
        let _ = writeln!(self.out);
    }

    /// Writes the single JSON document for this command.
    ///
    /// When the run is scoped to an agent display, every document says so —
    /// including error documents, which is exactly when it matters most.
    pub fn value(&mut self, value: &serde_json::Value) {
        let mut value = value.clone();
        if let (Some(display), Some(object)) = (self.display.as_deref(), value.as_object_mut()) {
            object.insert("display".to_owned(), json!(display));
        }
        let _ = writeln!(
            self.out,
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
        );
    }
}

type Rendered = desktop_core::errors::Result<()>;

/// Renders an error in whichever form the caller asked for.
///
/// The remedy is printed in human mode even though it is absent from the JSON:
/// it is the whole point of a permission error, and hiding it behind `--json`
/// would make the human path strictly worse.
pub fn render_error(sink: &mut Sink<'_>, error: &DesktopError) {
    if sink.is_json() {
        let value = serde_json::to_value(error)
            .unwrap_or_else(|_| json!({ "error": "internal", "message": error.to_string() }));
        sink.value(&value);
        return;
    }

    sink.line(&format!("error: {error}"));

    match error {
        DesktopError::PermissionRequired { remedy, .. } if !remedy.is_empty() => {
            sink.blank();
            sink.line(remedy);
        }
        DesktopError::SetupRequired { permission } => {
            sink.blank();
            sink.line(&format!(
                "The {} grant was not completed.",
                permission.as_str()
            ));
            sink.line(
                "A system dialog asks for it the first time. Approve it once and the \
                 grant is remembered; if no dialog appeared, check for one waiting \
                 behind another window.",
            );
        }
        DesktopError::UnsupportedCapability {
            capability,
            display_server,
            desktop_environment,
            ..
        } => {
            sink.blank();
            sink.line(&format!(
                "{} is not available under {} on {}.",
                capability.as_str(),
                display_server.as_str(),
                desktop_environment.as_str()
            ));
            sink.line("Run `desktop capabilities` to see what this session supports.");
        }
        _ => {}
    }
}

pub fn render_info(sink: &mut Sink<'_>, info: &BackendInfo) -> Rendered {
    if sink.is_json() {
        sink.value(&serde_json::to_value(info).unwrap_or_else(|_| json!({})));
        return Ok(());
    }
    sink.line(&format!("Platform:            {}", info.platform.as_str()));
    sink.line(&format!(
        "Display server:      {}",
        info.display_server.as_str()
    ));
    sink.line(&format!(
        "Desktop:             {}",
        info.desktop_environment.as_str()
    ));
    sink.blank();
    sink.line(&format!(
        "Accessibility:       {}",
        info.accessibility.as_str()
    ));
    sink.line(&format!("Windows:             {}", info.windows.as_str()));
    sink.line(&format!(
        "Screenshot:          {}",
        info.screenshot.as_str()
    ));
    sink.line(&format!("Input:               {}", info.input.as_str()));
    Ok(())
}

pub fn render_capabilities(
    sink: &mut Sink<'_>,
    info: &BackendInfo,
    capabilities: &CapabilitySet,
) -> Rendered {
    if sink.is_json() {
        let mut map = serde_json::Map::new();
        for (capability, state) in capabilities.iter() {
            map.insert(
                capability.as_str().to_owned(),
                serde_json::to_value(state).unwrap_or_else(|_| json!({})),
            );
        }
        sink.value(&json!({
            "platform": info.platform.as_str(),
            "display_server": info.display_server.as_str(),
            "desktop_environment": info.desktop_environment.as_str(),
            "capabilities": map,
        }));
        return Ok(());
    }

    sink.line(&format!("Platform: {}", info.platform.as_str()));
    sink.line(&format!("Display server: {}", info.display_server.as_str()));
    sink.line(&format!("Desktop: {}", info.desktop_environment.as_str()));
    sink.blank();
    sink.line("Capabilities:");
    sink.blank();

    let mut notes = Vec::new();
    for (capability, state) in capabilities.iter() {
        sink.line(&format!("  {} {}", state.glyph(), capability.as_str()));
        match state {
            CapabilityState::Degraded { note } => {
                notes.push(format!("  ~ {}: {note}", capability.as_str()));
            }
            CapabilityState::Unsupported { reason } => {
                notes.push(format!(
                    "  ✗ {}: {}",
                    capability.as_str(),
                    describe_reason(reason)
                ));
            }
            CapabilityState::Supported => {}
        }
    }

    if !notes.is_empty() {
        sink.blank();
        sink.line("Notes:");
        sink.blank();
        for note in notes {
            sink.line(&note);
        }
    }
    Ok(())
}

fn describe_reason(reason: &desktop_core::models::capability::UnsupportedReason) -> String {
    use desktop_core::models::capability::UnsupportedReason as Reason;
    match reason {
        Reason::NoBackendMechanism => {
            "this desktop provides no mechanism desktop-driver can use".to_owned()
        }
        Reason::ServiceUnavailable { service } => format!("{service} is not reachable"),
        Reason::NotImplemented => "not implemented in this build".to_owned(),
        Reason::PermissionMissing { permission } => format!("{permission} has not been granted"),
    }
}

pub fn render_doctor(
    sink: &mut Sink<'_>,
    info: &BackendInfo,
    diagnostics: &[Diagnostic],
    dependencies: &[SystemDependency],
    install: Option<&str>,
) -> Rendered {
    let mut sorted: Vec<&Diagnostic> = diagnostics.iter().collect();
    sorted.sort_by_key(|d| crate::run::severity_rank(d.severity));

    if sink.is_json() {
        let entries: Vec<serde_json::Value> = sorted
            .iter()
            .map(|d| {
                json!({
                    "severity": severity_name(d.severity),
                    "summary": d.summary,
                    "remedy": d.remedy,
                })
            })
            .collect();
        sink.value(&json!({
            "platform": info.platform.as_str(),
            "display_server": info.display_server.as_str(),
            "desktop_environment": info.desktop_environment.as_str(),
            "diagnostics": entries,
            "dependencies": dependencies,
            "install_command": install,
        }));
        return Ok(());
    }

    if sorted.is_empty() {
        sink.line("No problems detected.");
    }
    for diagnostic in sorted {
        sink.line(&format!(
            "{} {}",
            severity_glyph(diagnostic.severity),
            diagnostic.summary
        ));
        if let Some(remedy) = &diagnostic.remedy {
            sink.line(&format!("  → {remedy}"));
        }
        sink.blank();
    }

    render_dependencies(sink, dependencies, install);
    Ok(())
}

/// The dependency table.
///
/// Printed whether or not anything is missing: "what does this tool need"
/// should be answerable before the first confusing failure, not only after it.
fn render_dependencies(
    sink: &mut Sink<'_>,
    dependencies: &[SystemDependency],
    install: Option<&str>,
) {
    if dependencies.is_empty() {
        return;
    }
    sink.line("System dependencies:");
    sink.blank();
    for dependency in dependencies {
        let mark = if dependency.present { '✓' } else { '✗' };
        sink.line(&format!(
            "  {mark} {:<20} {:<12} {}",
            dependency.name,
            dependency.need.as_str(),
            dependency.enables
        ));
    }
    if let Some(command) = install {
        sink.blank();
        sink.line("Install what is missing:");
        sink.blank();
        sink.line(&format!("  {command}"));
    }
}

const fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

const fn severity_glyph(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "✗",
        Severity::Warning => "!",
        Severity::Info => "·",
    }
}

pub fn render_setup(
    sink: &mut Sink<'_>,
    ready: bool,
    outstanding: &[String],
    degraded: &[String],
) -> Rendered {
    if sink.is_json() {
        sink.value(&json!({
            "ready": ready,
            "outstanding": outstanding,
            "degraded": degraded,
        }));
        return Ok(());
    }
    if ready {
        sink.line("Setup is complete: no outstanding grants.");
        if !degraded.is_empty() {
            sink.blank();
            sink.line("Caveats that remain in this environment:");
            sink.blank();
            for note in degraded {
                sink.line(&format!("  ~ {note}"));
            }
        }
        return Ok(());
    }
    sink.line("The following grants are still needed:");
    sink.blank();
    for item in outstanding {
        sink.line(&format!("  • {item}"));
    }
    sink.blank();
    sink.line("Each grant shows a system dialog once. It cannot be accepted automatically.");
    Ok(())
}

/// A session that has just been created.
///
/// `unwatchable` is why there is no window to look at, on the machines where
/// there cannot be one. Sessions are visible by default, so a headless one is
/// now the surprising case and has to account for itself.
pub fn render_session_started(
    sink: &mut Sink<'_>,
    session: &AgentSession,
    unwatchable: Option<&str>,
) -> Rendered {
    if sink.is_json() {
        let mut value = serde_json::to_value(session.redacted()).unwrap_or_else(|_| json!({}));
        if let (Some(reason), Some(object)) = (unwatchable, value.as_object_mut()) {
            object.insert("unwatchable".to_owned(), json!(reason));
        }
        sink.value(&value);
        return Ok(());
    }
    sink.line(&format!(
        "Started an agent display on {} ({}x{}).",
        session.display, session.width, session.height
    ));
    sink.blank();
    sink.line("Every command now addresses it instead of your screen. It has its own");
    sink.line("pointer, its own keyboard focus and its own accessibility tree, so an");
    sink.line("agent working there cannot take the pointer from you, type into your");
    sink.line("window, or photograph it.");
    sink.blank();

    if session.visible {
        sink.line("It is in the window titled \"desktop-driver\" on your desktop — watch it");
        sink.line("there, and click into it if you want to take over. Watching changes");
        sink.line("nothing about the isolation.");
    } else if let Some(reason) = unwatchable {
        sink.line(&format!("You cannot watch it: {reason}."));
        sink.line("`desktop screenshot` is how to see what the agent sees.");
    } else {
        sink.line("It is headless, as you asked — `desktop screenshot` is how to see it.");
    }

    sink.blank();
    sink.line("  desktop session run firefox      launch something onto it");
    sink.line("  desktop screenshot               see it");
    sink.line("  desktop --host screenshot        see your own screen instead");
    sink.line("  desktop session stop             end it");
    Ok(())
}

/// The running session, or the absence of one.
///
/// Where the platform cannot host one at all, the advice is omitted rather than
/// printed: pointing someone at a command whose only possible answer is a
/// refusal is worse than saying nothing.
pub fn render_session_status(
    sink: &mut Sink<'_>,
    session: Option<&AgentSession>,
    supported: bool,
) -> Rendered {
    if sink.is_json() {
        let value = match session {
            Some(session) => json!({
                "running": true,
                "supported": supported,
                "session": serde_json::to_value(session.redacted()).unwrap_or_else(|_| json!({})),
            }),
            None => json!({ "running": false, "supported": supported }),
        };
        sink.value(&value);
        return Ok(());
    }

    let Some(session) = session else {
        sink.line("No agent display is running.");
        sink.blank();
        sink.line("Commands address the desktop you are looking at, which means the agent");
        sink.line("shares your pointer, your keyboard focus and your screen.");
        sink.blank();
        if supported {
            sink.line("  desktop session start            give it one of its own");
        } else {
            sink.line("This platform cannot give the agent a display of its own: there is one");
            sink.line("window server per login session and no supported way to create another.");
            sink.line("Use --no-steal-focus, which keeps the agent on element-addressed work");
            sink.line("and off your pointer and keyboard.");
        }
        return Ok(());
    };

    sink.line(&format!(
        "Agent display:  {} ({}x{}){}",
        session.display,
        session.width,
        session.height,
        if session.visible {
            " — visible on your desktop"
        } else {
            " — headless, see it with `desktop screenshot`"
        }
    ));
    sink.line(&format!("Accessibility:  {}", session.a11y_address));
    sink.line(&format!("Session bus:    {}", session.dbus_address));
    sink.blank();
    sink.line("Processes:");
    sink.blank();
    for process in &session.processes {
        sink.line(&format!("  {:<20} pid {}", process.name, process.pid));
    }
    Ok(())
}

pub fn render_session_stopped(sink: &mut Sink<'_>, session: Option<&AgentSession>) -> Rendered {
    if sink.is_json() {
        sink.value(&json!({
            "stopped": session.is_some(),
            "display": session.map(|session| session.display.clone()),
        }));
        return Ok(());
    }
    match session {
        Some(session) => {
            sink.line(&format!(
                "Stopped the agent display on {} and everything running on it.",
                session.display
            ));
            sink.blank();
            sink.line("Commands now address your own desktop again.");
        }
        None => sink.line("No agent display was running."),
    }
    Ok(())
}

pub fn render_session_launched(
    sink: &mut Sink<'_>,
    session: &AgentSession,
    program: &str,
    pid: u32,
) -> Rendered {
    if sink.is_json() {
        sink.value(&json!({
            "launched": program,
            "pid": pid,
            "display": session.display,
        }));
        return Ok(());
    }
    sink.line(&format!(
        "Launched {program} (pid {pid}) on {}.",
        session.display
    ));
    sink.blank();
    sink.line("It may take a moment to appear. `desktop apps` lists what is running there.");
    Ok(())
}

/// The exports that put any other command on the agent's display.
///
/// Worth having as its own command because `desktop session run` only covers
/// programs; this covers `xdotool`, a shell, or anything else a person reaches
/// for while working out what an application is doing.
///
/// The `unset` lines come first, and they are not optional: a shell that
/// exports `DISPLAY` while `WAYLAND_DISPLAY` is still set sends GTK and Qt
/// applications to the user's compositor, not to the agent's screen.
pub fn render_session_env(sink: &mut Sink<'_>, session: &AgentSession) -> Rendered {
    if sink.is_json() {
        let set: serde_json::Map<String, serde_json::Value> = session
            .environment()
            .into_iter()
            .map(|(key, value)| (key, json!(value)))
            .collect();
        sink.value(&json!({
            "set": set,
            "unset": AgentSession::removed_environment(),
        }));
        return Ok(());
    }
    for key in AgentSession::removed_environment() {
        sink.line(&format!("unset {key}"));
    }
    for (key, value) in session.environment() {
        sink.line(&format!("export {key}={value}"));
    }
    Ok(())
}

pub fn render_apps(sink: &mut Sink<'_>, apps: &[Application]) -> Rendered {
    if sink.is_json() {
        sink.value(&json!({ "apps": apps }));
        return Ok(());
    }
    if apps.is_empty() {
        sink.line("(no applications with accessibility support found)");
        sink.line("Run `desktop doctor` to find out why.");
        return Ok(());
    }
    for app in apps {
        let marker = if app.active { "*" } else { " " };
        sink.line(&format!(
            "{marker} {:>7}  {:<32} {} window(s)",
            app.pid.to_string(),
            app.name,
            app.window_count
        ));
    }
    Ok(())
}

/// Renders the window list.
///
/// A window with no bounds prints as absent rather than as zero: under Wayland
/// no client can learn its own position.
pub fn render_windows(sink: &mut Sink<'_>, windows: &[Window]) -> Rendered {
    if sink.is_json() {
        sink.value(&json!({ "windows": windows }));
        return Ok(());
    }
    if windows.is_empty() {
        sink.line("(no windows found)");
        return Ok(());
    }
    for window in windows {
        let focus = if window.focused { "*" } else { " " };
        let geometry = window.bounds.map_or_else(
            || "position unavailable".to_owned(),
            |b| format!("{}x{} at {},{}", b.width, b.height, b.x, b.y),
        );
        sink.line(&format!(
            "{focus} [{}] {:<40} {:<24} {}",
            window.id,
            window.title.as_deref().unwrap_or("(untitled)"),
            window.app.name,
            geometry
        ));
    }
    Ok(())
}

pub fn render_snapshot(sink: &mut Sink<'_>, snapshot: &Snapshot) -> Rendered {
    if sink.is_json() {
        sink.value(&serde_json::to_value(snapshot).unwrap_or_else(|_| json!({})));
        return Ok(());
    }
    let rendered = snapshot.render();
    let _ = write!(sink.out, "{rendered}");
    Ok(())
}

pub fn render_elements(sink: &mut Sink<'_>, elements: &[Element]) -> Rendered {
    if sink.is_json() {
        sink.value(&json!({ "elements": elements }));
        return Ok(());
    }
    if elements.is_empty() {
        sink.line("(no matching elements)");
        return Ok(());
    }
    for element in elements {
        let mut line = format!("[{}] {}", element.id, element.role.as_str());
        if let Some(name) = &element.name {
            line.push_str(&format!(" {name:?}"));
        }
        if element.redacted {
            line.push_str(" <redacted>");
        } else if let Some(value) = &element.value {
            line.push_str(&format!(" = {value:?}"));
        }
        sink.line(&line);
    }
    Ok(())
}

/// The raw tree, for `desktop inspect`.
pub fn render_tree(
    sink: &mut Sink<'_>,
    app: &desktop_core::models::app::AppKey,
    root: &RawNode,
) -> Rendered {
    if sink.is_json() {
        sink.value(&json!({ "app": app, "tree": tree_json(root) }));
        return Ok(());
    }
    sink.line(&format!("Application: {}", app.name));
    sink.blank();
    write_tree(sink, root, 0);
    Ok(())
}

fn write_tree(sink: &mut Sink<'_>, node: &RawNode, depth: usize) {
    let indent = "  ".repeat(depth);
    let mut line = format!("{indent}{}", node.role.as_str());
    if let Some(name) = &node.name {
        line.push_str(&format!(" {name:?}"));
    }
    if node.is_secure() {
        line.push_str(" <redacted>");
    } else if let Some(value) = &node.value {
        line.push_str(&format!(" = {value:?}"));
    }
    if let Some(bounds) = node.bounds {
        line.push_str(&format!(
            " [{},{} {}x{}]",
            bounds.x, bounds.y, bounds.width, bounds.height
        ));
    }
    if !node.actions.is_empty() {
        let names: Vec<&str> = node.actions.iter().map(|a| a.as_str()).collect();
        line.push_str(&format!(" ({})", names.join(",")));
    }
    sink.line(&line);
    for child in &node.children {
        write_tree(sink, child, depth + 1);
    }
}

/// Serializes the raw tree for `desktop inspect --json`.
///
/// Redaction applies here too: `inspect` is a debugging view, not a bypass.
fn tree_json(node: &RawNode) -> serde_json::Value {
    json!({
        "role": node.role.as_str(),
        "name": node.name,
        "description": node.description,
        "value": if node.is_secure() { None } else { node.value.clone() },
        "redacted": node.is_secure(),
        "enabled": node.states.enabled,
        "focused": node.states.focused,
        "selected": node.states.selected,
        "bounds": node.bounds,
        "actions": node.actions,
        "children": node.children.iter().map(tree_json).collect::<Vec<_>>(),
    })
}

pub fn render_screenshot(sink: &mut Sink<'_>, metadata: &ScreenshotMetadata) -> Rendered {
    if sink.is_json() {
        sink.value(&serde_json::to_value(metadata).unwrap_or_else(|_| json!({})));
        return Ok(());
    }
    sink.line(&format!(
        "{} ({}x{} @ {}x)",
        metadata.path, metadata.width, metadata.height, metadata.scale_factor
    ));
    Ok(())
}

pub fn render_activation(
    sink: &mut Sink<'_>,
    element: ElementId,
    activation: Activation,
) -> Rendered {
    let (via, detail) = match activation {
        Activation::Action(action) => ("action", action.as_str().to_owned()),
        Activation::Pointer(point) => ("pointer", format!("{},{}", point.x, point.y)),
    };
    if sink.is_json() {
        sink.value(&json!({
            "ok": true,
            "element": element,
            "via": via,
            "detail": detail,
        }));
        return Ok(());
    }
    sink.line(&format!("clicked [{element}] via {via} ({detail})"));
    Ok(())
}

pub fn render_ok(sink: &mut Sink<'_>, verb: &str, detail: &str) -> Rendered {
    if sink.is_json() {
        sink.value(&json!({ "ok": true, "action": verb, "detail": detail }));
        return Ok(());
    }
    sink.line(&format!("{verb} {detail}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_core::models::{
        backend::{Backend, DesktopEnvironment, DisplayServer, Platform},
        capability::{Capability, CapabilitySet, UnsupportedReason},
        dependency::Need,
        ids::ProcessId,
    };

    fn capture(json: bool, render: impl FnOnce(&mut Sink<'_>)) -> String {
        let mut buffer = Vec::new();
        let mut sink = Sink::new(&mut buffer, json);
        render(&mut sink);
        String::from_utf8(buffer).expect("utf-8")
    }

    fn info() -> BackendInfo {
        BackendInfo {
            platform: Platform::Linux,
            display_server: DisplayServer::Wayland,
            desktop_environment: DesktopEnvironment::Gnome,
            accessibility: Backend::AtSpi,
            windows: Backend::AtSpi,
            screenshot: Backend::PortalScreenCast,
            input: Backend::RemoteDesktopPortal,
        }
    }

    #[test]
    fn info_json_matches_the_shape_promised_in_the_documentation() {
        let text = capture(true, |sink| {
            render_info(sink, &info()).expect("renders");
        });
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["platform"], "linux");
        assert_eq!(parsed["display_server"], "wayland");
        assert_eq!(parsed["desktop_environment"], "gnome");
        assert_eq!(parsed["accessibility"], "at-spi");
        assert_eq!(parsed["screenshot"], "portal-screencast");
        assert_eq!(parsed["input"], "remote-desktop-portal");
    }

    #[test]
    fn capability_glyphs_distinguish_supported_degraded_and_unsupported() {
        let capabilities = CapabilitySet::new()
            .with(Capability::Accessibility, CapabilityState::Supported)
            .with(
                Capability::WindowScreenshots,
                CapabilityState::degraded("needs the portal picker"),
            )
            .with(
                Capability::Mouse,
                CapabilityState::unsupported(UnsupportedReason::NoBackendMechanism),
            );

        let text = capture(false, |sink| {
            render_capabilities(sink, &info(), &capabilities).expect("renders");
        });
        assert!(text.contains("✓ accessibility"), "got {text}");
        assert!(text.contains("~ window_screenshots"), "got {text}");
        assert!(text.contains("✗ mouse"), "got {text}");
        // The caveat itself must appear, not just the glyph.
        assert!(text.contains("needs the portal picker"), "got {text}");
    }

    #[test]
    fn a_window_with_no_known_position_says_so_rather_than_printing_zeros() {
        let window = Window {
            id: desktop_core::models::ids::WindowId::new(0),
            title: Some("main.rs".to_owned()),
            app: desktop_core::models::app::AppKey::new(ProcessId::new(1), "Code"),
            bounds: None,
            focused: true,
            minimized: false,
            index: 0,
        };
        let text = capture(false, |sink| {
            render_windows(sink, &[window]).expect("renders");
        });
        assert!(text.contains("position unavailable"), "got {text}");
        assert!(
            !text.contains("0,0"),
            "zeros would read as a real position: {text}"
        );
    }

    #[test]
    fn an_unsupported_capability_error_explains_the_environment_in_human_mode() {
        let error = DesktopError::UnsupportedCapability {
            capability: Capability::Mouse,
            backend: Backend::None,
            platform: Platform::Linux,
            display_server: DisplayServer::Wayland,
            desktop_environment: DesktopEnvironment::Kde,
        };
        let text = capture(false, |sink| render_error(sink, &error));
        assert!(text.contains("mouse"), "got {text}");
        assert!(text.contains("wayland"), "got {text}");
        assert!(text.contains("kde"), "got {text}");
        assert!(text.contains("desktop capabilities"), "got {text}");
    }

    #[test]
    fn a_permission_error_prints_its_remedy_in_human_mode() {
        let error = DesktopError::PermissionRequired {
            permission: desktop_core::errors::Permission::Accessibility,
            platform: Platform::Macos,
            remedy: "System Settings → Privacy & Security → Accessibility".to_owned(),
        };
        let text = capture(false, |sink| render_error(sink, &error));
        assert!(text.contains("System Settings"), "got {text}");
    }

    #[test]
    fn json_error_output_is_a_single_object_with_no_human_preamble() {
        let error = DesktopError::NoSnapshot;
        let text = capture(true, |sink| render_error(sink, &error));
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["error"], "no_snapshot");
    }

    #[test]
    fn inspect_redacts_secure_values_too_so_it_is_not_a_bypass() {
        let mut states = desktop_core::models::element::States::usable();
        states.protected = true;
        let root = RawNode::new(desktop_core::models::role::Role::TextBox)
            .with_name("Token")
            .with_value("s3cret")
            .with_states(states);

        let app = desktop_core::models::app::AppKey::new(ProcessId::new(1), "Fixture");
        let human = capture(false, |sink| {
            render_tree(sink, &app, &root).expect("renders");
        });
        let json = capture(true, |sink| {
            render_tree(sink, &app, &root).expect("renders");
        });
        assert!(!human.contains("s3cret"), "leaked in {human}");
        assert!(!json.contains("s3cret"), "leaked in {json}");
        assert!(human.contains("<redacted>"), "got {human}");
    }

    #[test]
    fn an_empty_application_list_points_at_the_diagnostic_command() {
        let text = capture(false, |sink| {
            render_apps(sink, &[]).expect("renders");
        });
        assert!(text.contains("desktop doctor"), "got {text}");
    }

    fn dependency(name: &str, present: bool, need: Need) -> SystemDependency {
        SystemDependency::new(name, "something useful", need, present)
            .with_package(Some(format!("{name}-package")))
    }

    #[test]
    fn doctor_lists_dependencies_even_when_nothing_is_wrong() {
        // "What does this tool need?" should be answerable before the first
        // confusing failure, not only after it.
        let dependencies = vec![dependency("at-spi2-core", true, Need::Required)];
        let text = capture(false, |sink| {
            render_doctor(sink, &info(), &[], &dependencies, None).expect("renders");
        });
        assert!(text.contains("System dependencies"), "got {text}");
        assert!(text.contains("✓ at-spi2-core"), "got {text}");
    }

    #[test]
    fn doctor_marks_missing_dependencies_and_offers_one_install_command() {
        let dependencies = vec![
            dependency("at-spi2-core", true, Need::Required),
            dependency("Xvfb", false, Need::Optional),
        ];
        let text = capture(false, |sink| {
            render_doctor(
                sink,
                &info(),
                &[],
                &dependencies,
                Some("sudo dnf install xorg-x11-server-Xvfb"),
            )
            .expect("renders");
        });
        assert!(text.contains("✗ Xvfb"), "got {text}");
        assert!(text.contains("sudo dnf install"), "got {text}");
    }

    #[test]
    fn doctor_json_carries_the_dependency_table_for_an_agent_to_act_on() {
        let dependencies = vec![dependency("Xvfb", false, Need::Optional)];
        let text = capture(true, |sink| {
            render_doctor(
                sink,
                &info(),
                &[],
                &dependencies,
                Some("sudo dnf install x"),
            )
            .expect("renders");
        });
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["dependencies"][0]["name"], "Xvfb");
        assert_eq!(parsed["dependencies"][0]["present"], false);
        assert_eq!(parsed["dependencies"][0]["need"], "optional");
        assert_eq!(parsed["install_command"], "sudo dnf install x");
    }

    #[test]
    fn doctor_orders_errors_before_warnings_before_information() {
        let diagnostics = vec![
            Diagnostic::info("third"),
            Diagnostic::warning("second", "fix"),
            Diagnostic::error("first", "fix"),
        ];
        let text = capture(false, |sink| {
            render_doctor(sink, &info(), &diagnostics, &[], None).expect("renders");
        });
        let first = text.find("first").expect("present");
        let second = text.find("second").expect("present");
        let third = text.find("third").expect("present");
        assert!(first < second && second < third, "wrong order in {text}");
    }
}
