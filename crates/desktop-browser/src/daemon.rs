use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use crate::{
    BrowserEngine, BrowserError, BrowserResult, Command, GetKind, LoadState, Request, Response,
    Selector,
    backend::Browser,
    cdp::{js_string, snapshot_script},
    paths::{browser_executable, profile_paths, save_profile_engine},
};

#[derive(Clone, Debug)]
pub struct DaemonOptions {
    pub profile: String,
}

pub struct Client {
    profile: String,
    socket: PathBuf,
}

impl Client {
    pub fn new(profile: &str) -> BrowserResult<Self> {
        Ok(Self {
            profile: profile.into(),
            socket: profile_paths(profile)?.socket,
        })
    }
    pub fn is_running(&self) -> bool {
        UnixStream::connect(&self.socket).is_ok()
    }
    pub fn request(&self, command: Command) -> BrowserResult<Response> {
        let mut stream = UnixStream::connect(&self.socket).map_err(|_| {
            BrowserError::new(
                "browser_not_running",
                format!("browser profile {:?} is not running", self.profile),
            )
            .remedy("Run `desktop browser open [URL]` first.")
        })?;
        stream.set_read_timeout(Some(Duration::from_secs(130))).ok();
        serde_json::to_writer(&mut stream, &Request { command }).map_err(protocol)?;
        stream.write_all(b"\n").map_err(io_error)?;
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .map_err(io_error)?;
        let response: Response = serde_json::from_str(&line).map_err(protocol)?;
        Ok(response)
    }
}

pub fn spawn_daemon(program: &Path, profile: &str) -> BrowserResult<()> {
    let paths = profile_paths(profile)?;
    if UnixStream::connect(&paths.socket).is_ok() {
        return Ok(());
    }
    ProcessCommand::new(program)
        .args(["__browser-daemon", "--profile", profile])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| BrowserError::new("daemon_start_failed", e.to_string()))?;
    wait_for_socket(&paths.socket)
}

pub fn wait_for_socket(socket: &Path) -> BrowserResult<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(BrowserError::new(
        "daemon_start_failed",
        "browser daemon did not become ready within 5 seconds",
    )
    .retryable())
}

pub fn run_daemon(options: DaemonOptions) -> BrowserResult<()> {
    let paths = profile_paths(&options.profile)?;
    if let Some(parent) = paths.socket.parent() {
        std::fs::create_dir_all(parent).map_err(io_error)?;
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(io_error)?;
    }
    if paths.socket.exists() {
        if UnixStream::connect(&paths.socket).is_ok() {
            return Err(BrowserError::new(
                "browser_already_running",
                "a daemon already owns this profile",
            ));
        }
        std::fs::remove_file(&paths.socket).map_err(io_error)?;
    }
    let listener = UnixListener::bind(&paths.socket).map_err(io_error)?;
    listener.set_nonblocking(true).map_err(io_error)?;
    let terminated = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&terminated);
    ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst)).map_err(|error| {
        BrowserError::new(
            "daemon_signal_handler_failed",
            format!("could not install daemon termination handler: {error}"),
        )
    })?;
    let mut state = State {
        profile: options.profile.clone(),
        browser: None,
        should_exit: false,
        headless_owned: false,
        last_activity: Instant::now(),
    };
    loop {
        if terminated.load(Ordering::SeqCst) {
            break;
        }
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if state.headless_owned
                    && state.last_activity.elapsed() >= Duration::from_secs(60 * 60)
                {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(error) => return Err(io_error(error)),
        };
        let mut line = String::new();
        let Ok(cloned) = stream.try_clone() else {
            continue;
        };
        let Ok(read) = BufReader::new(cloned).read_line(&mut line) else {
            continue;
        };
        // Readiness probes connect and close without a request. They must not
        // turn into a response write on a dead peer and kill the daemon.
        if read == 0 || line.trim().is_empty() {
            continue;
        }
        let request = serde_json::from_str::<Request>(&line).map_err(protocol);
        let response = match request {
            Ok(request) => state.handle(request.command),
            Err(error) => Response::failure(&options.profile, error),
        };
        state.last_activity = Instant::now();
        if serde_json::to_writer(&mut stream, &response).is_err() {
            continue;
        }
        if stream.write_all(b"\n").is_err() {
            continue;
        }
        if state.should_exit {
            break;
        }
    }
    let _ = std::fs::remove_file(&paths.socket);
    Ok(())
}

