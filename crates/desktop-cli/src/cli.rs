//! Command-line surface.

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "desktop",
    version,
    about = "See, inspect and control desktop applications — for AI agents.",
    long_about = "A deterministic desktop automation layer for macOS and Linux.\n\n\
                  Run `desktop capabilities` first: what works depends on the display \
                  server and desktop environment, and this tool reports that honestly \
                  rather than failing halfway through an action."
)]
pub struct Cli {
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Refuse every operation that would change the desktop.
    #[arg(long, global = true)]
    pub read_only: bool,

    /// Never take the pointer or keyboard focus away from the user.
    ///
    /// Element-addressed work still goes through, because it uses the
    /// accessibility API rather than synthetic input. Use this when an agent
    /// shares a desktop with a person.
    #[arg(long, global = true)]
    pub no_steal_focus: bool,

    /// Restrict all operations to these applications. Repeatable.
    #[arg(long, global = true, value_name = "APP")]
    pub allow_app: Vec<String>,

    /// Refuse all operations on these applications. Repeatable, and wins over
    /// --allow-app.
    #[arg(long, global = true, value_name = "APP")]
    pub deny_app: Vec<String>,

    /// Refuse actions on elements with these roles. Repeatable.
    #[arg(long, global = true, value_name = "ROLE")]
    pub deny_role: Vec<String>,

    /// Act on the user's own desktop even when an agent session is running.
    ///
    /// Commands normally address the agent's display whenever one exists, so
    /// that forgetting a flag is never what puts keystrokes on someone's
    /// screen. This is how to opt back out.
    #[arg(long, global = true)]
    pub host: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Report the detected platform, display server and selected backends.
    Info,

    /// Report what this environment can and cannot do.
    Capabilities,

    /// Diagnose why the accessibility tree is empty, shallow, or unreachable.
    Doctor,

    /// Perform the one-time interactive grant Wayland input and capture need.
    Setup,

    /// List running applications.
    Apps,

    /// List windows.
    Windows(TargetArgs),

    /// Print the raw accessibility tree, before snapshot pruning.
    Inspect(InspectArgs),

    /// Print a compact semantic snapshot and remember it for --element.
    Snapshot(SnapshotArgs),

    /// Capture the screen or a window to a PNG file.
    Screenshot(ScreenshotArgs),

    /// Bring an application or window to the front.
    Focus(TargetArgs),

    /// Move the pointer.
    Move(PointArgs),

    /// Click an element, a selector match, or a coordinate.
    Click(ClickArgs),

    /// Type literal text into whatever has focus.
    Type(TypeArgs),

    /// Send a keyboard shortcut, e.g. "cmd+s", "ctrl+shift+p", "accel+s".
    Key(KeyArgs),

    /// Scroll the surface under the pointer.
    Scroll(ScrollArgs),

    /// Search the last snapshot for matching elements.
    Find(SelectorArgs),

    /// Wait until an element appears, re-snapshotting until it does.
    Wait(WaitArgs),

    /// Give the agent a display of its own, so it stops sharing yours.
    #[command(subcommand)]
    Session(SessionCommand),

    /// Navigate and automate Chromium or Firefox with browser-native semantics.
    #[command(subcommand)]
    Browser(BrowserCommand),

    /// Internal browser daemon entrypoint.
    #[command(name = "__browser-daemon", hide = true)]
    BrowserDaemon(BrowserDaemonArgs),
}

#[derive(Debug, Args)]
pub struct BrowserDaemonArgs {
    #[arg(long)]
    pub profile: String,
}

