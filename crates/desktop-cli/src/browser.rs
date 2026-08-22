//! `desktop browser` command adapter.

use std::{io::Read as _, process::Command as ProcessCommand};

use desktop_browser::{BrowserError, Client, Command, GetKind, LoadState, Response, Selector};
use desktop_core::{SessionHost, errors::ExitCategory};
use serde_json::json;
use sha2::{Digest as _, Sha256};

use crate::{
    cli::{
        BrowserCommand, BrowserDialogCommand, BrowserGetCommand, BrowserLoadArg, BrowserTabCommand,
        BrowserTargetArgs, Cli,
    },
    output::Sink,
};

pub fn run(
    command: &BrowserCommand,
    cli: &Cli,
    sessions: &dyn SessionHost,
    sink: &mut Sink<'_>,
) -> ExitCategory {
    match execute(command, cli, sessions, sink) {
        Ok(()) => ExitCategory::Success,
        Err(error) => {
            render_browser_error(sink, &error);
            match error.code.as_str() {
                "timeout" => ExitCategory::Timeout,
                "password_field_denied" | "policy_denied" => ExitCategory::PolicyDenied,
                "browser_not_found" | "daemon_start_failed" | "visible_session_required" => {
                    ExitCategory::SetupOrConfigurationFailure
                }
                "element_not_found"
                | "element_not_actionable"
                | "tab_gone"
                | "invalid_url"
                | "invalid_profile" => ExitCategory::TargetFailure,
                _ => ExitCategory::BackendFailure,
            }
        }
    }
}

fn execute(
    command: &BrowserCommand,
    cli: &Cli,
    sessions: &dyn SessionHost,
    sink: &mut Sink<'_>,
) -> Result<(), BrowserError> {
    if matches!(command, BrowserCommand::Install) {
        return install(sink);
    }
    if let BrowserCommand::Doctor(args) = command {
        return doctor(args.profile.as_deref(), sessions, sink);
    }

    let explicit = command_profile(command);
    let active = sessions.status();
    let profile =
        desktop_browser::profile_name(explicit, active.as_ref().map(|s| s.name.as_str()))?;
    let client = Client::new(&profile)?;

    let wire = command_to_wire(command)?;
    if cli.read_only && wire.mutates() {
        return Err(BrowserError::new(
            "policy_denied",
            "--read-only refuses this browser command",
        ));
    }
    let names_browser = |name: &str| {
        let name = name.to_ascii_lowercase();
        name.contains("chrome") || name.contains("chromium") || name == "browser"
    };
    if cli.deny_app.iter().any(|name| names_browser(name)) {
        return Err(BrowserError::new(
            "policy_denied",
            "--deny-app refuses Chromium browser automation",
        ));
    }
    if !cli.allow_app.is_empty() && !cli.allow_app.iter().any(|name| names_browser(name)) {
        return Err(BrowserError::new(
            "policy_denied",
            "Chromium is outside the --allow-app list",
        ));
    }
    if let Some(role) = selector_role(&wire)
        && cli
            .deny_role
            .iter()
            .any(|denied| denied.eq_ignore_ascii_case(role))
    {
        return Err(BrowserError::new(
            "policy_denied",
            format!("--deny-role refuses browser role {role:?}"),
        ));
    }

    if matches!(command, BrowserCommand::Status(_)) && !client.is_running() {
        return render_value(sink, &json!({"ok":true,"profile":profile,"running":false}));
    }
    if matches!(command, BrowserCommand::Close(_)) && !client.is_running() {
        return Err(BrowserError::new(
            "browser_not_running",
            format!("browser profile {profile:?} is not running"),
        ));
    }

    if let BrowserCommand::Open(args) = command {
        if !client.is_running() {
            start_daemon(&profile, args.headless, sessions)?;
        }
    } else if matches!(command, BrowserCommand::Connect(_)) && !client.is_running() {
        let exe = std::env::current_exe().map_err(io_error)?;
        desktop_browser::spawn_daemon(&exe, &profile)?;
    }

    let response = client.request(wire)?;
    render_response(sink, &response)
}