struct State {
    profile: String,
    browser: Option<Browser>,
    should_exit: bool,
    headless_owned: bool,
    last_activity: Instant,
}

impl State {
    fn handle(&mut self, command: Command) -> Response {
        match self.execute(command) {
            Ok(result) => self.response(result),
            Err(error) => Response::failure(&self.profile, error),
        }
    }
    fn response(&mut self, result: Value) -> Response {
        let mut response = Response::success(&self.profile, result);
        if let Some(browser) = self.browser.as_mut() {
            response.tab_id = Some(browser.target_id().to_owned());
            response.document_id = Some(browser.document_id());
            // Runtime is suspended while alert/confirm/prompt is open. Asking
            // for location here would block the response that tells the next
            // CLI invocation it may handle the dialog.
            if browser.take_suppress_url() {
            } else if !browser.dialog_open() {
                response.url = browser.url().ok();
            }
        }
        response
    }
    fn browser(&mut self) -> BrowserResult<&mut Browser> {
        self.browser.as_mut().ok_or_else(|| {
            BrowserError::new("browser_not_running", "no browser is open")
                .remedy("Run `desktop browser open [URL]` first.")
        })
    }
    fn execute(&mut self, command: Command) -> BrowserResult<Value> {
        match command {
            Command::Open {
                url,
                executable,
                headless,
                engine,
            } => {
                if let Some(browser) = self.browser.as_ref() {
                    validate_open(browser, self.headless_owned, headless, engine)?;
                    // Repair profiles created before engine markers existed,
                    // using the live daemon as the source of truth.
                    save_profile_engine(&self.profile, engine)?;
                } else {
                    let paths = profile_paths(&self.profile)?;
                    let executable = browser_executable(engine, executable.as_deref())?;
                    let profile = match engine {
                        BrowserEngine::Chromium => &paths.user_data,
                        BrowserEngine::Firefox => &paths.firefox_user_data,
                    };
                    let browser = Browser::launch(engine, &executable, profile, headless)?;
                    if let Err(error) = save_profile_engine(&self.profile, engine) {
                        drop(browser);
                        return Err(error);
                    }
                    self.browser = Some(browser);
                    self.headless_owned = headless;
                }
                if let Some(url) = url {
                    navigate(self.browser()?, &url, 30_000)?;
                }
                Ok(json!({"opened":true,"headless":self.headless_owned,"browser":engine.as_str()}))
            }
            Command::Connect { endpoint, engine } => {
                if self.browser.is_some() {
                    return Err(BrowserError::new(
                        "browser_already_running",
                        "close the current browser before connecting",
                    ));
                }
                let browser = Browser::connect(engine, &endpoint)?;
                if let Err(error) = save_profile_engine(&self.profile, engine) {
                    drop(browser);
                    return Err(error);
                }
                self.browser = Some(browser);
                self.headless_owned = false;
                Ok(json!({"connected":true,"owned":false}))
            }
            Command::Status => Ok(json!({
                "running":self.browser.is_some(),
                "owned":self.browser.as_ref().is_some_and(Browser::owned),
                "headless":self.browser.as_ref().and_then(|browser| browser.owned().then_some(self.headless_owned)),
                "browser":self.browser.as_ref().map(|browser|browser.engine().as_str()),
            })),
            Command::Close => {
                self.browser.take();
                self.should_exit = true;
                Ok(json!({"closed":true}))
            }
            Command::Goto { url, timeout_ms } => {
                navigate(self.browser()?, &url, timeout_ms)?;
                Ok(json!({"navigated":true}))
            }
            Command::Back { timeout_ms } => {
                history(self.browser()?, -1, timeout_ms)?;
                Ok(json!({"navigated":true}))
            }
            Command::Forward { timeout_ms } => {
                history(self.browser()?, 1, timeout_ms)?;
                Ok(json!({"navigated":true}))
            }
            Command::Reload { timeout_ms } => {
                let b = self.browser()?;
                let previous_loader = b.current_loader_id()?;
                b.call("Page.reload", json!({}))?;
                b.bump_document();
                wait_for_new_loader(b, &previous_loader, timeout_ms)?;
                Ok(json!({"reloaded":true}))
            }
            Command::Snapshot {
                interactive,
                max_nodes,
            } => {
                let b = self.browser()?;
                let elements = b.evaluate(&snapshot_script(interactive, max_nodes))?;
                Ok(json!({"elements":elements,"interactive_only":interactive}))
            }
            Command::Screenshot { output, full_page } => {
                screenshot(self.browser()?, &output, full_page)
            }
            Command::Get {
                kind,
                selector,
                attribute,
            } => get(
                self.browser()?,
                kind,
                selector.as_ref(),
                attribute.as_deref(),
            ),
            Command::Click { selector } => action(self.browser()?, &selector, Action::Click),
            Command::Fill { selector, value } => {
                action(self.browser()?, &selector, Action::Fill(value))
            }
            Command::Type {
                selector,
                value,
                delay_ms,
            } => type_text(self.browser()?, &selector, &value, delay_ms),
            Command::Press { selector, key } => press(self.browser()?, selector.as_ref(), &key),
            Command::Select { selector, values } => {
                action(self.browser()?, &selector, Action::Select(values))
            }
            Command::Check { selector, checked } => {
                action(self.browser()?, &selector, Action::Check(checked))
            }
            Command::Hover { selector } => action(self.browser()?, &selector, Action::Hover),
            Command::Scroll { selector, x, y } => scroll(self.browser()?, selector.as_ref(), x, y),
            Command::Download { selector, output } => {
                prepare_download_directory(Path::new(&output))?;
                let browser = self.browser()?;
                browser.call_browser(
                    "Browser.setDownloadBehavior",
                    json!({"behavior":"allow","downloadPath":output,"eventsEnabled":true}),
                )?;
                let clicked = action(browser, &selector, Action::Click)?;
                Ok(json!({"download_started":true,"directory":output,"action":clicked}))
            }
            Command::Wait {
                selector,
                text,
                url,
                load,
                hidden,
                timeout_ms,
            } => wait(
                self.browser()?,
                selector.as_ref(),
                text.as_deref(),
                url.as_deref(),
                load,
                hidden,
                timeout_ms,
            ),
            Command::TabList => Ok(json!({"tabs":self.browser()?.tabs()?})),
            Command::TabNew { url } => {
                let b = self.browser()?;
                let result = b.call_browser(
                    "Target.createTarget",
                    json!({"url":url.unwrap_or_else(||"about:blank".into())}),
                )?;
                let id = result["targetId"].as_str().unwrap_or_default().to_owned();
                b.attach(&id)?;
                b.bump_document();
                Ok(json!({"created":id}))
            }
            Command::TabUse { target } => {
                let b = self.browser()?;
                let tabs = b.tabs()?;
                let id = resolve_tab(&tabs, &target)?;
                b.call_browser("Target.activateTarget", json!({"targetId":id}))?;
                b.attach(&id)?;
                b.bump_document();
                Ok(json!({"active":id}))
            }
            Command::TabClose { target } => {
                let b = self.browser()?;
                let id = if let Some(target) = target {
                    resolve_tab(&b.tabs()?, &target)?
                } else {
                    b.target_id().to_owned()
                };
                b.call_browser("Target.closeTarget", json!({"targetId":id}))?;
                if id == b.target_id() {
                    b.attach_first_available()?;
                    b.bump_document();
                }
                Ok(json!({"closed":id}))
            }
            Command::Dialog {
                accept,
                prompt_text,
            } => {
                let b = self.browser()?;
                let mut params = json!({"accept":accept});
                if let Some(prompt_text) = prompt_text {
                    params["promptText"] = json!(prompt_text);
                }
                b.call("Page.handleJavaScriptDialog", params)?;
                b.set_dialog_open(false);
                Ok(json!({"handled":true,"accepted":accept}))
            }
        }
    }
}