#[derive(Debug, Subcommand)]
pub enum BrowserCommand {
    /// Install the pinned Chrome for Testing build.
    Install,
    /// Show browser detection, profile and daemon diagnostics.
    Doctor(BrowserProfileArgs),
    /// Start a managed browser and optionally navigate.
    Open(BrowserOpenArgs),
    /// Attach to an existing loopback CDP or WebDriver BiDi endpoint.
    Connect(BrowserConnectArgs),
    /// Show managed browser state.
    Status(BrowserProfileArgs),
    /// Close the managed browser, or disconnect an attached one.
    Close(BrowserProfileArgs),
    /// Navigate the active tab.
    Goto(BrowserGotoArgs),
    /// Go back one history entry.
    Back(BrowserTimeoutArgs),
    /// Go forward one history entry.
    Forward(BrowserTimeoutArgs),
    /// Reload the active tab.
    Reload(BrowserTimeoutArgs),
    /// Print a compact page snapshot and assign @eN refs.
    Snapshot(BrowserSnapshotArgs),
    /// Capture the active page as PNG.
    Screenshot(BrowserScreenshotArgs),
    /// Read page or element data.
    #[command(subcommand)]
    Get(BrowserGetCommand),
    /// Click one uniquely matched element.
    Click(BrowserTargetArgs),
    /// Replace a field's value. Password fields are always refused.
    Fill(BrowserValueArgs),
    /// Type incrementally into a field. Password fields are always refused.
    Type(BrowserTypeArgs),
    /// Send a key to the page or an element.
    Press(BrowserPressArgs),
    /// Select one or more option values.
    Select(BrowserSelectArgs),
    /// Check a checkbox or radio.
    Check(BrowserTargetArgs),
    /// Uncheck a checkbox.
    Uncheck(BrowserTargetArgs),
    /// Hover an element.
    Hover(BrowserTargetArgs),
    /// Scroll the page or an element.
    Scroll(BrowserScrollArgs),
    /// Click an element with downloads enabled in the given directory.
    Download(BrowserDownloadArgs),
    /// Wait for a selector, text, URL or load state.
    Wait(BrowserWaitArgs),
    /// List, create, switch and close tabs.
    #[command(subcommand)]
    Tab(BrowserTabCommand),
    /// Accept or dismiss the current JavaScript dialog.
    #[command(subcommand)]
    Dialog(BrowserDialogCommand),
}

#[derive(Debug, Args, Default)]
pub struct BrowserProfileArgs {
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
}

#[derive(Debug, Args)]
pub struct BrowserOpenArgs {
    pub url: Option<String>,
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub executable: Option<String>,
    /// Browser engine to automate.
    #[arg(long, value_enum)]
    pub browser: Option<BrowserEngineArg>,
    /// Run without a watchable browser window.
    #[arg(long)]
    pub headless: bool,
}

