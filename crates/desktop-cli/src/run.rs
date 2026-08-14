//! Dispatch.
//!
//! [`run`] takes an already-built [`Driver`] and writes to a supplied sink, so
//! the whole CLI is exercisable in-process against fake ports — no subprocess,
//! no real desktop, and no moving the actual mouse.

use std::io::Write;

use desktop_core::{
    Driver,
    agent::{SessionHost, Visibility},
    errors::{DesktopError, ExitCategory, Result},
    models::{
        capability::CapabilityState,
        chord::Chord,
        geometry::{Point, ScrollDelta},
        ids::{ElementId, WindowId},
        image::ScreenshotMetadata,
        role::Role,
        selector::{ActivationMode, ClickTarget, Selector, Target},
        snapshot::WalkBudget,
    },
    ports::{CaptureTarget, MouseButton, Severity},
};

use crate::{
    cli::{
        BudgetArgs, ButtonArg, Cli, ClickArgs, Command, SelectorArgs, SessionCommand, TargetArgs,
        ViaArg,
    },
    output::{self, Sink},
};

/// Runs one command. Returns the exit category rather than exiting, so the
/// caller decides what to do with it.
///
/// Output is labelled with the agent display whenever one is in use, because
/// acting on the wrong screen must never be silent. `desktop session` commands
/// are the exception: they are *about* the session, so labelling them would be
/// noise.
pub fn run(
    cli: &Cli,
    driver: &Driver,
    sessions: &dyn SessionHost,
    out: &mut dyn Write,
) -> ExitCategory {
    let display = match &cli.command {
        Command::Session(_) => None,
        _ if cli.host => None,
        _ => sessions.status().map(|session| session.display),
    };

    let mut sink = Sink::new(out, cli.json).on_agent_display(display);
    if !sink.is_json()
        && let Some(display) = sink.agent_display().map(str::to_owned)
    {
        sink.line(&format!("[agent display {display}]"));
        sink.blank();
    }

    match dispatch(cli, driver, sessions, &mut sink) {
        Ok(()) => ExitCategory::Success,
        Err(error) => {
            output::render_error(&mut sink, &error);
            error.exit_category()
        }
    }
}

fn dispatch(
    cli: &Cli,
    driver: &Driver,
    sessions: &dyn SessionHost,
    sink: &mut Sink<'_>,
) -> Result<()> {
    match &cli.command {
        Command::Session(command) => session(command, sessions, sink),
        Command::Info => output::render_info(sink, &driver.info()),
        Command::Capabilities => {
            output::render_capabilities(sink, &driver.info(), &driver.capabilities())
        }
        Command::Doctor => output::render_doctor(
            sink,
            &driver.info(),
            &driver.diagnostics(),
            &driver.dependencies(),
            driver.install_command().as_deref(),
        ),
        Command::Setup => setup(driver, sink),
        Command::Apps => output::render_apps(sink, &driver.list_apps()?),
        Command::Windows(args) => {
            let apps = driver.list_apps()?;
            let key = args.app.as_deref().and_then(|needle| {
                apps.iter()
                    .find(|app| app.key().matches(needle))
                    .map(|app| app.key())
            });
            if args.app.is_some() && key.is_none() {
                return Err(DesktopError::TargetNotFound {
                    target: format!("application {:?}", args.app.as_deref().unwrap_or_default()),
                });
            }
            output::render_windows(sink, &driver.list_windows(key.as_ref())?)
        }
        Command::Inspect(args) => {
            let (app, root) = driver.inspect(&target_of(&args.target), budget_of(&args.budget))?;
            output::render_tree(sink, &app, &root)
        }
        Command::Snapshot(args) => {
            let snapshot =
                driver.snapshot(&target_of(&args.target), budget_of(&args.budget), args.all)?;
            output::render_snapshot(sink, &snapshot)
        }
        Command::Screenshot(args) => {
            let target = capture_target_of(&args.target);
            let image = driver.screenshot(&target)?;
            let path = crate::png::write(&image, args.output.as_deref())?;
            output::render_screenshot(
                sink,
                &ScreenshotMetadata {
                    path: path.display().to_string(),
                    width: image.width,
                    height: image.height,
                    scale_factor: image.scale_factor.get(),
                    space: image.space,
                },
            )
        }
        Command::Focus(args) => {
            driver.focus(&target_of(args))?;
            output::render_ok(sink, "focused", &target_of(args).describe())
        }
        Command::Move(args) => {
            let space = driver.coordinate_space_for_point()?;
            driver.move_mouse(Point::new(args.x, args.y), &space)?;
            output::render_ok(sink, "moved", &format!("{},{}", args.x, args.y))
        }
        Command::Click(args) => click(driver, args, sink),
        Command::Type(args) => match args.element {
            Some(id) => {
                driver.type_into_element(ElementId::new(id), &args.text)?;
                output::render_ok(sink, "typed into", &format!("[{id}] {:?}", args.text))
            }
            None => {
                driver.type_text(&args.text)?;
                output::render_ok(sink, "typed", &args.text)
            }
        },
        Command::Key(args) => {
            let chord = Chord::parse(&args.shortcut).map_err(|error| {
                DesktopError::invalid_argument(format!(
                    "cannot parse shortcut {:?}: {error}",
                    args.shortcut
                ))
            })?;
            driver.key(&chord)?;
            output::render_ok(sink, "sent", &chord.to_string())
        }
        Command::Scroll(args) => {
            let delta = ScrollDelta::new(args.x, args.y);
            if delta.is_zero() {
                return Err(DesktopError::invalid_argument(
                    "scroll needs a non-zero --x or --y",
                ));
            }
            let space = driver.coordinate_space_for_point()?;
            driver.scroll(delta, &space)?;
            output::render_ok(sink, "scrolled", &format!("{},{}", args.x, args.y))
        }
        Command::Find(args) => {
            let selector = selector_of(args)?;
            output::render_elements(sink, &driver.find_all(&selector)?)
        }
        Command::Wait(args) => {
            let selector = selector_of(&args.selector)?;
            let found = wait_for(
                driver,
                &selector,
                &target_of(&args.target),
                args.timeout,
                args.interval,
            )?;
            output::render_elements(sink, &[found])
        }
    }
}