fn start_daemon(
    profile: &str,
    headless: bool,
    sessions: &dyn SessionHost,
) -> Result<(), BrowserError> {
    let exe = std::env::current_exe().map_err(io_error)?;
    #[cfg(not(target_os = "linux"))]
    let _ = (headless, sessions);
    #[cfg(target_os = "linux")]
    if !headless {
        let active = sessions.status().ok_or_else(|| {
            BrowserError::new(
                "visible_session_required",
                "a visible Linux browser needs an active desktop-driver session",
            )
            .remedy(format!(
                "Run `desktop session start {profile} --visible`, then retry."
            ))
        })?;
        if active.name != profile {
            return Err(BrowserError::new(
                "profile_session_mismatch",
                format!(
                    "browser profile {profile:?} does not match active session {:?}",
                    active.name
                ),
            )
            .remedy(format!(
                "Use --profile {} or restart the matching session.",
                active.name
            )));
        }
        if !active.visible {
            return Err(BrowserError::new(
                "visible_session_required",
                "the active session is headless; authentication and visible browsing require --visible",
            ));
        }
        let program = exe.to_string_lossy().to_string();
        sessions
            .launch(
                &program,
                &[
                    "__browser-daemon".into(),
                    "--profile".into(),
                    profile.into(),
                ],
            )
            .map_err(|e| BrowserError::new("daemon_start_failed", e.to_string()))?;
        return desktop_browser::daemon_wait(profile);
    }
    desktop_browser::spawn_daemon(&exe, profile)
}