#[derive(Debug, Args)]
pub struct BrowserConnectArgs {
    pub endpoint: String,
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    /// Protocol exposed by the endpoint.
    #[arg(long, value_enum)]
    pub browser: Option<BrowserEngineArg>,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum BrowserEngineArg {
    #[default]
    Chromium,
    Firefox,
}

#[derive(Debug, Args)]
pub struct BrowserGotoArgs {
    pub url: String,
    #[command(flatten)]
    pub common: BrowserTimeoutArgs,
}

#[derive(Debug, Args)]
pub struct BrowserTimeoutArgs {
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    #[arg(long, default_value_t = 30_000)]
    pub timeout: u64,
}

#[derive(Debug, Args)]
pub struct BrowserSnapshotArgs {
    #[arg(short = 'i', long)]
    pub interactive: bool,
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub max_nodes: Option<usize>,
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
}

#[derive(Debug, Args)]
pub struct BrowserScreenshotArgs {
    #[arg(long, short = 'o', default_value = "browser.png")]
    pub output: String,
    #[arg(long)]
    pub full_page: bool,
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
}

#[derive(Debug, Args, Clone, Default)]
pub struct BrowserTargetArgs {
    /// @eN, css=..., xpath=..., or text=...
    pub target: Option<String>,
    #[arg(long)]
    pub role: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub label: Option<String>,
    #[arg(long = "test-id")]
    pub test_id: Option<String>,
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
}

#[derive(Debug, Args)]
pub struct BrowserValueArgs {
    #[command(flatten)]
    pub selector: BrowserTargetArgs,
    /// Literal value to enter.
    pub value: Option<String>,
}

#[derive(Debug, Args)]
pub struct BrowserTypeArgs {
    #[command(flatten)]
    pub selector: BrowserTargetArgs,
    pub value: Option<String>,
    #[arg(long, default_value_t = 0)]
    pub delay: u64,
}

#[derive(Debug, Args)]
pub struct BrowserPressArgs {
    pub key: String,
    #[command(flatten)]
    pub selector: BrowserTargetArgs,
}

#[derive(Debug, Args)]
pub struct BrowserSelectArgs {
    #[command(flatten)]
    pub selector: BrowserTargetArgs,
    pub values: Vec<String>,
}

#[derive(Debug, Args)]
pub struct BrowserScrollArgs {
    #[command(flatten)]
    pub selector: BrowserTargetArgs,
    #[arg(long, allow_negative_numbers = true, default_value_t = 0)]
    pub x: i64,
    #[arg(long, allow_negative_numbers = true, default_value_t = 0)]
    pub y: i64,
}

#[derive(Debug, Args)]
pub struct BrowserDownloadArgs {
    #[command(flatten)]
    pub selector: BrowserTargetArgs,
    #[arg(long, short = 'o', default_value = ".")]
    pub output: String,
}

#[derive(Debug, Args)]
pub struct BrowserWaitArgs {
    #[command(flatten)]
    pub selector: BrowserTargetArgs,
    #[arg(long)]
    pub text: Option<String>,
    #[arg(long)]
    pub url: Option<String>,
    #[arg(long, value_enum)]
    pub load: Option<BrowserLoadArg>,
    #[arg(long)]
    pub hidden: bool,
    #[arg(long, default_value_t = 30_000)]
    pub timeout: u64,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum BrowserLoadArg {
    Load,
    Domcontentloaded,
    Networkidle,
}

#[derive(Debug, Subcommand)]
pub enum BrowserGetCommand {
    Text(BrowserTargetArgs),
    Html(BrowserTargetArgs),
    Value(BrowserTargetArgs),
    Attr(BrowserAttrArgs),
    Title(BrowserProfileArgs),
    Url(BrowserProfileArgs),
    Count(BrowserTargetArgs),
}

#[derive(Debug, Args)]
pub struct BrowserAttrArgs {
    #[command(flatten)]
    pub selector: BrowserTargetArgs,
    pub attribute: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum BrowserTabCommand {
    List(BrowserProfileArgs),
    New(BrowserTabNewArgs),
    Use(BrowserTabUseArgs),
    Close(BrowserTabCloseArgs),
}
#[derive(Debug, Args)]
pub struct BrowserTabNewArgs {
    pub url: Option<String>,
    #[arg(long)]
    pub profile: Option<String>,
}
#[derive(Debug, Args)]
pub struct BrowserTabUseArgs {
    pub target: String,
    #[arg(long)]
    pub profile: Option<String>,
}
#[derive(Debug, Args)]
pub struct BrowserTabCloseArgs {
    pub target: Option<String>,
    #[arg(long)]
    pub profile: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum BrowserDialogCommand {
    Accept(BrowserDialogArgs),
    Dismiss(BrowserProfileArgs),
}
#[derive(Debug, Args)]
pub struct BrowserDialogArgs {
    #[arg(long)]
    pub prompt_text: Option<String>,
    #[arg(long)]
    pub profile: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    /// Create a persistent, isolated browser workspace.
    Create(SessionNameArgs),

    /// List persistent browser workspaces.
    List,

    /// Start a display only the agent can see, and only the agent can type on.
    Start(SessionStartArgs),

    /// Report the running session, if there is one.
    Status,

    /// End the session and everything running on it.
    Stop,

    /// Permanently remove a workspace, including its cookies and saved logins.
    Delete(SessionNameArgs),

    /// Launch a program onto the agent's display.
    Run(SessionRunArgs),

    /// Print the shell exports that put a command on the agent's display.
    Env,
}

#[derive(Debug, Args)]
pub struct SessionNameArgs {
    /// Persistent workspace name.
    pub name: String,
}

#[derive(Debug, Args)]
pub struct SessionStartArgs {
    /// Persistent workspace name. Defaults to the backwards-compatible workspace.
    #[arg(default_value = "default")]
    pub name: String,

    /// Screen size, e.g. 1920x1080.
    #[arg(long, default_value = "1920x1080", value_name = "WxH")]
    pub size: String,

    /// Use this X display number instead of searching for a free one.
    #[arg(long, value_name = "N")]
    pub display: Option<u32>,

    /// Require a screen you can watch, and fail if this machine cannot.
    ///
    /// Watching is already the default wherever it is possible; this only
    /// turns the fallback into an error, for when an unwatched session would
    /// be worse than none.
    #[arg(long, conflicts_with = "headless")]
    pub visible: bool,