/// Dispatches a `desktop session` subcommand.
///
/// A session nobody can watch is the exception rather than the norm, so it has
/// to explain itself rather than quietly being one.
fn session(
    command: &SessionCommand,
    sessions: &dyn SessionHost,
    sink: &mut Sink<'_>,
) -> Result<()> {
    match command {
        SessionCommand::Start(args) => {
            let (width, height) =
                crate::cli::parse_size(&args.size).map_err(DesktopError::invalid_argument)?;
            let started = sessions.start(desktop_core::agent::StartOptions {
                width,
                height,
                display: args.display,
                visibility: if args.headless {
                    Visibility::Headless
                } else if args.visible {
                    Visibility::Visible
                } else {
                    Visibility::Auto
                },
                share_home: args.share_home,
            })?;
            let unwatchable = if started.visible {
                None
            } else {
                sessions.unwatchable()
            };
            output::render_session_started(sink, &started, unwatchable)
        }
        SessionCommand::Status => {
            output::render_session_status(sink, sessions.status().as_ref(), sessions.supported())
        }
        SessionCommand::Stop => {
            let stopped = sessions.stop()?;
            output::render_session_stopped(sink, stopped.as_ref())
        }
        SessionCommand::Run(args) => {
            let pid = sessions.launch(&args.program, &args.args)?;
            let session = sessions.status().ok_or_else(|| {
                DesktopError::internal("the session vanished between launching and reporting")
            })?;
            output::render_session_launched(sink, &session, &args.program, pid)
        }
        SessionCommand::Env => {
            let session = sessions.status().ok_or_else(|| {
                DesktopError::invalid_argument(
                    "no agent display is running; start one with `desktop session start`",
                )
            })?;
            output::render_session_env(sink, &session)
        }
    }
}

/// Polls until the selector matches or the deadline passes.
///
/// Polling rather than subscribing to accessibility events: an event stream
/// would need a long-lived process, and a one-shot CLI that re-snapshots is
/// both simpler and immune to missing an event that fired before we subscribed.
///
/// Re-snapshotting also refreshes the stored element ids, so a `wait` followed
/// by `click --element N` refers to the tree that was just observed.
fn wait_for(
    driver: &Driver,
    selector: &Selector,
    target: &Target,
    timeout_ms: u64,
    interval_ms: u64,
) -> Result<desktop_core::models::element::Element> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let interval = std::time::Duration::from_millis(interval_ms.max(10));

    loop {
        if driver
            .snapshot(target, WalkBudget::default(), false)
            .is_ok()
            && let Ok(found) = driver.find(selector)
        {
            return Ok(found);
        }

        if std::time::Instant::now() >= deadline {
            return Err(DesktopError::Timeout {
                waited_ms: timeout_ms,
                condition: selector.describe(),
            });
        }
        std::thread::sleep(interval);
    }
}