fn command_to_wire(command: &BrowserCommand) -> Result<Command, BrowserError> {
    Ok(match command {
        BrowserCommand::Open(a) => Command::Open {
            url: a.url.clone(),
            executable: a.executable.clone(),
            headless: a.headless,
        },
        BrowserCommand::Connect(a) => Command::Connect {
            endpoint: a.endpoint.clone(),
        },
        BrowserCommand::Status(_) => Command::Status,
        BrowserCommand::Close(_) => Command::Close,
        BrowserCommand::Goto(a) => Command::Goto {
            url: a.url.clone(),
            timeout_ms: a.common.timeout,
        },
        BrowserCommand::Back(a) => Command::Back {
            timeout_ms: a.timeout,
        },
        BrowserCommand::Forward(a) => Command::Forward {
            timeout_ms: a.timeout,
        },
        BrowserCommand::Reload(a) => Command::Reload {
            timeout_ms: a.timeout,
        },
        BrowserCommand::Snapshot(a) => Command::Snapshot {
            interactive: a.interactive || !a.all,
            max_nodes: a
                .max_nodes
                .unwrap_or(if a.interactive || !a.all { 200 } else { 500 }),
        },
        BrowserCommand::Screenshot(a) => Command::Screenshot {
            output: a.output.clone(),
            full_page: a.full_page,
        },
        BrowserCommand::Get(get) => match get {
            BrowserGetCommand::Text(a) => Command::Get {
                kind: GetKind::Text,
                selector: Some(selector(a)?),
                attribute: None,
            },
            BrowserGetCommand::Html(a) => Command::Get {
                kind: GetKind::Html,
                selector: Some(selector(a)?),
                attribute: None,
            },
            BrowserGetCommand::Value(a) => Command::Get {
                kind: GetKind::Value,
                selector: Some(selector(a)?),
                attribute: None,
            },
            BrowserGetCommand::Attr(a) => Command::Get {
                kind: GetKind::Attr,
                selector: Some(selector(&a.selector)?),
                attribute: Some(a.attribute.clone().ok_or_else(|| {
                    BrowserError::new("attribute_required", "get attr requires an attribute name")
                })?),
            },
            BrowserGetCommand::Title(_) => Command::Get {
                kind: GetKind::Title,
                selector: None,
                attribute: None,
            },
            BrowserGetCommand::Url(_) => Command::Get {
                kind: GetKind::Url,
                selector: None,
                attribute: None,
            },
            BrowserGetCommand::Count(a) => Command::Get {
                kind: GetKind::Count,
                selector: Some(selector(a)?),
                attribute: None,
            },
        },
        BrowserCommand::Click(a) => Command::Click {
            selector: selector(a)?,
        },
        BrowserCommand::Fill(a) => Command::Fill {
            selector: selector(&a.selector)?,
            value: a
                .value
                .clone()
                .ok_or_else(|| BrowserError::new("value_required", "fill requires a value"))?,
        },
        BrowserCommand::Type(a) => Command::Type {
            selector: selector(&a.selector)?,
            value: a
                .value
                .clone()
                .ok_or_else(|| BrowserError::new("value_required", "type requires a value"))?,
            delay_ms: a.delay,
        },
        BrowserCommand::Press(a) => Command::Press {
            selector: optional_selector(&a.selector)?,
            key: a.key.clone(),
        },
        BrowserCommand::Select(a) => Command::Select {
            selector: selector(&a.selector)?,
            values: if a.values.is_empty() {
                return Err(BrowserError::new(
                    "value_required",
                    "select requires at least one value",
                ));
            } else {
                a.values.clone()
            },
        },
        BrowserCommand::Check(a) => Command::Check {
            selector: selector(a)?,
            checked: true,
        },
        BrowserCommand::Uncheck(a) => Command::Check {
            selector: selector(a)?,
            checked: false,
        },
        BrowserCommand::Hover(a) => Command::Hover {
            selector: selector(a)?,
        },
        BrowserCommand::Scroll(a) => Command::Scroll {
            selector: optional_selector(&a.selector)?,
            x: a.x,
            y: a.y,
        },
        BrowserCommand::Download(a) => Command::Download {
            selector: selector(&a.selector)?,
            output: a.output.clone(),
        },
        BrowserCommand::Wait(a) => Command::Wait {
            selector: optional_selector(&a.selector)?,
            text: a.text.clone(),
            url: a.url.clone(),
            load: a.load.map(|l| match l {
                BrowserLoadArg::Load => LoadState::Load,
                BrowserLoadArg::Domcontentloaded => LoadState::Domcontentloaded,
                BrowserLoadArg::Networkidle => LoadState::Networkidle,
            }),
            hidden: a.hidden,
            timeout_ms: a.timeout,
        },
        BrowserCommand::Tab(tab) => match tab {
            BrowserTabCommand::List(_) => Command::TabList,
            BrowserTabCommand::New(a) => Command::TabNew { url: a.url.clone() },
            BrowserTabCommand::Use(a) => Command::TabUse {
                target: a.target.clone(),
            },
            BrowserTabCommand::Close(a) => Command::TabClose {
                target: a.target.clone(),
            },
        },
        BrowserCommand::Dialog(dialog) => match dialog {
            BrowserDialogCommand::Accept(a) => Command::Dialog {
                accept: true,
                prompt_text: a.prompt_text.clone(),
            },
            BrowserDialogCommand::Dismiss(_) => Command::Dialog {
                accept: false,
                prompt_text: None,
            },
        },
        BrowserCommand::Install | BrowserCommand::Doctor(_) => {
            return Err(BrowserError::new(
                "internal",
                "command handled before protocol conversion",
            ));
        }
    })
}