    /// Do not show the agent's screen.
    ///
    /// The default is to render it into a window you can watch, because an
    /// agent driving your computer where you cannot see it is asking you to
    /// take its word for it. This opts out — for a long unattended run, or a
    /// machine where the extra window is in the way.
    #[arg(long)]
    pub headless: bool,

    /// Let the session use your home directory instead of one of its own.
    ///
    /// Off by default, because sharing it means sharing every application
    /// profile: Firefox, Chrome and VS Code are single-instance and lock
    /// theirs, so whichever of you starts second cannot start at all. Sharing
    /// also means the agent arrives logged in to everything you are.
    #[arg(long)]
    pub share_home: bool,
}

#[derive(Debug, Args)]
pub struct SessionRunArgs {
    /// The program to launch.
    pub program: String,

    /// Its arguments.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Parses a `WIDTHxHEIGHT` screen size.
pub fn parse_size(value: &str) -> Result<(u32, u32), String> {
    let (width, height) = value
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("expected WIDTHxHEIGHT, got `{value}`"))?;
    let width: u32 = width
        .trim()
        .parse()
        .map_err(|_| format!("`{width}` is not a width"))?;
    let height: u32 = height
        .trim()
        .parse()
        .map_err(|_| format!("`{height}` is not a height"))?;
    if width == 0 || height == 0 {
        return Err("a screen cannot have a zero dimension".to_owned());
    }
    Ok((width, height))
}

#[derive(Debug, Args, Default)]
pub struct TargetArgs {
    /// Target this application by name, bundle id or pid.
    #[arg(long, value_name = "APP")]
    pub app: Option<String>,

    /// Target this window, as numbered by `desktop windows`.
    #[arg(long, value_name = "ID", conflicts_with = "app")]
    pub window: Option<u32>,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    #[command(flatten)]
    pub budget: BudgetArgs,
}

#[derive(Debug, Args)]
pub struct SnapshotArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    #[command(flatten)]
    pub budget: BudgetArgs,

    /// Include elements that are present but not currently on screen.
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Args, Default)]
pub struct BudgetArgs {
    /// Stop after visiting this many accessibility nodes.
    #[arg(long, value_name = "N")]
    pub max_nodes: Option<usize>,

    /// Stop descending past this depth.
    #[arg(long, value_name = "N")]
    pub max_depth: Option<usize>,
}

#[derive(Debug, Args)]
pub struct ScreenshotArgs {
    #[command(flatten)]
    pub target: TargetArgs,

    /// Write the PNG here. Defaults to a temporary file.
    #[arg(long, short, value_name = "PATH")]
    pub output: Option<String>,
}

#[derive(Debug, Args)]
pub struct PointArgs {
    #[arg(long, allow_negative_numbers = true)]
    pub x: i32,

