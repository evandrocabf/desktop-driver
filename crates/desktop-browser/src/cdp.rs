use std::{
    io::{Read as _, Write as _},
    net::TcpStream,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tungstenite::{Message, WebSocket, connect, stream::MaybeTlsStream};

use crate::{BrowserError, BrowserResult, Selector};

pub struct Browser {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    pub child: Option<Child>,
    pub session_id: String,
    pub target_id: String,
    pub document_id: u64,
    pub dialog_open: bool,
    pub suppress_url_once: bool,
}

impl Browser {
    pub fn launch(
        executable: &std::path::Path,
        user_data: &std::path::Path,
        headless: bool,
    ) -> BrowserResult<Self> {
        std::fs::create_dir_all(user_data).map_err(io_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(user_data, std::fs::Permissions::from_mode(0o700))
                .map_err(io_error)?;
        }
        let mut command = Command::new(executable);
        command
            .args([
                "--remote-debugging-port=0",
                "--remote-debugging-address=127.0.0.1",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                "--password-store=basic",
                "--force-renderer-accessibility",
                "about:blank",
            ])
            .arg(format!("--user-data-dir={}", user_data.display()));
        if headless {
            command.arg("--headless=new");
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().map_err(|error| {
            BrowserError::new(
                "browser_launch_failed",
                format!("could not launch {}: {error}", executable.display()),
            )
        })?;
        let active = user_data.join("DevToolsActivePort");
        let deadline = Instant::now() + Duration::from_secs(10);
        let contents = loop {
            if let Ok(contents) = std::fs::read_to_string(&active) {
                break contents;
            }
            if Instant::now() >= deadline {
                return Err(BrowserError::new(
                    "browser_launch_failed",
                    "Chromium did not publish its DevTools endpoint within 10 seconds",
                )
                .retryable());
            }
            thread::sleep(Duration::from_millis(50));
        };
        let mut lines = contents.lines();
        let port = lines.next().ok_or_else(|| {
            BrowserError::new("browser_disconnected", "invalid DevToolsActivePort")
        })?;
        let path = lines.next().ok_or_else(|| {
            BrowserError::new("browser_disconnected", "invalid DevToolsActivePort")
        })?;
        let endpoint = format!("ws://127.0.0.1:{port}{path}");
        let mut browser = Self::connect(&endpoint)?;
        browser.child = Some(child);
        Ok(browser)
    }

    pub fn connect(endpoint: &str) -> BrowserResult<Self> {
        let endpoint = resolve_endpoint(endpoint)?;
        let rest = endpoint.strip_prefix("ws://").ok_or_else(|| {
            BrowserError::new("invalid_endpoint", "CDP endpoint must be a ws:// URL")
        })?;
        let authority = rest.split('/').next().unwrap_or_default();
        if !local_authority(authority) || authority.contains('@') {
            return Err(BrowserError::new(
                "endpoint_denied",
                "v1 only connects to a loopback CDP endpoint",
            ));
        }
        let (socket, _) = connect(endpoint.as_str()).map_err(|e| {
            BrowserError::new(
                "browser_disconnected",
                format!("CDP connection failed: {e}"),
            )
        })?;
        let mut this = Self {
            socket,
            next_id: 1,
            child: None,
            session_id: String::new(),
            target_id: String::new(),
            document_id: 1,
            dialog_open: false,
            suppress_url_once: false,
        };
        this.attach_first_page()?;
        Ok(this)
    }

    fn attach_first_page(&mut self) -> BrowserResult<()> {
        let targets = self.call_browser("Target.getTargets", json!({}))?;
        let target = targets["targetInfos"]
            .as_array()
            .and_then(|items| items.iter().find(|t| t["type"] == "page"))
            .and_then(|t| t["targetId"].as_str())
            .map(str::to_owned);
        let target = match target {
            Some(id) => id,
            None => {
                self.call_browser("Target.createTarget", json!({"url":"about:blank"}))?["targetId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            }
        };
        self.attach(&target)
    }

    pub fn attach(&mut self, target: &str) -> BrowserResult<()> {
        let result = self.call_browser(
            "Target.attachToTarget",
            json!({"targetId":target,"flatten":true}),
        )?;
        self.session_id = result["sessionId"]
            .as_str()
            .ok_or_else(|| BrowserError::new("tab_gone", "could not attach to tab"))?
            .to_owned();
        self.target_id = target.to_owned();
        for method in [
            "Page.enable",
            "Runtime.enable",
            "DOM.enable",
            "Accessibility.enable",
        ] {
            self.call(method, json!({}))?;
        }
        Ok(())
    }

    pub fn call(&mut self, method: &str, params: Value) -> BrowserResult<Value> {
        self.call_inner(method, params, true)
    }
    pub fn call_browser(&mut self, method: &str, params: Value) -> BrowserResult<Value> {
        self.call_inner(method, params, false)
    }
    pub fn send_no_wait(&mut self, method: &str, params: Value) -> BrowserResult<()> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({"id":id,"method":method,"params":params});
        if !self.session_id.is_empty() {
            request["sessionId"] = json!(self.session_id);
        }
        self.socket
            .send(Message::Text(request.to_string()))
            .map_err(cdp_io)
    }
    fn call_inner(&mut self, method: &str, params: Value, session: bool) -> BrowserResult<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let completes_on_dialog = method == "Input.dispatchMouseEvent"
            && params["type"].as_str() == Some("mouseReleased");
        let mut request = json!({"id":id,"method":method,"params":params});
        if session && !self.session_id.is_empty() {
            request["sessionId"] = json!(self.session_id);
        }
        self.socket
            .send(Message::Text(request.to_string()))
            .map_err(cdp_io)?;
        loop {
            let message = self.socket.read().map_err(cdp_io)?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text)
                .map_err(|e| BrowserError::new("cdp_protocol_error", e.to_string()))?;
            if value["method"] == "Page.javascriptDialogOpening" {
                self.dialog_open = true;
                // Chromium pauses the mouseReleased response while JavaScript
                // is blocked in alert/confirm/prompt. Let the click return so
                // a separate CLI invocation can handle the pending dialog.
                if completes_on_dialog {
                    return Ok(json!({"dialogOpened": true}));
                }
            }
            if value["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(BrowserError::new(
                    "cdp_command_failed",
                    format!(
                        "{method}: {}",
                        error["message"].as_str().unwrap_or("unknown CDP error")
                    ),
                ));
            }
            return Ok(value.get("result").cloned().unwrap_or_else(|| json!({})));
        }
    }

    pub fn evaluate(&mut self, expression: &str) -> BrowserResult<Value> {
        let result = self.call("Runtime.evaluate", json!({"expression":expression,"returnByValue":true,"awaitPromise":true,"userGesture":true}))?;
        if let Some(exception) = result.get("exceptionDetails") {
            let message = exception["exception"]["description"]
                .as_str()
                .or_else(|| exception["exception"]["value"].as_str())
                .or_else(|| exception["text"].as_str())
                .unwrap_or("page script failed");
            return Err(BrowserError::new("page_script_failed", message));
        }
        Ok(result["result"]["value"].clone())
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
                .is_some_and(|s| matches!(s, "interactive" | "complete"))
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(BrowserError::new(
                    "timeout",
                    format!("timed out after {timeout_ms}ms waiting for the document"),
                )
                .retryable());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn selector_candidates_script(selector: &Selector) -> String {
        match selector {
            Selector::Ref(reference) => format!(
                "[window.__desktopDriverRefs&&window.__desktopDriverRefs.get({})].filter(Boolean)",
                js(reference)
            ),
            Selector::Css(css) => format!("[...document.querySelectorAll({})]", js(css)),
            Selector::XPath(xpath) => format!(
                "(()=>{{const r=document.evaluate({},document,null,XPathResult.ORDERED_NODE_SNAPSHOT_TYPE,null);return Array.from({{length:r.snapshotLength}},(_,i)=>r.snapshotItem(i))}})()",
                js(xpath)
            ),
            Selector::Text(text) => format!(
                "[...document.querySelectorAll('body *')].filter(e=>e.children.length===0&&e.textContent.trim().includes({}))",
                js(text)
            ),
            Selector::Label(label) => format!(
                "[...document.querySelectorAll('label')].filter(e=>e.textContent.trim()==={0}).map(l=>l.control||document.getElementById(l.htmlFor)).filter(Boolean)",
                js(label)
            ),
            Selector::TestId(id) => format!(
                "[...document.querySelectorAll('[data-testid='+CSS.escape({})+']')]",
                js(id)
            ),
            Selector::Role { role, name } => {
                let name = name.as_ref().map(|n| format!("&&((e.getAttribute('aria-label')||e.innerText||e.value||'').trim()==={})", js(n))).unwrap_or_default();
                format!(
                    "[...document.querySelectorAll('*')].filter(e=>(e.getAttribute('role')||{})==={} {})",
                    implicit_role_js(),
                    js(role),
                    name
                )
            }
        }
    }

    pub fn selector_script(selector: &Selector) -> String {
        format!("({})[0]", Self::selector_candidates_script(selector))
    }

    pub fn element_eval(&mut self, selector: &Selector, body: &str) -> BrowserResult<Value> {
        let expression = format!(
            "(()=>{{const matches={};if(matches.length===0)throw new Error('element_not_found');if(matches.length>1)throw new Error('ambiguous_selector:'+matches.length);const e=matches[0];if(!e.isConnected)throw new Error('element_detached');{} }})()",
            Self::selector_candidates_script(selector),
            body
        );
        self.evaluate(&expression).map_err(|mut error| {
            if error.message.contains("element_not_found") {
                error.code = "element_not_found".into();
                error.message = "no element matched the selector; take a fresh snapshot".into();
                error.retryable = true;
            }
            if error.message.contains("ambiguous_selector") {
                error.code = "ambiguous_selector".into();
                error.message =
                    "more than one element matched; refine the selector or use a snapshot ref"
                        .into();
            }
            error
        })
    }

    pub fn tabs(&mut self) -> BrowserResult<Vec<Value>> {
        let result = self.call_browser("Target.getTargets", json!({}))?;
        Ok(result["targetInfos"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t["type"] == "page")
            .collect())
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn js(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}
fn implicit_role_js() -> &'static str {
    "({BUTTON:'button',A:'link',INPUT:e.type==='password'?'password':e.type==='checkbox'?'checkbox':e.type==='radio'?'radio':e.type==='submit'?'button':'textbox',TEXTAREA:'textbox',SELECT:'combobox',OPTION:'option',IMG:'img',H1:'heading',H2:'heading',H3:'heading'}[e.tagName]||'')"
}
fn io_error(error: std::io::Error) -> BrowserError {
    BrowserError::new("browser_io_error", error.to_string())
}
fn cdp_io(error: tungstenite::Error) -> BrowserError {
    BrowserError::new("browser_disconnected", error.to_string()).retryable()
}

fn resolve_endpoint(endpoint: &str) -> BrowserResult<String> {
    if endpoint.starts_with("ws://") {
        return Ok(endpoint.to_owned());
    }
    let rest = endpoint.strip_prefix("http://").ok_or_else(|| {
        BrowserError::new(
            "invalid_endpoint",
            "CDP endpoint must use ws:// or loopback http://",
        )
    })?;
    let (authority, base_path) = rest.split_once('/').unwrap_or((rest, ""));
    if !local_authority(authority) || authority.contains('@') {
        return Err(BrowserError::new(
            "endpoint_denied",
            "v1 only connects to a loopback CDP endpoint",
        ));
    }
    let path = format!("{}/json/version", base_path.trim_end_matches('/'));
    let path = if path.starts_with('/') {
        path
    } else {
        format!("/{path}")
    };
    let mut stream = TcpStream::connect(authority).map_err(|error| {
        BrowserError::new(
            "browser_disconnected",
            format!("could not reach CDP HTTP endpoint: {error}"),
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )
    .map_err(io_error)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(io_error)?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| BrowserError::new("invalid_endpoint", "invalid HTTP response from CDP"))?;
    if !response.starts_with(b"HTTP/1.1 200") && !response.starts_with(b"HTTP/1.0 200") {
        return Err(BrowserError::new(
            "browser_disconnected",
            "CDP /json/version did not return HTTP 200",
        ));
    }
    let body: Value = serde_json::from_slice(&response[split + 4..])
        .map_err(|error| BrowserError::new("invalid_endpoint", error.to_string()))?;
    body["webSocketDebuggerUrl"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| BrowserError::new("invalid_endpoint", "CDP response has no WebSocket URL"))
}

fn local_authority(authority: &str) -> bool {
    authority == "localhost"
        || authority.starts_with("localhost:")
        || authority == "127.0.0.1"
        || authority.starts_with("127.0.0.1:")
        || authority == "[::1]"
        || authority.starts_with("[::1]:")
}

pub fn js_string(value: &str) -> String {
    js(value)
}
pub fn snapshot_script(interactive: bool, max_nodes: usize) -> String {
    format!(
        r#"(()=>{{
const roots=[document];for(let i=0;i<roots.length;i++){{for(const e of roots[i].querySelectorAll('*')){{if(e.shadowRoot)roots.push(e.shadowRoot);if(e.tagName==='IFRAME'){{try{{if(e.contentDocument)roots.push(e.contentDocument)}}catch{{}}}}}}}}
const query=s=>roots.flatMap(root=>[...root.querySelectorAll(s)]);
const all=query('a,button,input,textarea,select,option,[role],[contenteditable],[tabindex],summary,details');
const candidates={} ? all : query('body *');
window.__desktopDriverRefs=new Map();let out=[];let n=0;
const role=e=>e.getAttribute('role')||({{BUTTON:'button',A:'link',INPUT:e.type==='password'?'password':e.type==='checkbox'?'checkbox':e.type==='radio'?'radio':e.type==='submit'?'button':'textbox',TEXTAREA:'textbox',SELECT:'combobox',OPTION:'option',IMG:'img',H1:'heading',H2:'heading',H3:'heading'}}[e.tagName]||e.tagName.toLowerCase());
for(const e of candidates){{if(n>={})break;const r=e.getBoundingClientRect();const s=e.ownerDocument.defaultView.getComputedStyle(e);if(!r.width||!r.height||s.visibility==='hidden'||s.display==='none')continue;const ref='@e'+(++n);window.__desktopDriverRefs.set(ref,e);const label=e.labels&&[...e.labels].map(l=>l.innerText).join(' ');const name=(e.getAttribute('aria-label')||label||e.getAttribute('alt')||e.innerText||e.value||e.getAttribute('placeholder')||'').trim().replace(/\s+/g,' ').slice(0,160);out.push({{ref,role:role(e),name,value:e.matches('input,textarea,select')?(e.type==='password'?null:e.value):undefined,redacted:e.type==='password'||undefined,disabled:e.disabled||undefined,checked:e.checked===true?true:undefined}})}}
return out;
}})()"#,
        if interactive { "true" } else { "false" },
        max_nodes
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_and_css_selectors_have_distinct_resolution_rules() {
        assert!(
            Browser::selector_candidates_script(&Selector::Ref("@e7".into()))
                .contains("__desktopDriverRefs")
        );
        assert!(
            Browser::selector_candidates_script(&Selector::Css("button.save".into()))
                .contains("querySelectorAll")
        );
    }

    #[test]
    fn cdp_attachment_is_loopback_only() {
        for local in ["localhost:9222", "127.0.0.1:9222", "[::1]:9222"] {
            assert!(local_authority(local));
        }
        for remote in ["example.com:9222", "127.0.0.1.example.com:9222"] {
            assert!(!local_authority(remote));
        }
    }

    #[test]
    fn snapshot_script_redacts_password_values_in_the_page() {
        let script = snapshot_script(true, 200);
        assert!(script.contains("e.type==='password'?null:e.value"));
        assert!(script.contains("redacted:e.type==='password'"));
        assert!(script.contains("e.type==='password'?'password'"));
        assert!(script.contains("n>=200"));
    }
}