fn selector(a: &BrowserTargetArgs) -> Result<Selector, BrowserError> {
    optional_selector(a)?.ok_or_else(|| {
        BrowserError::new(
            "selector_required",
            "provide @eN, css=..., xpath=..., text=..., --role, --label, or --test-id",
        )
    })
}
fn optional_selector(a: &BrowserTargetArgs) -> Result<Option<Selector>, BrowserError> {
    let mut candidates = Vec::new();
    if let Some(raw) = &a.target {
        candidates.push(if let Some(v) = raw.strip_prefix("css=") {
            Selector::Css(v.into())
        } else if let Some(v) = raw.strip_prefix("xpath=") {
            Selector::XPath(v.into())
        } else if let Some(v) = raw.strip_prefix("text=") {
            Selector::Text(v.into())
        } else if raw.starts_with("@e") {
            Selector::Ref(raw.clone())
        } else {
            return Err(BrowserError::new(
                "invalid_selector",
                format!("unknown selector {raw:?}; prefix CSS with css="),
            ));
        });
    }
    if let Some(role) = &a.role {
        candidates.push(Selector::Role {
            role: role.clone(),
            name: a.name.clone(),
        });
    } else if a.name.is_some() {
        return Err(BrowserError::new(
            "invalid_selector",
            "--name requires --role",
        ));
    }
    if let Some(v) = &a.label {
        candidates.push(Selector::Label(v.clone()));
    }
    if let Some(v) = &a.test_id {
        candidates.push(Selector::TestId(v.clone()));
    }
    if candidates.len() > 1 {
        return Err(BrowserError::new(
            "ambiguous_selector",
            "provide exactly one selector strategy",
        ));
    }
    Ok(candidates.pop())
}

fn command_profile(command: &BrowserCommand) -> Option<&str> {
    match command {
        BrowserCommand::Open(a) => a.profile.as_deref(),
        BrowserCommand::Connect(a) => a.profile.as_deref(),
        BrowserCommand::Doctor(a) | BrowserCommand::Status(a) | BrowserCommand::Close(a) => {
            a.profile.as_deref()
        }
        BrowserCommand::Goto(a) => a.common.profile.as_deref(),
        BrowserCommand::Back(a) | BrowserCommand::Forward(a) | BrowserCommand::Reload(a) => {
            a.profile.as_deref()
        }
        BrowserCommand::Snapshot(a) => a.profile.as_deref(),
        BrowserCommand::Screenshot(a) => a.profile.as_deref(),
        BrowserCommand::Get(g) => match g {
            BrowserGetCommand::Text(a)
            | BrowserGetCommand::Html(a)
            | BrowserGetCommand::Value(a)
            | BrowserGetCommand::Count(a) => a.profile.as_deref(),
            BrowserGetCommand::Attr(a) => a.selector.profile.as_deref(),
            BrowserGetCommand::Title(a) | BrowserGetCommand::Url(a) => a.profile.as_deref(),
        },
        BrowserCommand::Click(a)
        | BrowserCommand::Check(a)
        | BrowserCommand::Uncheck(a)
        | BrowserCommand::Hover(a) => a.profile.as_deref(),
        BrowserCommand::Fill(a) => a.selector.profile.as_deref(),
        BrowserCommand::Type(a) => a.selector.profile.as_deref(),
        BrowserCommand::Press(a) => a.selector.profile.as_deref(),
        BrowserCommand::Select(a) => a.selector.profile.as_deref(),
        BrowserCommand::Scroll(a) => a.selector.profile.as_deref(),
        BrowserCommand::Download(a) => a.selector.profile.as_deref(),
        BrowserCommand::Wait(a) => a.selector.profile.as_deref(),
        BrowserCommand::Tab(t) => match t {
            BrowserTabCommand::List(a) => a.profile.as_deref(),
            BrowserTabCommand::New(a) => a.profile.as_deref(),
            BrowserTabCommand::Use(a) => a.profile.as_deref(),
            BrowserTabCommand::Close(a) => a.profile.as_deref(),
        },
        BrowserCommand::Dialog(d) => match d {
            BrowserDialogCommand::Accept(a) => a.profile.as_deref(),
            BrowserDialogCommand::Dismiss(a) => a.profile.as_deref(),
        },
        BrowserCommand::Install => None,
    }
}

