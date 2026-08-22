use std::{
    collections::HashSet,
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};
use tungstenite::{Message, WebSocket, connect, stream::MaybeTlsStream};

use crate::{BrowserError, BrowserResult, Selector};

pub struct Browser {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    pub child: Option<Child>,
    pub target_id: String,
    pub document_id: u64,
    pub dialog_open: bool,
    pub suppress_url_once: bool,
    inflight_requests: HashSet<String>,
    network_idle_since: Option<Instant>,
}

impl Browser {
    pub fn launch(executable: &Path, profile: &Path, headless: bool) -> BrowserResult<Self> {
        std::fs::create_dir_all(profile).map_err(io_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(profile, std::fs::Permissions::from_mode(0o700))
                .map_err(io_error)?;
        }
        let port = TcpListener::bind(("127.0.0.1", 0))
            .map_err(io_error)?
            .local_addr()
            .map_err(io_error)?
            .port();
        let mut command = Command::new(executable);
        command
            .args(["--no-remote", "--profile"])
            .arg(profile)
            .arg("--remote-debugging-port")
            .arg(port.to_string())
            .arg("about:blank");
        if headless {
            command.arg("--headless");
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|error| {
            BrowserError::new(
                "browser_launch_failed",
                format!("could not launch {}: {error}", executable.display()),
            )
        })?;
        let endpoint = format!("ws://127.0.0.1:{port}/session");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Self::connect(&endpoint) {
                Ok(mut browser) => {
                    browser.child = Some(child);
                    return Ok(browser);
                }
                Err(_) if Instant::now() < deadline => {
                    if child.try_wait().map_err(io_error)?.is_some() {
                        return Err(BrowserError::new(
                            "browser_launch_failed",
                            "Firefox exited before its WebDriver BiDi endpoint became ready",
                        ));
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    terminate_child(&mut child);
                    return Err(BrowserError::new(
                        "browser_launch_failed",
                        format!("Firefox WebDriver BiDi endpoint was not ready: {error}"),
                    )
                    .retryable());
                }
            }
        }
    }

    pub fn connect(endpoint: &str) -> BrowserResult<Self> {
        let endpoint = normalize_endpoint(endpoint)?;
        let (socket, _) = connect(endpoint.as_str()).map_err(|error| {
            BrowserError::new(
                "browser_disconnected",
                format!("WebDriver BiDi connection failed: {error}"),
            )
        })?;
        let mut this = Self {
            socket,
            next_id: 1,
            child: None,
            target_id: String::new(),
            document_id: 1,
            dialog_open: false,
            suppress_url_once: false,
            inflight_requests: HashSet::new(),
            network_idle_since: Some(Instant::now()),
        };
        this.command("session.new", json!({"capabilities":{}}))?;
        this.command(
            "session.subscribe",
            json!({"events":["network.beforeRequestSent","network.responseCompleted","network.fetchError","browsingContext.userPromptOpened","browsingContext.userPromptClosed"]}),
        )?;
        this.attach_first_available()?;
        Ok(this)
    }