/// Resolves what to click, and clicks it.
///
/// An over-specified click is refused rather than resolved by precedence: an
/// agent that passes both an element and a coordinate meant one of them, and
/// guessing would click the wrong thing.
fn click(driver: &Driver, args: &ClickArgs, sink: &mut Sink<'_>) -> Result<()> {
    let button = match args.button {
        ButtonArg::Left => MouseButton::Left,
        ButtonArg::Right => MouseButton::Right,
        ButtonArg::Middle => MouseButton::Middle,
    };
    let mode = match args.via {
        ViaArg::Auto => ActivationMode::Auto,
        ViaArg::Action => ActivationMode::Action,
        ViaArg::Pointer => ActivationMode::Pointer,
    };

    let target = click_target_of(args)?;
    match target {
        ClickTarget::Point { x, y } => {
            let space = driver.coordinate_space_for_point()?;
            driver.click_point(Point::new(x, y), &space, button, args.count)?;
            output::render_ok(sink, "clicked", &format!("{x},{y}"))
        }
        ClickTarget::Element(id) => {
            let activation = driver.click_element(id, mode, button, args.count)?;
            output::render_activation(sink, id, activation)
        }
        ClickTarget::Selector(selector) => {
            let element = driver.find(&selector)?;
            let activation = driver.click_element(element.id, mode, button, args.count)?;
            output::render_activation(sink, element.id, activation)
        }
    }
}

fn click_target_of(args: &ClickArgs) -> Result<ClickTarget> {
    let has_selector = !selector_args_empty(&args.selector);
    let has_point = args.x.is_some() && args.y.is_some();
    let has_element = args.element.is_some();

    let count = usize::from(has_selector) + usize::from(has_point) + usize::from(has_element);
    match count {
        0 => Err(DesktopError::invalid_argument(
            "click needs --element, a selector (--role/--name/--text), or --x and --y",
        )),
        1 => {
            if let Some(id) = args.element {
                Ok(ClickTarget::Element(ElementId::new(id)))
            } else if has_point {
                Ok(ClickTarget::Point {
                    x: args.x.unwrap_or_default(),
                    y: args.y.unwrap_or_default(),
                })
            } else {
                Ok(ClickTarget::Selector(selector_of(&args.selector)?))
            }
        }
        _ => Err(DesktopError::invalid_argument(
            "click takes exactly one of --element, a selector, or --x/--y",
        )),
    }
}

const fn selector_args_empty(args: &SelectorArgs) -> bool {
    args.role.is_none() && args.name.is_none() && args.text.is_none()
}

fn selector_of(args: &SelectorArgs) -> Result<Selector> {
    if selector_args_empty(args) {
        return Err(DesktopError::invalid_argument(
            "a selector needs at least one of --role, --name or --text",
        ));
    }
    Ok(Selector {
        role: args.role.as_deref().map(Role::parse),
        name: args.name.clone(),
        text: args.text.clone(),
    })
}

fn target_of(args: &TargetArgs) -> Target {
    match (&args.app, args.window) {
        (Some(app), _) => Target::App(app.clone()),
        (None, Some(id)) => Target::Window(WindowId::new(id)),
        (None, None) => Target::Focused,
    }
}

/// What to capture, from the target flags.
///
/// `--app` implies a window capture as much as `--window` does; the adapter
/// resolves which window that is.
fn capture_target_of(args: &TargetArgs) -> CaptureTarget {
    match args.window {
        Some(id) => CaptureTarget::Window(WindowId::new(id)),
        None if args.app.is_some() => CaptureTarget::Window(WindowId::new(0)),
        None => CaptureTarget::Screen,
    }
}

fn budget_of(args: &BudgetArgs) -> WalkBudget {
    let default = WalkBudget::default();
    WalkBudget {
        max_nodes: args.max_nodes.unwrap_or(default.max_nodes),
        max_depth: args.max_depth.unwrap_or(default.max_depth),
    }
}

/// Walks the user through the one-time grant Wayland needs.
fn setup(driver: &Driver, sink: &mut Sink<'_>) -> Result<()> {
    let permissions = driver.request_permissions();
    let capabilities = driver.capabilities();

    let pending: Vec<_> = permissions.iter().filter(|state| !state.granted).collect();
    if pending.is_empty() {
        let degraded: Vec<String> = capabilities
            .iter()
            .filter_map(|(capability, state)| match state {
                CapabilityState::Degraded { note } => {
                    Some(format!("{}: {note}", capability.as_str()))
                }
                _ => None,
            })
            .collect();
        return output::render_setup(sink, true, &[], &degraded);
    }

    let outstanding: Vec<String> = pending
        .iter()
        .map(|state| {
            format!(
                "{}: {}",
                state.permission.as_str(),
                state.remedy.as_deref().unwrap_or("not granted")
            )
        })
        .collect();
    output::render_setup(sink, false, &outstanding, &[])
}