fn selector_role(command: &Command) -> Option<&str> {
    let selector = match command {
        Command::Click { selector }
        | Command::Fill { selector, .. }
        | Command::Type { selector, .. }
        | Command::Select { selector, .. }
        | Command::Check { selector, .. }
        | Command::Hover { selector }
        | Command::Download { selector, .. } => Some(selector),
        Command::Press { selector, .. }
        | Command::Scroll { selector, .. }
        | Command::Wait { selector, .. }
        | Command::Get { selector, .. } => selector.as_ref(),
        _ => None,
    };
    match selector {
        Some(Selector::Role { role, .. }) => Some(role),
        _ => None,
    }
}

fn render_response(sink: &mut Sink<'_>, response: &Response) -> Result<(), BrowserError> {
    if let Some(error) = &response.error {
        return Err(error.clone());
    }
    let value = serde_json::to_value(response).unwrap_or_else(|_| json!({"ok":false}));
    render_value(sink, &value)
}
fn render_value(sink: &mut Sink<'_>, value: &serde_json::Value) -> Result<(), BrowserError> {
    if sink.is_json() {
        sink.value(value);
    } else if let Some(elements) = value.pointer("/result/elements").and_then(|v| v.as_array()) {
        for e in elements {
            let r = e["ref"].as_str().unwrap_or("?");
            let role = e["role"].as_str().unwrap_or("unknown");
            let name = e["name"].as_str().unwrap_or("");
            let suffix: String = if e["redacted"] == true {
                " value=<redacted>".into()
            } else if let Some(v) = e["value"].as_str() {
                if v.is_empty() {
                    "".into()
                } else {
                    format!(" value={v:?}")
                }
            } else {
                "".into()
            };
            sink.line(&format!("{r} {role} {name:?}{suffix}"));
        }
    } else {
        sink.line(&serde_json::to_string_pretty(value).unwrap_or_default());
    }
    Ok(())
}
pub fn render_browser_error(sink: &mut Sink<'_>, error: &BrowserError) {
    if sink.is_json() {
        sink.value(&json!({"error":error.code,"message":error.message,"retryable":error.retryable,"remedy":error.remedy}));
    } else {
        sink.line(&format!("error [{}]: {}", error.code, error.message));
        if let Some(remedy) = &error.remedy {
            sink.blank();
            sink.line(remedy);
        }
    }
}

fn doctor(
    profile: Option<&str>,
    sessions: &dyn SessionHost,
    sink: &mut Sink<'_>,
) -> Result<(), BrowserError> {
    let active = sessions.status();
    let profile = desktop_browser::profile_name(profile, active.as_ref().map(|s| s.name.as_str()))?;
    let client = Client::new(&profile)?;
    let executable = desktop_browser::browser_executable(None).ok();
    render_value(
        sink,
        &json!({"ok":executable.is_some(),"profile":profile,"browser":executable.as_ref().map(|p|p.display().to_string()),"daemon_running":client.is_running(),"session":active.map(|s|json!({"name":s.name,"display":s.display,"visible":s.visible})),"remedy":if executable.is_none(){Some("Run `desktop browser install` or pass --executable to `browser open`.")}else{None}}),
    )
}