    fn command(&mut self, method: &str, params: Value) -> BrowserResult<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.socket
            .send(Message::Text(
                json!({"id":id,"method":method,"params":params}).to_string(),
            ))
            .map_err(bidi_io)?;
        loop {
            let message = self.socket.read().map_err(bidi_io)?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text)
                .map_err(|error| BrowserError::new("bidi_protocol_error", error.to_string()))?;
            self.process_event(&value);
            if value["id"].as_u64() != Some(id) {
                continue;
            }
            if value["type"] == "error" {
                return Err(BrowserError::new(
                    "bidi_command_failed",
                    format!(
                        "{method}: {}",
                        value["message"]
                            .as_str()
                            .unwrap_or("unknown WebDriver BiDi error")
                    ),
                ));
            }
            return Ok(value.get("result").cloned().unwrap_or_else(|| json!({})));
        }
    }

    fn request_session_end(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        let _ = self.socket.send(Message::Text(
            json!({"id":id,"method":"session.end","params":{}}).to_string(),
        ));
    }

    fn process_event(&mut self, event: &Value) {
        let Some(method) = event["method"].as_str() else {
            return;
        };
        if method.starts_with("network.")
            && event["params"]["context"].as_str() != Some(self.target_id.as_str())
        {
            return;
        }
        match method {
            "network.beforeRequestSent" => {
                if let Some(id) = event
                    .pointer("/params/request/request")
                    .and_then(Value::as_str)
                {
                    self.inflight_requests.insert(id.to_owned());
                    self.network_idle_since = None;
                }
            }
            "network.responseCompleted" | "network.fetchError" => {
                if let Some(id) = event
                    .pointer("/params/request/request")
                    .and_then(Value::as_str)
                {
                    self.inflight_requests.remove(id);
                    if self.inflight_requests.is_empty() {
                        self.network_idle_since = Some(Instant::now());
                    }
                }
            }
            "browsingContext.userPromptOpened" => self.dialog_open = true,
            "browsingContext.userPromptClosed" => self.dialog_open = false,
            _ => {}
        }
    }

    pub fn evaluate(&mut self, expression: &str) -> BrowserResult<Value> {
        let context = self.target_id.clone();
        self.evaluate_in(&context, expression)
    }

    fn evaluate_in(&mut self, context: &str, expression: &str) -> BrowserResult<Value> {
        let result = self.command(
            "script.evaluate",
            json!({
                "expression": expression,
                "target":{"context":context},
                "awaitPromise":true,
                "resultOwnership":"none",
                "serializationOptions":{"maxObjectDepth":20,"maxDomDepth":0}
            }),
        )?;
        if result["type"] == "exception" {
            return Err(BrowserError::new(
                "page_script_failed",
                result
                    .pointer("/exceptionDetails/text")
                    .and_then(Value::as_str)
                    .unwrap_or("page script failed"),
            ));
        }
        remote_value(&result["result"])
    }

    pub fn element_eval(&mut self, selector: &Selector, body: &str) -> BrowserResult<Value> {
        let candidates = Self::selector_candidates_script(selector);
        self.evaluate(&format!("(()=>{{const a={candidates};if(a.length===0)throw new Error('element_not_found');if(a.length>1)throw new Error('ambiguous_selector:'+a.length);const e=a[0];if(!e.isConnected)throw new Error('element_detached');{body}}})()"))
            .map_err(|mut error| {
                if error.message.contains("element_not_found") {
                    error.code = "element_not_found".into();
                    error.message =
                        "no element matched the selector; take a fresh snapshot".into();
                    error.retryable = true;
                } else if error.message.contains("ambiguous_selector") {
                    error.code = "ambiguous_selector".into();
                    error.message =
                        "more than one element matched; refine the selector or use a snapshot ref"
                            .into();
                }
                error
            })
    }

    pub fn selector_candidates_script(selector: &Selector) -> String {
        crate::cdp::Browser::selector_candidates_script(selector)
    }

    pub fn url(&mut self) -> BrowserResult<String> {
        Ok(self
            .evaluate("location.href")?
            .as_str()
            .unwrap_or_default()
            .to_owned())
    }

    pub fn wait_ready(&mut self, timeout_ms: u64) -> BrowserResult<()> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if self
                .evaluate("document.readyState")?
                .as_str()
                .is_some_and(|state| matches!(state, "interactive" | "complete"))
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(timeout(timeout_ms, "the document"));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn current_loader_id(&mut self) -> BrowserResult<String> {
        Ok(self.document_id.to_string())
    }

    pub fn wait_for_loader(&mut self, _loader_id: &str, timeout_ms: u64) -> BrowserResult<()> {
        self.wait_ready(timeout_ms)
    }

    pub fn reset_network_idle_window(&mut self) {
        self.drain_events();
        self.network_idle_since = self.inflight_requests.is_empty().then(Instant::now);
    }

    pub fn network_idle_for(&mut self, duration: Duration) -> bool {
        self.drain_events();
        self.inflight_requests.is_empty()
            && self
                .network_idle_since
                .is_some_and(|since| since.elapsed() >= duration)
    }

    fn drain_events(&mut self) {
        if let MaybeTlsStream::Plain(stream) = self.socket.get_mut() {
            let _ = stream.set_nonblocking(true);
        }
        loop {
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    if let Ok(value) = serde_json::from_str(&text) {
                        self.process_event(&value);
                    }
                }
                Err(tungstenite::Error::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    break;
                }
                Err(_) => break,
                _ => {}
            }
        }
        if let MaybeTlsStream::Plain(stream) = self.socket.get_mut() {
            let _ = stream.set_nonblocking(false);
        }
    }

    pub fn tabs(&mut self) -> BrowserResult<Vec<Value>> {
        let result = self.command("browsingContext.getTree", json!({"maxDepth":0}))?;
        let contexts = result["contexts"].as_array().cloned().unwrap_or_default();
        let mut tabs = Vec::with_capacity(contexts.len());
        for tab in contexts {
            let id = tab["context"].as_str().unwrap_or_default();
            let title = self
                .evaluate_in(id, "document.title")
                .unwrap_or(Value::Null);
            tabs.push(json!({"targetId":id,"title":title,"url":tab["url"]}));
        }
        Ok(tabs)
    }

    pub fn attach(&mut self, target: &str) -> BrowserResult<()> {
        self.command("browsingContext.activate", json!({"context":target}))?;
        self.target_id = target.to_owned();
        self.inflight_requests.clear();
        self.network_idle_since = Some(Instant::now());
        Ok(())
    }

    pub fn attach_first_available(&mut self) -> BrowserResult<()> {
        let id = self
            .tabs()?
            .first()
            .and_then(|tab| tab["targetId"].as_str())
            .ok_or_else(|| BrowserError::new("tab_gone", "no tabs remain"))?
            .to_owned();
        self.attach(&id)
    }

    pub fn call(&mut self, method: &str, params: Value) -> BrowserResult<Value> {
        match method {
            "Page.navigate" => {
                let context = self.target_id.clone();
                self.command(
                    "browsingContext.navigate",
                    json!({"context":context,"url":params["url"],"wait":"interactive"}),
                )
            }
            "Page.reload" => {
                let context = self.target_id.clone();
                self.command(
                    "browsingContext.reload",
                    json!({"context":context,"wait":"interactive"}),
                )
            }
            "Page.handleJavaScriptDialog" => {
                let context = self.target_id.clone();
                let mut bidi = json!({"context":context,"accept":params["accept"]});
                if let Some(text) = params.get("promptText") {
                    bidi["userText"] = text.clone();
                }
                self.command("browsingContext.handleUserPrompt", bidi)
            }
            "Page.getLayoutMetrics" => Ok(
                json!({"contentSize":{"width":self.evaluate("document.documentElement.scrollWidth")?,"height":self.evaluate("document.documentElement.scrollHeight")?}}),
            ),
            "Page.captureScreenshot" => {
                let context = self.target_id.clone();
                let origin = if params.get("clip").is_some() {
                    "document"
                } else {
                    "viewport"
                };
                self.command(
                    "browsingContext.captureScreenshot",
                    json!({"context":context,"origin":origin}),
                )
            }
            "Input.dispatchMouseEvent" => self.mouse(&params),
            "Input.insertText" => self.type_chars(params["text"].as_str().unwrap_or("")),
            "Input.dispatchKeyEvent" => {
                if params["type"] == "keyDown" {
                    self.key(params["key"].as_str().unwrap_or(""))
                } else {
                    Ok(json!({}))
                }
            }
            _ => Err(BrowserError::new(
                "bidi_unsupported",
                format!("Firefox does not support internal operation {method}"),
            )),
        }
    }

    pub fn send_no_wait(&mut self, method: &str, params: Value) -> BrowserResult<()> {
        self.call(method, params).map(drop)
    }

    pub fn call_browser(&mut self, method: &str, params: Value) -> BrowserResult<Value> {
        match method {
            "Target.createTarget" => {
                let result = self.command("browsingContext.create", json!({"type":"tab"}))?;
                let id = result["context"].as_str().unwrap_or_default().to_owned();
                self.attach(&id)?;
                if let Some(url) = params["url"].as_str().filter(|url| *url != "about:blank") {
                    self.call("Page.navigate", json!({"url":url}))?;
                }
                Ok(json!({"targetId":id}))
            }
            "Target.activateTarget" => {
                self.attach(params["targetId"].as_str().unwrap_or_default())?;
                Ok(json!({}))
            }
            "Target.closeTarget" => self.command(
                "browsingContext.close",
                json!({"context":params["targetId"]}),
            ),
            "Browser.setDownloadBehavior" => self.command(
                "browser.setDownloadBehavior",
                json!({
                    "downloadBehavior":{"type":"allowed","destinationFolder":params["downloadPath"]}
                }),
            ),
            _ => Err(BrowserError::new(
                "bidi_unsupported",
                format!("Firefox does not support internal operation {method}"),
            )),
        }
    }

    fn mouse(&mut self, params: &Value) -> BrowserResult<Value> {
        let context = self.target_id.clone();
        let x = params["x"].as_f64().unwrap_or(0.0).round() as i64;
        let y = params["y"].as_f64().unwrap_or(0.0).round() as i64;
        let action = match params["type"].as_str() {
            Some("mouseMoved") => {
                json!({"type":"pointerMove","x":x,"y":y,"duration":0,"origin":"viewport"})
            }
            Some("mousePressed") => json!({"type":"pointerDown","button":0}),
            Some("mouseReleased") => json!({"type":"pointerUp","button":0}),
            _ => return Ok(json!({})),
        };
        self.command("input.performActions", json!({"context":context,"actions":[{"type":"pointer","id":"mouse","parameters":{"pointerType":"mouse"},"actions":[action]}]}))
    }

    fn type_chars(&mut self, text: &str) -> BrowserResult<Value> {
        let context = self.target_id.clone();
        let actions: Vec<Value> = text
            .chars()
            .flat_map(|ch| {
                [
                    json!({"type":"keyDown","value":ch.to_string()}),
                    json!({"type":"keyUp","value":ch.to_string()}),
                ]
            })
            .collect();
        self.command(
            "input.performActions",
            json!({"context":context,"actions":[{"type":"key","id":"keyboard","actions":actions}]}),
        )
    }

    fn key(&mut self, key: &str) -> BrowserResult<Value> {
        let value = webdriver_key(key);
        self.type_chars(value)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // Do not wait indefinitely for a shutdown response from a browser that
        // may already be unhealthy. The owned process still receives a grace
        // period before TERM and, finally, KILL.
        self.request_session_end();
        if let Some(child) = self.child.as_mut() {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            terminate_child(child);
        }
    }
}