    #[arg(long, allow_negative_numbers = true)]
    pub y: i32,
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum ButtonArg {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum ViaArg {
    /// Use the accessibility action when the element offers one, else the
    /// pointer.
    #[default]
    Auto,
    /// Always use the accessibility action; fail if there is none.
    Action,
    /// Always move the pointer and click.
    Pointer,
}

#[derive(Debug, Args)]
pub struct ClickArgs {
    /// Click the element with this id from the last snapshot.
    #[arg(long, value_name = "ID")]
    pub element: Option<u32>,

    #[command(flatten)]
    pub selector: SelectorArgs,

    /// Click this x coordinate. Requires --y.
    #[arg(long, allow_negative_numbers = true, requires = "y")]
    pub x: Option<i32>,

    /// Click this y coordinate. Requires --x.
    #[arg(long, allow_negative_numbers = true, requires = "x")]
    pub y: Option<i32>,

    #[arg(long, value_enum, default_value_t = ButtonArg::Left)]
    pub button: ButtonArg,

    /// Click this many times; 2 is a double-click.
    #[arg(long, default_value_t = 1)]
    pub count: u8,

    /// How to deliver the click.
    #[arg(long, value_enum, default_value_t = ViaArg::Auto)]
    pub via: ViaArg,
}

#[derive(Debug, Args, Default)]
pub struct SelectorArgs {
    /// Match elements with this role, e.g. button, textbox, menuitem.
    #[arg(long, value_name = "ROLE")]
    pub role: Option<String>,

    /// Match elements whose accessible name is exactly this.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Match elements containing this text in their name, value or description.
    #[arg(long, value_name = "TEXT")]
    pub text: Option<String>,
}

#[derive(Debug, Args)]
pub struct TypeArgs {
    /// The literal text to type.
    pub text: String,

    /// Write the text straight into this element instead of sending
    /// keystrokes. Does not move the pointer or change focus, so it neither
    /// races nor interferes with someone using the machine.
    #[arg(long, value_name = "ID")]
    pub element: Option<u32>,
}

#[derive(Debug, Args)]
pub struct KeyArgs {
    /// The shortcut, e.g. "cmd+s". Use "accel" for the platform's menu
    /// modifier (Command on macOS, Ctrl on Linux).
    pub shortcut: String,
}

#[derive(Debug, Args)]
pub struct ScrollArgs {
    /// Horizontal distance in logical pixels; negative scrolls left.
    #[arg(long, allow_negative_numbers = true, default_value_t = 0)]
    pub x: i32,

    /// Vertical distance in logical pixels; negative scrolls up.
    #[arg(long, allow_negative_numbers = true, default_value_t = 0)]
    pub y: i32,
}

#[derive(Debug, Args)]
pub struct WaitArgs {
    #[command(flatten)]
    pub selector: SelectorArgs,

    #[command(flatten)]
    pub target: TargetArgs,

    /// Give up after this many milliseconds.
    #[arg(long, default_value_t = 5_000, value_name = "MS")]
    pub timeout: u64,

    /// Re-check this often.
    #[arg(long, default_value_t = 250, value_name = "MS")]
    pub interval: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn the_command_tree_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn browser_engine_is_optional_but_explicit_firefox_is_preserved() {
        let omitted = Cli::try_parse_from(["desktop", "browser", "open", "example.com"]).unwrap();
        let Command::Browser(BrowserCommand::Open(omitted)) = omitted.command else {
            panic!("expected browser open")
        };
        assert!(omitted.browser.is_none());

        let explicit = Cli::try_parse_from([
            "desktop",
            "browser",
            "open",
            "example.com",
            "--browser",
            "firefox",
        ])
        .unwrap();
        let Command::Browser(BrowserCommand::Open(explicit)) = explicit.command else {
            panic!("expected browser open")
        };
        assert!(matches!(explicit.browser, Some(BrowserEngineArg::Firefox)));
    }

    #[test]
    fn the_documented_example_invocations_all_parse() {
        // These are exact forms promised in the README, installed skill and --help.
        let examples: &[&[&str]] = &[
            &["desktop", "apps"],
            &["desktop", "windows"],
            &["desktop", "snapshot"],
            &["desktop", "snapshot", "--app", "Firefox"],
            &["desktop", "inspect", "--window", "123"],
            &["desktop", "screenshot"],
            &[
                "desktop",
                "screenshot",
                "--window",
                "123",
                "--output",
                "screen.png",
            ],
            &["desktop", "focus", "--app", "Visual Studio Code"],
            &["desktop", "move", "--x", "800", "--y", "400"],
            &["desktop", "click", "--x", "800", "--y", "400"],
            &["desktop", "click", "--element", "42"],
            &["desktop", "click", "--role", "button", "--name", "Save"],
            &["desktop", "click", "--text", "Continue"],
            &["desktop", "type", "Hello world"],
            &["desktop", "type", "--element", "42", "Hello world"],
            &["desktop", "key", "cmd+s"],
            &["desktop", "scroll", "--y", "-500"],
            &["desktop", "find", "--role", "textbox"],
            &["desktop", "find", "--text", "Settings"],
            &["desktop", "wait", "--text", "Build complete"],
            &["desktop", "capabilities"],
            &["desktop", "info"],
            &["desktop", "session", "start"],
            &["desktop", "session", "start", "--size", "1280x800"],
            &["desktop", "session", "start", "--visible"],
            &["desktop", "session", "start", "--headless"],
            &["desktop", "session", "status"],
            &["desktop", "session", "stop"],
            &["desktop", "session", "env"],
            &["desktop", "session", "run", "firefox"],
            &["desktop", "session", "run", "firefox", "https://x.com"],
            &["desktop", "snapshot", "--host"],
            &["desktop", "browser", "doctor"],
            &[
                "desktop",
                "browser",
                "open",
                "https://example.com",
                "--headless",
            ],
            &[
                "desktop",
                "browser",
                "open",
                "https://example.com",
                "--browser",
                "firefox",
                "--headless",
            ],
            &["desktop", "browser", "snapshot", "-i"],
            &["desktop", "browser", "fill", "@e2", "value"],
            &["desktop", "browser", "click", "@e3"],
            &[
                "desktop", "browser", "click", "--role", "button", "--name", "Save",
            ],
            &["desktop", "browser", "wait", "--load", "domcontentloaded"],
            &["desktop", "browser", "get", "text", "@e4"],
            &["desktop", "browser", "download", "@e8", "--output", "/tmp"],
            &["desktop", "browser", "tab", "new", "https://example.com"],
            &["desktop", "browser", "dialog", "dismiss"],
            &[
                "desktop",
                "--json",
                "browser",
                "status",
                "--profile",
                "research",
            ],
            &[
                "desktop",
                "--json",
                "browser",
                "goto",
                "https://example.com/form",
                "--timeout",
                "30000",
            ],
            &["desktop", "--json", "browser", "snapshot", "--interactive"],
            &["desktop", "--json", "browser", "press", "Enter", "@e2"],
            &[
                "desktop", "--json", "browser", "type", "@e2", "value", "--delay", "25",
            ],
            &[
                "desktop", "--json", "browser", "select", "@e5", "one", "two",
            ],
            &["desktop", "--json", "browser", "scroll", "--y", "-500"],
            &[
                "desktop", "--json", "browser", "get", "attr", "css=a", "href",
            ],
            &[
                "desktop",
                "--json",
                "browser",
                "get",
                "text",
                "--label",
                "Email address",
            ],
            &[
                "desktop",
                "--json",
                "browser",
                "wait",
                "css=.spinner",
                "--hidden",
                "--timeout",
                "10000",
            ],
            &[
                "desktop",
                "--json",
                "browser",
                "wait",
                "--url",
                "/complete",
                "--timeout",
                "10000",
            ],
            &["desktop", "--json", "browser", "tab", "use", "2"],
            &["desktop", "--json", "browser", "tab", "close"],
            &[
                "desktop",
                "--json",
                "browser",
                "dialog",
                "accept",
                "--prompt-text",
                "value",
            ],
            &["desktop", "--json", "session", "list"],
            &["desktop", "--json", "session", "create", "task-name"],
            &[
                "desktop",
                "--json",
                "session",
                "start",
                "task-name",
                "--visible",
            ],
            &[
                "desktop",
                "--json",
                "session",
                "run",
                "firefox",
                "https://example.com",
            ],
            &[
                "desktop",
                "--no-steal-focus",
                "--json",
                "snapshot",
                "--app",
                "Calculator",
            ],
        ];
        for argv in examples {
            Cli::try_parse_from(*argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse, got:\n{error}"));
        }
    }

    #[test]
    fn json_is_accepted_after_any_subcommand() {
        for argv in [
            vec!["desktop", "apps", "--json"],
            vec!["desktop", "--json", "apps"],
            vec!["desktop", "capabilities", "--json"],
            vec!["desktop", "snapshot", "--json"],
        ] {
            let cli = Cli::try_parse_from(&argv).expect("parses");
            assert!(cli.json, "{argv:?} should set --json");
        }
    }

    #[test]
    fn negative_scroll_and_coordinates_are_accepted_rather_than_read_as_flags() {
        let cli = Cli::try_parse_from(["desktop", "scroll", "--y", "-500"]).expect("parses");
        match cli.command {
            Command::Scroll(args) => assert_eq!(args.y, -500),
            other => panic!("expected scroll, got {other:?}"),
        }
    }

    #[test]
    fn a_coordinate_click_needs_both_axes() {
        assert!(Cli::try_parse_from(["desktop", "click", "--x", "10"]).is_err());
        assert!(Cli::try_parse_from(["desktop", "click", "--y", "10"]).is_err());
        assert!(Cli::try_parse_from(["desktop", "click", "--x", "10", "--y", "20"]).is_ok());
    }

    #[test]
    fn app_and_window_targets_are_mutually_exclusive() {
        // Naming both would leave it ambiguous which one wins.
        assert!(
            Cli::try_parse_from(["desktop", "windows", "--app", "Firefox", "--window", "1"])
                .is_err()
        );
    }

    #[test]
    fn no_steal_focus_is_accepted_before_or_after_the_subcommand() {
        for argv in [
            vec!["desktop", "--no-steal-focus", "type", "hi"],
            vec!["desktop", "type", "hi", "--no-steal-focus"],
        ] {
            let cli = Cli::try_parse_from(&argv).expect("parses");
            assert!(cli.no_steal_focus, "{argv:?}");
        }
    }

    #[test]
    fn typing_can_be_addressed_to_an_element() {
        let cli =
            Cli::try_parse_from(["desktop", "type", "--element", "23", "x.com"]).expect("parses");
        match cli.command {
            Command::Type(args) => {
                assert_eq!(args.element, Some(23));
                assert_eq!(args.text, "x.com");
            }
            other => panic!("expected type, got {other:?}"),
        }
    }

    #[test]
    fn policy_flags_are_repeatable_and_global() {
        let cli = Cli::try_parse_from([
            "desktop",
            "--deny-app",
            "1Password",
            "--deny-app",
            "Keychain Access",
            "--read-only",
            "snapshot",
        ])
        .expect("parses");
        assert_eq!(cli.deny_app.len(), 2);
        assert!(cli.read_only);
    }

    #[test]
    fn watching_and_not_watching_cannot_both_be_asked_for() {
        assert!(
            Cli::try_parse_from(["desktop", "session", "start", "--visible", "--headless"])
                .is_err()
        );
    }

    #[test]
    fn a_named_session_is_positional_and_default_remains_backwards_compatible() {
        let named = Cli::try_parse_from(["desktop", "session", "start", "github"])
            .expect("named session parses");
        let default =
            Cli::try_parse_from(["desktop", "session", "start"]).expect("default session parses");
        match named.command {
            Command::Session(SessionCommand::Start(args)) => assert_eq!(args.name, "github"),
            other => panic!("expected named start, got {other:?}"),
        }
        match default.command {
            Command::Session(SessionCommand::Start(args)) => assert_eq!(args.name, "default"),
            other => panic!("expected default start, got {other:?}"),
        }
    }

    #[test]
    fn a_screen_size_is_read_as_width_by_height() {
        assert_eq!(parse_size("1920x1080"), Ok((1920, 1080)));
        assert_eq!(parse_size("1280X800"), Ok((1280, 800)));
    }

    #[test]
    fn a_nonsensical_screen_size_is_refused_rather_than_rounded() {
        for value in ["1920", "1920x", "axb", "0x1080", "1920x0", ""] {
            assert!(parse_size(value).is_err(), "`{value}` should be refused");
        }
    }

    #[test]
    fn arguments_after_the_program_belong_to_the_program() {
        // `desktop session run firefox --new-window` must not have
        // --new-window read as a flag of `desktop`.
        let cli = Cli::try_parse_from([
            "desktop",
            "session",
            "run",
            "firefox",
            "--new-window",
            "https://x.com",
        ])
        .expect("parses");
        match cli.command {
            Command::Session(SessionCommand::Run(args)) => {
                assert_eq!(args.program, "firefox");
                assert_eq!(args.args, ["--new-window", "https://x.com"]);
            }
            other => panic!("expected session run, got {other:?}"),
        }
    }

    #[test]
    fn the_host_desktop_can_be_addressed_explicitly() {
        for argv in [
            vec!["desktop", "--host", "screenshot"],
            vec!["desktop", "screenshot", "--host"],
        ] {
            let cli = Cli::try_parse_from(&argv).expect("parses");
            assert!(cli.host, "{argv:?}");
        }
    }

    #[test]
    fn click_defaults_to_a_single_left_click_via_the_accessibility_action() {
        let cli = Cli::try_parse_from(["desktop", "click", "--element", "1"]).expect("parses");
        match cli.command {
            Command::Click(args) => {
                assert_eq!(args.count, 1);
                assert!(matches!(args.button, ButtonArg::Left));
                assert!(matches!(args.via, ViaArg::Auto));
            }
            other => panic!("expected click, got {other:?}"),
        }
    }
}