/// Severity ordering used when printing diagnostics; errors first.
pub(crate) fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser as _;
    use desktop_core::{
        SessionStore,
        models::capability::Capability,
        policy::Policy,
        testing::{FakePorts, FakeSessions, RecordedInput},
    };

    fn harness(tag: &str, ports: FakePorts) -> (Driver, RecordedInput) {
        let recorded = ports.recorded();
        let store = SessionStore::new(
            std::env::temp_dir()
                .join(format!("desktop-cli-test-{}-{tag}", std::process::id()))
                .join("snapshot.json"),
        );
        let _ = store.clear();
        (
            Driver::new(ports.into_ports(), Policy::permissive(), store),
            recorded,
        )
    }

    fn invoke(driver: &Driver, argv: &[&str]) -> (ExitCategory, String) {
        invoke_with(driver, &FakeSessions::idle(), argv)
    }

    fn invoke_with(
        driver: &Driver,
        sessions: &dyn SessionHost,
        argv: &[&str],
    ) -> (ExitCategory, String) {
        let cli = Cli::try_parse_from(argv).expect("parses");
        let mut buffer = Vec::new();
        let category = run(&cli, driver, sessions, &mut buffer);
        (category, String::from_utf8(buffer).expect("utf-8 output"))
    }

    #[test]
    fn apps_renders_a_human_readable_list_and_exits_successfully() {
        let (driver, _) = harness("apps", FakePorts::new().with_apps(&["Firefox", "Code"]));
        let (category, text) = invoke(&driver, &["desktop", "apps"]);
        assert_eq!(category, ExitCategory::Success);
        assert!(text.contains("Firefox"), "got {text}");
        assert!(text.contains("Code"), "got {text}");
    }

    #[test]
    fn apps_json_is_a_parseable_array_of_objects() {
        let (driver, _) = harness("apps-json", FakePorts::new().with_apps(&["Firefox"]));
        let (category, text) = invoke(&driver, &["desktop", "apps", "--json"]);
        assert_eq!(category, ExitCategory::Success);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["apps"][0]["name"], "Firefox");
    }

    #[test]
    fn an_unsupported_capability_produces_the_documented_error_json_and_exit_code() {
        let (driver, recorded) = harness(
            "unsupported",
            FakePorts::new().without_capability(Capability::Mouse),
        );
        let (category, text) = invoke(
            &driver,
            &["desktop", "click", "--x", "800", "--y", "400", "--json"],
        );
        assert_eq!(category, ExitCategory::SetupOrConfigurationFailure);
        assert_eq!(category.status(), 2);

        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["error"], "unsupported_capability");
        assert_eq!(parsed["capability"], "mouse");
        assert!(recorded.is_empty(), "the backend was reached anyway");
    }

    #[test]
    fn snapshot_then_click_by_element_works_across_two_invocations() {
        // The whole point of persisting snapshots: these are separate commands.
        let (driver, recorded) = harness("two-step", FakePorts::new().with_button("Save"));
        let (first, _) = invoke(&driver, &["desktop", "snapshot"]);
        assert_eq!(first, ExitCategory::Success);

        let (second, text) = invoke(&driver, &["desktop", "click", "--element", "1"]);
        assert_eq!(second, ExitCategory::Success, "got {text}");
        // Preferred the accessibility action, so no pointer was moved.
        assert!(recorded.clicks().is_empty());
        let _ = driver.store().clear();
    }

    #[test]
    fn clicking_by_selector_resolves_through_the_snapshot() {
        let (driver, _) = harness(
            "by-selector",
            FakePorts::new().with_buttons(&["Save", "Run"]),
        );
        invoke(&driver, &["desktop", "snapshot"]);
        let (category, text) = invoke(
            &driver,
            &["desktop", "click", "--role", "button", "--name", "Run"],
        );
        assert_eq!(category, ExitCategory::Success, "got {text}");
        let _ = driver.store().clear();
    }

    #[test]
    fn an_ambiguous_selector_is_refused_with_the_candidate_ids() {
        let (driver, _) = harness(
            "ambiguous",
            FakePorts::new().with_buttons(&["Save", "Save"]),
        );
        invoke(&driver, &["desktop", "snapshot"]);
        let (category, text) = invoke(&driver, &["desktop", "click", "--name", "Save", "--json"]);
        assert_eq!(category, ExitCategory::TargetFailure);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["error"], "ambiguous_selector");
        assert_eq!(parsed["matches"], 2);
        assert!(parsed["candidates"].is_array());
        let _ = driver.store().clear();
    }

    #[test]
    fn an_over_specified_click_is_refused_rather_than_guessed() {
        let (driver, _) = harness("over-specified", FakePorts::new().with_button("Save"));
        let (category, text) = invoke(
            &driver,
            &[
                "desktop",
                "click",
                "--element",
                "1",
                "--x",
                "5",
                "--y",
                "5",
                "--json",
            ],
        );
        assert_eq!(category, ExitCategory::TargetFailure);
        assert!(text.contains("exactly one"), "got {text}");
    }

    #[test]
    fn a_click_with_no_target_at_all_is_refused() {
        let (driver, _) = harness("no-target", FakePorts::new());
        let (category, text) = invoke(&driver, &["desktop", "click"]);
        assert_eq!(category, ExitCategory::TargetFailure);
        assert!(text.contains("--element"), "got {text}");
    }

    #[test]
    fn read_only_mode_refuses_typing_and_says_why() {
        let cli = Cli::try_parse_from(["desktop", "--read-only", "type", "hello", "--json"])
            .expect("parses");
        let ports = FakePorts::new();
        let recorded = ports.recorded();
        let store = SessionStore::new(
            std::env::temp_dir()
                .join(format!("desktop-cli-test-{}-ro", std::process::id()))
                .join("snapshot.json"),
        );
        let driver = Driver::new(ports.into_ports(), Policy::read_only(), store);

        let mut buffer = Vec::new();
        let category = run(&cli, &driver, &FakeSessions::idle(), &mut buffer);
        let text = String::from_utf8(buffer).expect("utf-8");

        assert_eq!(category, ExitCategory::PolicyDenied);
        assert_eq!(category.status(), 3);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["error"], "policy_denied");
        assert!(recorded.is_empty());
    }

    #[test]
    fn typing_reaches_the_backend_verbatim() {
        let (driver, recorded) = harness("typing", FakePorts::new());
        let (category, _) = invoke(&driver, &["desktop", "type", "Hello world"]);
        assert_eq!(category, ExitCategory::Success);
        assert_eq!(recorded.typed(), vec!["Hello world".to_owned()]);
    }

    #[test]
    fn a_malformed_shortcut_is_rejected_before_any_key_is_sent() {
        let (driver, recorded) = harness("bad-chord", FakePorts::new());
        let (category, text) = invoke(&driver, &["desktop", "key", "ctrl+"]);
        assert_eq!(category, ExitCategory::TargetFailure);
        assert!(text.contains("shortcut"), "got {text}");
        assert!(recorded.is_empty());
    }

    #[test]
    fn a_valid_shortcut_reaches_the_backend() {
        let (driver, recorded) = harness("chord", FakePorts::new());
        let (category, _) = invoke(&driver, &["desktop", "key", "ctrl+shift+p"]);
        assert_eq!(category, ExitCategory::Success);
        assert_eq!(recorded.keys().len(), 1);
    }

    #[test]
    fn a_zero_scroll_is_refused_because_it_would_do_nothing() {
        let (driver, recorded) = harness("zero-scroll", FakePorts::new());
        let (category, _) = invoke(&driver, &["desktop", "scroll"]);
        assert_eq!(category, ExitCategory::TargetFailure);
        assert!(recorded.is_empty());
    }

    #[test]
    fn scrolling_passes_the_signed_delta_through() {
        let (driver, recorded) = harness("scroll", FakePorts::new());
        invoke(&driver, &["desktop", "scroll", "--y", "-500"]);
        assert_eq!(recorded.scrolls(), vec![ScrollDelta::new(0, -500)]);
    }

    #[test]
    fn capabilities_json_reports_every_capability_with_a_state() {
        let (driver, _) = harness("caps", FakePorts::new());
        let (category, text) = invoke(&driver, &["desktop", "capabilities", "--json"]);
        assert_eq!(category, ExitCategory::Success);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        for capability in Capability::ALL {
            assert!(
                parsed["capabilities"][capability.as_str()].is_object(),
                "{} missing from {text}",
                capability.as_str()
            );
        }
    }

    #[test]
    fn info_json_names_the_platform_and_every_selected_backend() {
        let (driver, _) = harness("info", FakePorts::new());
        let (_, text) = invoke(&driver, &["desktop", "info", "--json"]);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        for key in [
            "platform",
            "display_server",
            "desktop_environment",
            "accessibility",
            "screenshot",
            "input",
        ] {
            assert!(parsed[key].is_string(), "{key} missing from {text}");
        }
    }

    #[test]
    fn a_password_value_never_appears_in_snapshot_output_in_either_format() {
        let (driver, _) = harness("secret", FakePorts::new().with_password_field("Password"));
        let (_, human) = invoke(&driver, &["desktop", "snapshot"]);
        let (_, json) = invoke(&driver, &["desktop", "snapshot", "--json"]);
        assert!(!human.contains("hunter2"), "secret leaked: {human}");
        assert!(!json.contains("hunter2"), "secret leaked: {json}");
        assert!(
            human.contains("redacted"),
            "expected a redaction marker in {human}"
        );
        let _ = driver.store().clear();
    }

    #[test]
    fn find_without_criteria_is_refused() {
        let (driver, _) = harness("find-empty", FakePorts::new());
        let (category, text) = invoke(&driver, &["desktop", "find"]);
        assert_eq!(category, ExitCategory::TargetFailure);
        assert!(text.contains("--role"), "got {text}");
    }

    #[test]
    fn waiting_for_something_that_never_appears_times_out_with_the_condition() {
        let (driver, _) = harness("wait", FakePorts::new().with_button("Save"));
        let (category, text) = invoke(
            &driver,
            &[
                "desktop",
                "wait",
                "--text",
                "never",
                "--timeout",
                "60",
                "--interval",
                "10",
                "--json",
            ],
        );
        assert_eq!(category, ExitCategory::Timeout);
        assert_eq!(category.status(), 7);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["error"], "timeout");
        assert!(
            parsed["condition"]
                .as_str()
                .unwrap_or_default()
                .contains("never"),
            "got {text}"
        );
        let _ = driver.store().clear();
    }

    #[test]
    fn waiting_for_something_already_present_returns_immediately() {
        let (driver, _) = harness("wait-hit", FakePorts::new().with_button("Save"));
        let (category, text) = invoke(
            &driver,
            &["desktop", "wait", "--name", "Save", "--timeout", "1000"],
        );
        assert_eq!(category, ExitCategory::Success, "got {text}");
        assert!(text.contains("Save"), "got {text}");
        let _ = driver.store().clear();
    }

    #[test]
    fn clicking_before_any_snapshot_reports_no_snapshot_not_a_crash() {
        let (driver, _) = harness("no-snap", FakePorts::new());
        let _ = driver.store().clear();
        let (category, text) = invoke(&driver, &["desktop", "click", "--element", "1", "--json"]);
        assert_eq!(category, ExitCategory::TargetFailure);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["error"], "no_snapshot");
    }

    #[test]
    fn every_json_error_path_emits_exactly_one_json_object() {
        // Agents parse this stream; a stray human line would break them.
        let (driver, _) = harness("json-purity", FakePorts::new());
        for argv in [
            vec!["desktop", "click", "--json"],
            vec!["desktop", "find", "--json"],
            vec!["desktop", "scroll", "--json"],
            vec!["desktop", "key", "nonsense!!", "--json"],
        ] {
            let (_, text) = invoke(&driver, &argv);
            let parsed: serde_json::Value = serde_json::from_str(&text)
                .unwrap_or_else(|error| panic!("{argv:?} produced invalid JSON: {error}\n{text}"));
            assert!(parsed["error"].is_string(), "{argv:?} produced {text}");
        }
    }

    #[test]
    fn with_no_agent_display_status_says_so_and_says_how_to_get_one() {
        let (driver, _) = harness("session-none", FakePorts::new());
        let (category, text) = invoke_with(
            &driver,
            &FakeSessions::idle(),
            &["desktop", "session", "status"],
        );
        assert_eq!(category, ExitCategory::Success);
        assert!(text.contains("No agent display"), "got {text}");
        assert!(text.contains("desktop session start"), "got {text}");
    }

    #[test]
    fn a_platform_without_sessions_is_not_told_to_start_one() {
        // macOS has one window server per login and no way to make another.
        // Advising `desktop session start` there sends someone to a command
        // whose only possible answer is a refusal.
        let (driver, _) = harness("session-unsupported", FakePorts::new());
        let host = desktop_core::NoSessionHost::new(
            desktop_core::models::backend::Platform::Macos,
            desktop_core::models::backend::DisplayServer::Quartz,
            desktop_core::models::backend::DesktopEnvironment::Aqua,
        );
        let (category, text) = invoke_with(&driver, &host, &["desktop", "session", "status"]);
        assert_eq!(category, ExitCategory::Success);
        assert!(
            !text.contains("desktop session start"),
            "must not advise an impossible command:\n{text}"
        );
        assert!(text.contains("--no-steal-focus"), "got {text}");
    }

    #[test]
    fn json_status_says_whether_sessions_are_possible_at_all() {
        let (driver, _) = harness("session-supported-json", FakePorts::new());
        let (_, text) = invoke_with(
            &driver,
            &FakeSessions::idle(),
            &["desktop", "session", "status", "--json"],
        );
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["running"], false);
        assert_eq!(parsed["supported"], true);
    }

    #[test]
    fn a_session_nobody_can_watch_says_why_instead_of_just_being_headless() {
        // Visible is the default, so headless is the surprising outcome and
        // owes the reader an explanation.
        struct Blind;
        impl SessionHost for Blind {
            fn unwatchable(&self) -> Option<&'static str> {
                Some("Xephyr is not installed")
            }
            fn start(
                &self,
                _options: desktop_core::agent::StartOptions,
            ) -> desktop_core::errors::Result<desktop_core::AgentSession> {
                Ok(desktop_core::AgentSession {
                    visible: false,
                    ..FakeSessions::example()
                })
            }
            fn status(&self) -> Option<desktop_core::AgentSession> {
                None
            }
            fn stop(&self) -> desktop_core::errors::Result<Option<desktop_core::AgentSession>> {
                Ok(None)
            }
            fn launch(&self, _: &str, _: &[String]) -> desktop_core::errors::Result<u32> {
                Ok(0)
            }
        }

        let (driver, _) = harness("session-blind", FakePorts::new());
        let (category, text) = invoke_with(&driver, &Blind, &["desktop", "session", "start"]);
        assert_eq!(category, ExitCategory::Success);
        assert!(text.contains("cannot watch it"), "got {text}");
        assert!(text.contains("Xephyr is not installed"), "got {text}");
    }

    #[test]
    fn a_visible_session_tells_the_user_where_to_look() {
        let (driver, _) = harness("session-visible", FakePorts::new());
        let (_, text) = invoke_with(
            &driver,
            &FakeSessions::idle(),
            &["desktop", "session", "start"],
        );
        assert!(text.contains("window titled"), "got {text}");
    }

    #[test]
    fn starting_an_agent_display_reports_its_geometry() {
        let (driver, _) = harness("session-start", FakePorts::new());
        let sessions = FakeSessions::idle();
        let (category, text) = invoke_with(
            &driver,
            &sessions,
            &["desktop", "session", "start", "--size", "1280x800"],
        );
        assert_eq!(category, ExitCategory::Success);
        assert!(text.contains("1280x800"), "got {text}");
        assert!(sessions.status().is_some(), "the session should now exist");
    }

    #[test]
    fn a_malformed_screen_size_is_a_target_failure_rather_than_a_default() {
        let (driver, _) = harness("session-size", FakePorts::new());
        let (category, text) = invoke_with(
            &driver,
            &FakeSessions::idle(),
            &["desktop", "session", "start", "--size", "big", "--json"],
        );
        assert_eq!(category, ExitCategory::TargetFailure);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["error"], "invalid_argument");
    }

    #[test]
    fn the_session_cookie_never_reaches_the_output() {
        // It is the credential for the agent's display; printing it would hand
        // read-and-inject access to anything that can see a terminal buffer.
        let (driver, _) = harness("session-secret", FakePorts::new());
        for argv in [
            vec!["desktop", "session", "status"],
            vec!["desktop", "session", "status", "--json"],
            vec!["desktop", "session", "start", "--json"],
        ] {
            let (_, text) = invoke_with(&driver, &FakeSessions::running(), &argv);
            assert!(
                !text.contains("00112233445566778899aabbccddeeff"),
                "{argv:?} leaked the cookie:\n{text}"
            );
        }
    }

    #[test]
    fn launching_a_program_sends_it_to_the_agents_display_with_its_arguments() {
        let (driver, _) = harness("session-run", FakePorts::new());
        let sessions = FakeSessions::running();
        let (category, text) = invoke_with(
            &driver,
            &sessions,
            &["desktop", "session", "run", "firefox", "https://x.com"],
        );
        assert_eq!(category, ExitCategory::Success);
        assert!(text.contains(":97"), "got {text}");
        assert_eq!(
            sessions.launched(),
            vec![("firefox".to_owned(), vec!["https://x.com".to_owned()])]
        );
    }

    #[test]
    fn launching_without_a_display_refuses_rather_than_starting_one_implicitly() {
        let (driver, _) = harness("session-run-none", FakePorts::new());
        let sessions = FakeSessions::idle();
        let (category, _) = invoke_with(
            &driver,
            &sessions,
            &["desktop", "session", "run", "firefox"],
        );
        assert_eq!(category, ExitCategory::TargetFailure);
        assert!(sessions.launched().is_empty());
    }

    #[test]
    fn stopping_reports_what_was_stopped_and_leaves_nothing_running() {
        let (driver, _) = harness("session-stop", FakePorts::new());
        let sessions = FakeSessions::running();
        let (category, text) = invoke_with(&driver, &sessions, &["desktop", "session", "stop"]);
        assert_eq!(category, ExitCategory::Success);
        assert!(text.contains(":97"), "got {text}");
        assert_eq!(sessions.status(), None);
    }

    #[test]
    fn stopping_when_nothing_runs_is_reported_rather_than_treated_as_an_error() {
        let (driver, _) = harness("session-stop-none", FakePorts::new());
        let (category, text) = invoke_with(
            &driver,
            &FakeSessions::idle(),
            &["desktop", "session", "stop"],
        );
        assert_eq!(category, ExitCategory::Success);
        assert!(text.contains("No agent display"), "got {text}");
    }

    #[test]
    fn every_command_discloses_that_it_acted_on_the_agents_display() {
        // A screenshot of an empty virtual display and a screenshot of an empty
        // desktop look identical. Only one of them means the agent is pointed
        // somewhere the user did not expect.
        let (driver, _) = harness("session-label", FakePorts::new().with_apps(&["Firefox"]));
        let (_, text) = invoke_with(&driver, &FakeSessions::running(), &["desktop", "apps"]);
        assert!(text.contains("[agent display :97]"), "got {text}");
    }

    #[test]
    fn the_disclosure_is_a_field_rather_than_a_stray_line_in_json_mode() {
        let (driver, _) = harness(
            "session-label-json",
            FakePorts::new().with_apps(&["Firefox"]),
        );
        let (_, text) = invoke_with(
            &driver,
            &FakeSessions::running(),
            &["desktop", "apps", "--json"],
        );
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("must stay a single JSON document: {error}\n{text}"));
        assert_eq!(parsed["display"], ":97");
        assert_eq!(parsed["apps"][0]["name"], "Firefox");
    }

    #[test]
    fn an_error_raised_while_scoped_to_a_session_still_names_the_display() {
        let (driver, _) = harness("session-label-error", FakePorts::new());
        let _ = driver.store().clear();
        let (_, text) = invoke_with(
            &driver,
            &FakeSessions::running(),
            &["desktop", "click", "--element", "1", "--json"],
        );
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["error"], "no_snapshot");
        assert_eq!(parsed["display"], ":97");
    }

    #[test]
    fn asking_for_the_host_removes_the_label_because_nothing_was_redirected() {
        let (driver, _) = harness("session-host", FakePorts::new().with_apps(&["Firefox"]));
        let (_, text) = invoke_with(
            &driver,
            &FakeSessions::running(),
            &["desktop", "apps", "--host"],
        );
        assert!(!text.contains("agent display"), "got {text}");
    }

    #[test]
    fn session_commands_are_not_labelled_with_the_session_they_describe() {
        let (driver, _) = harness("session-label-self", FakePorts::new());
        let (_, text) = invoke_with(
            &driver,
            &FakeSessions::running(),
            &["desktop", "session", "status"],
        );
        assert!(!text.contains("[agent display"), "got {text}");
    }

    #[test]
    fn the_environment_command_prints_shell_exports_that_can_be_pasted() {
        let (driver, _) = harness("session-env", FakePorts::new());
        let (category, text) = invoke_with(
            &driver,
            &FakeSessions::running(),
            &["desktop", "session", "env"],
        );
        assert_eq!(category, ExitCategory::Success);
        assert!(text.contains("export DISPLAY=:97"), "got {text}");
        assert!(
            text.contains("export DBUS_SESSION_BUS_ADDRESS="),
            "got {text}"
        );
    }

    #[test]
    fn the_environment_command_unsets_wayland_before_exporting_anything() {
        // Pasting exports alone would leave WAYLAND_DISPLAY set, and GTK and Qt
        // would send the application to the user's compositor instead.
        let (driver, _) = harness("session-env-wayland", FakePorts::new());
        let (_, text) = invoke_with(
            &driver,
            &FakeSessions::running(),
            &["desktop", "session", "env"],
        );
        let unset = text
            .find("unset WAYLAND_DISPLAY")
            .expect("WAYLAND_DISPLAY must be unset");
        let export = text
            .find("export DISPLAY=")
            .expect("DISPLAY must be exported");
        assert!(unset < export, "the unset must come first:\n{text}");
    }
}