fn remote_value(value: &Value) -> BrowserResult<Value> {
    Ok(match value["type"].as_str().unwrap_or("undefined") {
        "undefined" | "null" => Value::Null,
        "string" | "number" | "boolean" => value.get("value").cloned().unwrap_or(Value::Null),
        "bigint" => Value::String(value["value"].as_str().unwrap_or_default().to_owned()),
        "array" => Value::Array(
            value["value"]
                .as_array()
                .into_iter()
                .flatten()
                .map(remote_value)
                .collect::<BrowserResult<_>>()?,
        ),
        "object" => {
            let mut object = Map::new();
            for pair in value["value"].as_array().into_iter().flatten() {
                let Some(items) = pair.as_array() else {
                    continue;
                };
                if items.len() == 2 {
                    let key = items[0].as_str().map(str::to_owned).unwrap_or_else(|| {
                        remote_value(&items[0])
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default()
                    });
                    object.insert(key, remote_value(&items[1])?);
                }
            }
            Value::Object(object)
        }
        other => {
            return Err(BrowserError::new(
                "bidi_protocol_error",
                format!("unsupported remote value type {other}"),
            ));
        }
    })
}

fn normalize_endpoint(endpoint: &str) -> BrowserResult<String> {
    let mut value = endpoint.trim().trim_end_matches('/').to_owned();
    if let Some(rest) = value.strip_prefix("http://") {
        value = format!("ws://{rest}");
    }
    if !value.starts_with("ws://") {
        return Err(BrowserError::new(
            "invalid_endpoint",
            "Firefox BiDi endpoint must use ws:// or http://",
        ));
    }
    let authority = value
        .trim_start_matches("ws://")
        .split('/')
        .next()
        .unwrap_or_default();
    let host = authority
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(authority)
        .trim_matches(['[', ']']);
    if !matches!(host, "127.0.0.1" | "localhost" | "::1") || authority.contains('@') {
        return Err(BrowserError::new(
            "endpoint_denied",
            "only loopback WebDriver BiDi endpoints are allowed",
        ));
    }
    if !value.ends_with("/session") {
        value.push_str("/session");
    }
    Ok(value)
}