fn install(sink: &mut Sink<'_>) -> Result<(), BrowserError> {
    const VERSION: &str = "151.0.7922.174";
    if cfg!(all(target_os = "linux", not(target_arch = "x86_64"))) {
        return Err(BrowserError::new(
            "browser_install_unsupported",
            "Chrome for Testing Stable does not publish this Linux architecture",
        )
        .remedy("Install a compatible system Chromium and use `browser open --executable PATH`."));
    }
    let (platform, archive, inner, expected_sha256) = if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            (
                "mac-arm64",
                "chrome-mac-arm64.zip",
                "chrome-mac-arm64/Google Chrome for Testing.app",
                "7897e8c7241500f67f99ddf0ddf86bd173a606f45bb2fc16ea8b3513f149a38b",
            )
        } else {
            (
                "mac-x64",
                "chrome-mac-x64.zip",
                "chrome-mac-x64/Google Chrome for Testing.app",
                "fefd2eb5259893a3e8d8b29ce406b627134c7ea482345fe841706ed590db9640",
            )
        }
    } else {
        (
            "linux64",
            "chrome-linux64.zip",
            "chrome-linux64",
            "b8531103d26142e78a05425bf3ae3ebe30e2a2d3c5971b639d37ecd93c52e253",
        )
    };
    let target = desktop_browser::installed_path();
    let root = target
        .parent()
        .and_then(|p| {
            if cfg!(target_os = "macos") {
                p.parent().and_then(|p| p.parent()).and_then(|p| p.parent())
            } else {
                p.parent()
            }
        })
        .ok_or_else(|| BrowserError::new("install_failed", "invalid install path"))?
        .to_path_buf();
    std::fs::create_dir_all(&root).map_err(io_error)?;
    let tmp = std::env::temp_dir().join(format!("desktop-driver-{archive}-{}", std::process::id()));
    let url = format!(
        "https://storage.googleapis.com/chrome-for-testing-public/{VERSION}/{platform}/chrome-{platform}.zip"
    );
    let status = ProcessCommand::new("curl")
        .args([
            "--fail",
            "--location",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--output",
        ])
        .arg(&tmp)
        .arg(&url)
        .status()
        .map_err(|e| BrowserError::new("install_failed", format!("could not run curl: {e}")))?;
    if !status.success() {
        return Err(BrowserError::new(
            "install_failed",
            format!("download failed: {url}"),
        ));
    }
    let actual_sha256 = sha256_file(&tmp)?;
    if actual_sha256 != expected_sha256 {
        let _ = std::fs::remove_file(&tmp);
        return Err(BrowserError::new(
            "install_checksum_mismatch",
            format!(
                "Chrome archive SHA-256 mismatch: expected {expected_sha256}, got {actual_sha256}"
            ),
        ));
    }
    let unpack = std::env::temp_dir().join(format!(
        "desktop-driver-browser-unpack-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&unpack).map_err(io_error)?;
    let status = ProcessCommand::new("unzip")
        .args(["-q", "-o"])
        .arg(&tmp)
        .arg("-d")
        .arg(&unpack)
        .status()
        .map_err(|e| BrowserError::new("install_failed", format!("could not run unzip: {e}")))?;
    if !status.success() {
        return Err(BrowserError::new(
            "install_failed",
            "Chrome archive could not be unpacked",
        ));
    }
    let source = unpack.join(inner);
    let install_tree = if cfg!(target_os = "macos") {
        root.join("Google Chrome for Testing.app")
    } else {
        root.join("chrome-linux64")
    };
    if install_tree.exists() {
        std::fs::remove_dir_all(&install_tree).map_err(io_error)?;
    }
    let status = ProcessCommand::new("cp")
        .args(["-R"])
        .arg(&source)
        .arg(&install_tree)
        .status()
        .map_err(io_error)?;
    if !status.success() {
        return Err(BrowserError::new(
            "install_failed",
            "could not install browser tree",
        ));
    }
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_dir_all(&unpack);
    render_value(
        sink,
        &json!({"ok":true,"version":VERSION,"path":target,"source":url,"sha256":expected_sha256}),
    )
}
fn sha256_file(path: &std::path::Path) -> Result<String, BrowserError> {
    let mut file = std::fs::File::open(path).map_err(io_error)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn io_error(e: std::io::Error) -> BrowserError {
    BrowserError::new("browser_io_error", e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_archive_hashes_are_checked_with_sha256() {
        let path = std::env::temp_dir().join(format!("desktop-browser-sha-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(path);
    }
}