fn validate_open(
    browser: &Browser,
    actual_headless: bool,
    requested_headless: bool,
    requested_engine: BrowserEngine,
) -> BrowserResult<()> {
    validate_open_mode(browser.owned(), actual_headless, requested_headless)?;
    if browser.engine() != requested_engine {
        return Err(BrowserError::new(
            "browser_engine_mismatch",
            format!("profile is already running {}", browser.engine().as_str()),
        )
        .remedy("Close the profile before changing its browser engine."));
    }
    Ok(())
}

fn validate_open_mode(
    owned: bool,
    actual_headless: bool,
    requested_headless: bool,
) -> BrowserResult<()> {
    if !owned {
        return Err(BrowserError::new(
            "browser_already_running",
            "this profile is attached to an external browser; close it before opening an owned browser",
        ));
    }
    if actual_headless != requested_headless {
        return Err(BrowserError::new(
            "browser_mode_mismatch",
            format!(
                "browser is already running in {} mode",
                if actual_headless {
                    "headless"
                } else {
                    "visible"
                }
            ),
        )
        .remedy("Close the profile before changing its browser mode."));
    }
    Ok(())
}

fn navigate(browser: &mut Browser, url: &str, timeout: u64) -> BrowserResult<()> {
    let url = normalize_url(url)?;
    let result = browser.call("Page.navigate", json!({"url":url}))?;
    if let Some(error) = result["errorText"].as_str() {
        return Err(BrowserError::new("navigation_failed", error));
    }
    browser.bump_document();
    if let Some(loader_id) = result["loaderId"].as_str() {
        browser.wait_for_loader(loader_id, timeout)
    } else {
        // Same-document navigations have no loader id and are committed before
        // Page.navigate responds.
        browser.wait_ready(timeout)
    }
}
fn normalize_url(url: &str) -> BrowserResult<String> {
    let value = if url.contains("://") || url.starts_with("about:") || url.starts_with("data:") {
        url.to_owned()
    } else {
        format!("https://{url}")
    };
    if value.chars().any(char::is_whitespace)
        || !["http://", "https://", "file://", "about:", "data:"]
            .iter()
            .any(|prefix| value.starts_with(prefix))
    {
        return Err(BrowserError::new(
            "invalid_url",
            format!("unsupported or malformed URL {value:?}"),
        ));
    }
    Ok(value)
}
fn history(browser: &mut Browser, delta: i32, timeout: u64) -> BrowserResult<()> {
    let previous_url = browser.url()?;
    browser.evaluate(&format!("history.go({delta})"))?;
    browser.bump_document();
    let deadline = Instant::now() + Duration::from_millis(timeout);
    loop {
        if browser.url()? != previous_url {
            return browser.wait_ready(timeout);
        }
        if Instant::now() >= deadline {
            return Err(BrowserError::new(
                "timeout",
                format!("timed out after {timeout}ms waiting for history navigation"),
            )
            .retryable());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_new_loader(
    browser: &mut Browser,
    previous_loader: &str,
    timeout: u64,
) -> BrowserResult<()> {
    let deadline = Instant::now() + Duration::from_millis(timeout);
    loop {
        let loader = browser.current_loader_id()?;
        if !loader.is_empty() && loader != previous_loader {
            return browser.wait_for_loader(&loader, timeout);
        }
        if Instant::now() >= deadline {
            return Err(BrowserError::new(
                "timeout",
                format!("timed out after {timeout}ms waiting for the new document"),
            )
            .retryable());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

enum Action {
    Click,
    Fill(String),
    Select(Vec<String>),
    Check(bool),
    Hover,
}
fn action(browser: &mut Browser, selector: &Selector, action: Action) -> BrowserResult<Value> {
    let is_click = matches!(action, Action::Click);
    let is_hover = matches!(action, Action::Hover);
    let body=match action {
        Action::Click=>"if(e.disabled)throw new Error('element_disabled');e.scrollIntoView({block:'center'});return new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>{const r=e.getBoundingClientRect();let x=r.left+r.width/2,y=r.top+r.height/2,p=e.ownerDocument.elementFromPoint(x,y);if(!r.width||!r.height)throw new Error('element_not_visible');if(p!==e&&!e.contains(p))throw new Error('element_obscured');let w=e.ownerDocument.defaultView;while(w.frameElement){const f=w.frameElement.getBoundingClientRect();x+=f.left;y+=f.top;w=w.parent}resolve({x,y})})));".into(),
        Action::Fill(v)=>format!("if(e.type==='password')throw new Error('password_field_denied');if(e.disabled||e.readOnly)throw new Error('element_disabled');const r=e.getBoundingClientRect(),style=e.ownerDocument.defaultView.getComputedStyle(e);if(!r.width||!r.height||style.display==='none'||style.visibility!=='visible')throw new Error('element_not_visible');const input=e.matches('textarea,input:not([type=hidden]):not([type=checkbox]):not([type=radio]):not([type=button]):not([type=submit]):not([type=reset]):not([type=file])');if(!input&&!e.isContentEditable)throw new Error('element_not_editable');e.focus();if(e.getRootNode().activeElement!==e)throw new Error('element_focus_failed');if(e.isContentEditable)e.textContent={0};else{{const s=Object.getOwnPropertyDescriptor(Object.getPrototypeOf(e),'value');if(s&&s.set)s.set.call(e,{0});else e.value={0}}}e.dispatchEvent(new InputEvent('input',{{bubbles:true,inputType:'insertText',data:{0}}}));e.dispatchEvent(new Event('change',{{bubbles:true}}));return {{filled:true}};",js_string(&v)),
        Action::Select(values)=>format!("if(e.tagName!=='SELECT')throw new Error('element_not_select');const v=new Set({});for(const o of e.options)o.selected=v.has(o.value)||v.has(o.text);e.dispatchEvent(new Event('input',{{bubbles:true}}));e.dispatchEvent(new Event('change',{{bubbles:true}}));return {{selected:[...e.selectedOptions].map(o=>o.value)}};",serde_json::to_string(&values).unwrap()),
        Action::Check(checked)=>format!("if(!['checkbox','radio'].includes(e.type))throw new Error('element_not_checkable');if(e.checked!=={checked})e.click();return {{checked:e.checked}};"),
        Action::Hover=>"e.scrollIntoView({block:'center'});return new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>{const r=e.getBoundingClientRect();let x=r.left+r.width/2,y=r.top+r.height/2,p=e.ownerDocument.elementFromPoint(x,y);const s=e.ownerDocument.defaultView.getComputedStyle(e);if(!r.width||!r.height||s.display==='none'||s.visibility!=='visible')throw new Error('element_not_visible');if(p!==e&&!e.contains(p))throw new Error('element_obscured');let w=e.ownerDocument.defaultView;while(w.frameElement){const f=w.frameElement.getBoundingClientRect();x+=f.left;y+=f.top;w=w.parent}resolve({x,y})})));".into(),
    };
    let result = browser
        .element_eval(selector, &body)
        .map_err(classify_page_error)?;
    if is_click || is_hover {
        let x = result["x"].as_f64().ok_or_else(|| {
            BrowserError::new("element_not_actionable", "element has no clickable center")
        })?;
        let y = result["y"].as_f64().ok_or_else(|| {
            BrowserError::new("element_not_actionable", "element has no clickable center")
        })?;
        browser.call(
            "Input.dispatchMouseEvent",
            json!({"type":"mouseMoved","x":x,"y":y}),
        )?;
        if is_hover {
            return Ok(json!({"hovered":true}));
        }
        browser.call(
            "Input.dispatchMouseEvent",
            json!({"type":"mousePressed","x":x,"y":y,"button":"left","clickCount":1}),
        )?;
        browser.send_no_wait(
            "Input.dispatchMouseEvent",
            json!({"type":"mouseReleased","x":x,"y":y,"button":"left","clickCount":1}),
        )?;
        browser.set_suppress_url();
        return Ok(json!({"clicked":true}));
    }
    Ok(result)
}
fn type_text(
    browser: &mut Browser,
    selector: &Selector,
    value: &str,
    delay: u64,
) -> BrowserResult<Value> {
    browser.element_eval(selector,"if(e.type==='password')throw new Error('password_field_denied');if(e.disabled||e.readOnly)throw new Error('element_disabled');const r=e.getBoundingClientRect(),s=e.ownerDocument.defaultView.getComputedStyle(e);if(!r.width||!r.height||s.display==='none'||s.visibility!=='visible')throw new Error('element_not_visible');const input=e.matches('textarea,input:not([type=hidden]):not([type=checkbox]):not([type=radio]):not([type=button]):not([type=submit]):not([type=reset]):not([type=file])');if(!input&&!e.isContentEditable)throw new Error('element_not_editable');e.focus();if(e.getRootNode().activeElement!==e)throw new Error('element_focus_failed');return true;").map_err(classify_page_error)?;
    for ch in value.chars() {
        browser.call("Input.insertText", json!({"text":ch.to_string()}))?;
        if delay > 0 {
            thread::sleep(Duration::from_millis(delay));
        }
    }
    Ok(json!({"typed":true,"characters":value.chars().count()}))
}
fn press(browser: &mut Browser, selector: Option<&Selector>, key: &str) -> BrowserResult<Value> {
    if let Some(selector) = selector {
        browser.element_eval(selector, "e.focus();return true;")?;
    }
    browser.call(
        "Input.dispatchKeyEvent",
        json!({"type":"keyDown","key":key}),
    )?;
    browser.call("Input.dispatchKeyEvent", json!({"type":"keyUp","key":key}))?;
    Ok(json!({"pressed":key}))
}
fn scroll(
    browser: &mut Browser,
    selector: Option<&Selector>,
    x: i64,
    y: i64,
) -> BrowserResult<Value> {
    match selector {
        Some(s) => {
            browser.element_eval(s, &format!("e.scrollBy({x},{y});return {{scrolled:true}};"))
        }
        None => browser.evaluate(&format!("scrollBy({x},{y});({{scrolled:true}})")),
    }
}
fn get(
    browser: &mut Browser,
    kind: GetKind,
    selector: Option<&Selector>,
    attribute: Option<&str>,
) -> BrowserResult<Value> {
    match kind {
        GetKind::Title => Ok(json!({"title":browser.evaluate("document.title")?})),
        GetKind::Url => Ok(json!({"url":browser.url()?})),
        GetKind::Count => {
            let s = selector.ok_or_else(|| {
                BrowserError::new("selector_required", "count requires a selector")
            })?;
            let script = format!("({}).length", Browser::selector_candidates_script(s));
            Ok(json!({"count":browser.evaluate(&script)?}))
        }
        _ => {
            let s = selector.ok_or_else(|| {
                BrowserError::new(
                    "selector_required",
                    "this get operation requires a selector",
                )
            })?;
            let body=match kind{GetKind::Text=>"return {text:(e.innerText||e.textContent||'').trim()};".into(),GetKind::Html=>"return {html:e.outerHTML};".into(),GetKind::Value=>"if(e.type==='password')return {value:null,redacted:true};return {value:e.value};".into(),GetKind::Attr=>format!("return {{attribute:{0},value:e.getAttribute({0})}};",js_string(attribute.ok_or_else(||BrowserError::new("attribute_required","get attr requires --attribute"))?)),_=>unreachable!()};
            browser.element_eval(s, &body)
        }
    }
}
fn wait(
    browser: &mut Browser,
    selector: Option<&Selector>,
    text: Option<&str>,
    url: Option<&str>,
    load: Option<LoadState>,
    hidden: bool,
    timeout: u64,
) -> BrowserResult<Value> {
    let deadline = Instant::now() + Duration::from_millis(timeout);
    if matches!(load, Some(LoadState::Networkidle)) {
        browser.reset_network_idle_window();
    }
    loop {
        let matched = if let Some(s) = selector {
            browser
                .evaluate(&format!(
                    "({}).some(e=>{{const r=e.getBoundingClientRect(),s=e.ownerDocument.defaultView.getComputedStyle(e);return e.isConnected&&r.width>0&&r.height>0&&s.display!=='none'&&s.visibility==='visible'}})",
                    Browser::selector_candidates_script(s)
                ))?
                .as_bool()
                .unwrap_or(false)
                != hidden
        } else if let Some(t) = text {
            browser
                .evaluate(&format!(
                    "document.body&&document.body.innerText.includes({})",
                    js_string(t)
                ))?
                .as_bool()
                .unwrap_or(false)
        } else if let Some(u) = url {
            browser.url()?.contains(u)
        } else if let Some(load) = load {
            let ready = browser
                .evaluate("document.readyState")?
                .as_str()
                .unwrap_or("")
                .to_owned();
            match load {
                LoadState::Load => ready == "complete",
                LoadState::Domcontentloaded => matches!(ready.as_str(), "interactive" | "complete"),
                LoadState::Networkidle => {
                    ready == "complete" && browser.network_idle_for(Duration::from_millis(500))
                }
            }
        } else {
            return Err(BrowserError::new(
                "condition_required",
                "wait requires a selector, --text, --url, or --load",
            ));
        };
        if matched {
            return Ok(json!({"matched":true}));
        }
        if Instant::now() >= deadline {
            return Err(
                BrowserError::new("timeout", format!("timed out after {timeout}ms")).retryable(),
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}
fn screenshot(browser: &mut Browser, output: &str, full_page: bool) -> BrowserResult<Value> {
    let params = if full_page {
        let m = browser.call("Page.getLayoutMetrics", json!({}))?;
        let c = &m["cssContentSize"];
        json!({"format":"png","captureBeyondViewport":true,"clip":{"x":0,"y":0,"width":c["width"],"height":c["height"],"scale":1}})
    } else {
        json!({"format":"png"})
    };
    let data = browser.call("Page.captureScreenshot", params)?["data"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let bytes = STANDARD
        .decode(data)
        .map_err(|e| BrowserError::new("screenshot_failed", e.to_string()))?;
    if let Some(parent) = std::path::Path::new(output)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    write_private_file(Path::new(output), &bytes)?;
    Ok(json!({"path":output,"bytes":bytes.len()}))
}
fn write_private_file(path: &Path, bytes: &[u8]) -> BrowserResult<()> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(io_error)
}
fn prepare_download_directory(path: &Path) -> BrowserResult<()> {
    if path.exists() {
        if path.is_dir() {
            return Ok(());
        }
        return Err(BrowserError::new(
            "invalid_output_path",
            format!("download output {} is not a directory", path.display()),
        ));
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    match std::fs::create_dir(path) {
        Ok(()) => {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .map_err(io_error)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {
            // Another caller won the race. The directory is caller-owned, so
            // preserve its permissions exactly as for any pre-existing path.
        }
        Err(error) => return Err(io_error(error)),
    }
    Ok(())
}
fn resolve_tab(tabs: &[Value], target: &str) -> BrowserResult<String> {
    if let Ok(index) = target.parse::<usize>() {
        return tabs
            .get(index.saturating_sub(1))
            .and_then(|t| t["targetId"].as_str())
            .map(str::to_owned)
            .ok_or_else(|| BrowserError::new("tab_gone", format!("tab {target} does not exist")));
    }
    tabs.iter()
        .find(|t| {
            t["targetId"] == target || t["title"].as_str().is_some_and(|v| v.contains(target))
        })
        .and_then(|t| t["targetId"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| BrowserError::new("tab_gone", format!("no tab matched {target:?}")))
}
fn classify_page_error(mut error: BrowserError) -> BrowserError {
    for (needle, code, message) in [
        (
            "password_field_denied",
            "password_field_denied",
            "agents cannot write passwords, passkeys, or one-time codes; hand the visible browser to the user",
        ),
        (
            "element_disabled",
            "element_not_actionable",
            "element is disabled",
        ),
        (
            "element_obscured",
            "element_not_actionable",
            "element is obscured",
        ),
        (
            "element_not_visible",
            "element_not_actionable",
            "element is not visible",
        ),
        (
            "element_not_editable",
            "element_not_actionable",
            "element is not editable",
        ),
        (
            "element_focus_failed",
            "element_not_actionable",
            "element could not receive focus",
        ),
        (
            "element_detached",
            "element_stale",
            "element is detached; take a fresh snapshot",
        ),
    ] {
        if error.message.contains(needle) {
            error.code = code.into();
            error.message = message.into();
        }
    }
    error
}
fn io_error(e: std::io::Error) -> BrowserError {
    BrowserError::new("browser_io_error", e.to_string())
}
fn protocol(e: impl std::fmt::Display) -> BrowserError {
    BrowserError::new("protocol_error", e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    #[ignore = "requires local Unix socket capability"]
    fn daemon_protocol_survives_separate_status_and_close_requests() {
        let profile = format!("test_{}", std::process::id());
        let options = DaemonOptions {
            profile: profile.clone(),
        };
        let handle = std::thread::spawn(move || run_daemon(options));
        wait_for_socket(&profile_paths(&profile).unwrap().socket).unwrap();
        let client = Client::new(&profile).unwrap();
        let status = client.request(Command::Status).unwrap();
        assert!(status.ok);
        assert_eq!(status.result.unwrap()["running"], false);
        let closed = client.request(Command::Close).unwrap();
        assert!(closed.ok);
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn existing_download_directories_keep_their_permissions() {
        let path = std::env::temp_dir().join(format!(
            "desktop-browser-existing-download-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        prepare_download_directory(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
        std::fs::remove_dir(&path).unwrap();
    }

    #[test]
    fn newly_created_download_directories_are_private() {
        let path = std::env::temp_dir().join(format!(
            "desktop-browser-new-download-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        prepare_download_directory(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::remove_dir(&path).unwrap();
    }

    #[test]
    fn overwriting_a_screenshot_tightens_existing_permissions() {
        let path = std::env::temp_dir().join(format!(
            "desktop-browser-existing-screenshot-{}",
            std::process::id()
        ));
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private_file(&path, b"private image").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"private image");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reopening_a_profile_refuses_mode_changes_and_external_attachments() {
        assert!(validate_open_mode(true, true, true).is_ok());
        assert_eq!(
            validate_open_mode(true, true, false).unwrap_err().code,
            "browser_mode_mismatch"
        );
        assert_eq!(
            validate_open_mode(false, false, false).unwrap_err().code,
            "browser_already_running"
        );
    }
}