fn webdriver_key(key: &str) -> &str {
    match key.to_ascii_lowercase().as_str() {
        "enter" => "\u{e007}",
        "tab" => "\u{e004}",
        "escape" | "esc" => "\u{e00c}",
        "backspace" => "\u{e003}",
        "delete" => "\u{e017}",
        "arrowup" => "\u{e013}",
        "arrowdown" => "\u{e015}",
        "arrowleft" => "\u{e012}",
        "arrowright" => "\u{e014}",
        _ => key,
    }
}

fn timeout(ms: u64, what: &str) -> BrowserError {
    BrowserError::new(
        "timeout",
        format!("timed out after {ms}ms waiting for {what}"),
    )
    .retryable()
}
fn bidi_io(error: tungstenite::Error) -> BrowserError {
    BrowserError::new(
        "browser_disconnected",
        format!("WebDriver BiDi connection failed: {error}"),
    )
}
fn io_error(error: std::io::Error) -> BrowserError {
    BrowserError::new("browser_io", error.to_string())
}
fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = rustix::process::kill_process(
            rustix::process::Pid::from_child(child),
            rustix::process::Signal::TERM,
        );
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_loopback_and_gain_session_path() {
        assert_eq!(
            normalize_endpoint("http://127.0.0.1:9222").unwrap(),
            "ws://127.0.0.1:9222/session"
        );
        assert!(normalize_endpoint("ws://example.com:9222/session").is_err());
    }

    #[test]
    fn converts_nested_remote_values() {
        let value = json!({"type":"object","value":[["name",{"type":"string","value":"fox"}],["items",{"type":"array","value":[{"type":"number","value":2}]}]]});
        assert_eq!(
            remote_value(&value).unwrap(),
            json!({"name":"fox","items":[2]})
        );
    }
}
