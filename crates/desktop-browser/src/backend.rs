use std::{path::Path, time::Duration};

use serde_json::Value;

use crate::{BrowserEngine, BrowserResult, Selector, bidi, cdp};

pub enum Browser {
    Chromium(cdp::Browser),
    Firefox(bidi::Browser),
}

macro_rules! delegate_mut {
    ($self:ident, $method:ident ( $($arg:expr),* )) => {
        match $self {
            Self::Chromium(browser) => browser.$method($($arg),*),
            Self::Firefox(browser) => browser.$method($($arg),*),
        }
    };
}

impl Browser {
    pub fn launch(
        engine: BrowserEngine,
        executable: &Path,
        profile: &Path,
        headless: bool,
    ) -> BrowserResult<Self> {
        match engine {
            BrowserEngine::Chromium => {
                cdp::Browser::launch(executable, profile, headless).map(Self::Chromium)
            }
            BrowserEngine::Firefox => {
                bidi::Browser::launch(executable, profile, headless).map(Self::Firefox)
            }
        }
    }
    pub fn connect(engine: BrowserEngine, endpoint: &str) -> BrowserResult<Self> {
        match engine {
            BrowserEngine::Chromium => cdp::Browser::connect(endpoint).map(Self::Chromium),
            BrowserEngine::Firefox => bidi::Browser::connect(endpoint).map(Self::Firefox),
        }
    }
    pub fn engine(&self) -> BrowserEngine {
        match self {
            Self::Chromium(_) => BrowserEngine::Chromium,
            Self::Firefox(_) => BrowserEngine::Firefox,
        }
    }
    pub fn owned(&self) -> bool {
        match self {
            Self::Chromium(b) => b.child.is_some(),
            Self::Firefox(b) => b.child.is_some(),
        }
    }
    pub fn target_id(&self) -> &str {
        match self {
            Self::Chromium(b) => &b.target_id,
            Self::Firefox(b) => &b.target_id,
        }
    }
    pub fn document_id(&self) -> u64 {
        match self {
            Self::Chromium(b) => b.document_id,
            Self::Firefox(b) => b.document_id,
        }
    }
    pub fn bump_document(&mut self) {
        match self {
            Self::Chromium(b) => b.document_id += 1,
            Self::Firefox(b) => b.document_id += 1,
        }
    }
    pub fn dialog_open(&self) -> bool {
        match self {
            Self::Chromium(b) => b.dialog_open,
            Self::Firefox(b) => b.dialog_open,
        }
    }
    pub fn set_dialog_open(&mut self, value: bool) {
        match self {
            Self::Chromium(b) => b.dialog_open = value,
            Self::Firefox(b) => b.dialog_open = value,
        }
    }
    pub fn take_suppress_url(&mut self) -> bool {
        match self {
            Self::Chromium(b) => std::mem::take(&mut b.suppress_url_once),
            Self::Firefox(b) => std::mem::take(&mut b.suppress_url_once),
        }
    }
    pub fn set_suppress_url(&mut self) {
        match self {
            Self::Chromium(b) => b.suppress_url_once = true,
            Self::Firefox(b) => b.suppress_url_once = true,
        }
    }
    pub fn evaluate(&mut self, expression: &str) -> BrowserResult<Value> {
        delegate_mut!(self, evaluate(expression))
    }
    pub fn element_eval(&mut self, selector: &Selector, body: &str) -> BrowserResult<Value> {
        delegate_mut!(self, element_eval(selector, body))
    }
    pub fn url(&mut self) -> BrowserResult<String> {
        delegate_mut!(self, url())
    }
    pub fn wait_ready(&mut self, timeout: u64) -> BrowserResult<()> {
        delegate_mut!(self, wait_ready(timeout))
    }
    pub fn current_loader_id(&mut self) -> BrowserResult<String> {
        delegate_mut!(self, current_loader_id())
    }
    pub fn wait_for_loader(&mut self, loader: &str, timeout: u64) -> BrowserResult<()> {
        delegate_mut!(self, wait_for_loader(loader, timeout))
    }
    pub fn reset_network_idle_window(&mut self) {
        match self {
            Self::Chromium(b) => b.reset_network_idle_window(),
            Self::Firefox(b) => b.reset_network_idle_window(),
        }
    }
    pub fn network_idle_for(&mut self, duration: Duration) -> bool {
        match self {
            Self::Chromium(b) => b.network_idle_for(duration),
            Self::Firefox(b) => b.network_idle_for(duration),
        }
    }
    pub fn tabs(&mut self) -> BrowserResult<Vec<Value>> {
        delegate_mut!(self, tabs())
    }
    pub fn attach(&mut self, target: &str) -> BrowserResult<()> {
        delegate_mut!(self, attach(target))
    }
    pub fn attach_first_available(&mut self) -> BrowserResult<()> {
        match self {
            Self::Chromium(b) => {
                let tabs = b.tabs()?;
                let id = tabs
                    .first()
                    .and_then(|t| t["targetId"].as_str())
                    .ok_or_else(|| crate::BrowserError::new("tab_gone", "no tabs remain"))?
                    .to_owned();
                b.attach(&id)
            }
            Self::Firefox(b) => b.attach_first_available(),
        }
    }
    pub fn call(&mut self, method: &str, params: Value) -> BrowserResult<Value> {
        delegate_mut!(self, call(method, params))
    }
    pub fn call_browser(&mut self, method: &str, params: Value) -> BrowserResult<Value> {
        delegate_mut!(self, call_browser(method, params))
    }
    pub fn send_no_wait(&mut self, method: &str, params: Value) -> BrowserResult<()> {
        delegate_mut!(self, send_no_wait(method, params))
    }
    pub fn selector_candidates_script(selector: &Selector) -> String {
        cdp::Browser::selector_candidates_script(selector)
    }
}
