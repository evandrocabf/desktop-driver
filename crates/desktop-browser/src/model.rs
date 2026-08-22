use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub type BrowserResult<T> = Result<T, BrowserError>;

#[derive(Clone, Debug, Deserialize, Error, Serialize)]
#[error("{message}")]
pub struct BrowserError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub remedy: Option<String>,
}

impl BrowserError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            remedy: None,
        }
    }

    pub fn remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Request {
    pub command: Command,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Open {
        url: Option<String>,
        executable: Option<String>,
        headless: bool,
    },
    Connect {
        endpoint: String,
    },
    Status,
    Close,
    Goto {
        url: String,
        timeout_ms: u64,
    },
    Back {
        timeout_ms: u64,
    },
    Forward {
        timeout_ms: u64,
    },
    Reload {
        timeout_ms: u64,
    },
    Snapshot {
        interactive: bool,
        max_nodes: usize,
    },
    Screenshot {
        output: String,
        full_page: bool,
    },
    Get {
        kind: GetKind,
        selector: Option<Selector>,
        attribute: Option<String>,
    },
    Click {
        selector: Selector,
    },
    Fill {
        selector: Selector,
        value: String,
    },
    Type {
        selector: Selector,
        value: String,
        delay_ms: u64,
    },
    Press {
        selector: Option<Selector>,
        key: String,
    },
    Select {
        selector: Selector,
        values: Vec<String>,
    },
    Check {
        selector: Selector,
        checked: bool,
    },
    Hover {
        selector: Selector,
    },
    Scroll {
        selector: Option<Selector>,
        x: i64,
        y: i64,
    },
    Download {
        selector: Selector,
        output: String,
    },
    Wait {
        selector: Option<Selector>,
        text: Option<String>,
        url: Option<String>,
        load: Option<LoadState>,
        hidden: bool,
        timeout_ms: u64,
    },
    TabList,
    TabNew {
        url: Option<String>,
    },
    TabUse {
        target: String,
    },
    TabClose {
        target: Option<String>,
    },
    Dialog {
        accept: bool,
        prompt_text: Option<String>,
    },
}

impl Command {
    pub fn mutates(&self) -> bool {
        !matches!(
            self,
            Self::Status
                | Self::Snapshot { .. }
                | Self::Screenshot { .. }
                | Self::Get { .. }
                | Self::TabList
                | Self::Wait { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GetKind {
    Text,
    Html,
    Value,
    Attr,
    Title,
    Url,
    Count,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadState {
    Load,
    Domcontentloaded,
    Networkidle,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "by", content = "value", rename_all = "snake_case")]
pub enum Selector {
    Ref(String),
    Css(String),
    XPath(String),
    Text(String),
    Role { role: String, name: Option<String> },
    Label(String),
    TestId(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Response {
    pub ok: bool,
    pub profile: String,
    pub tab_id: Option<String>,
    pub document_id: Option<u64>,
    pub url: Option<String>,
    pub result: Option<Value>,
    pub error: Option<BrowserError>,
}

impl Response {
    pub fn success(profile: &str, result: Value) -> Self {
        Self {
            ok: true,
            profile: profile.into(),
            tab_id: None,
            document_id: None,
            url: None,
            result: Some(result),
            error: None,
        }
    }
    pub fn failure(profile: &str, error: BrowserError) -> Self {
        Self {
            ok: false,
            profile: profile.into(),
            tab_id: None,
            document_id: None,
            url: None,
            result: None,
            error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_is_read_only_and_actions_are_not() {
        assert!(!Command::Status.mutates());
        assert!(
            !Command::Snapshot {
                interactive: true,
                max_nodes: 20
            }
            .mutates()
        );
        assert!(
            Command::Click {
                selector: Selector::Ref("@e1".into())
            }
            .mutates()
        );
    }

    #[test]
    fn browser_errors_have_agent_branching_fields() {
        let error = BrowserError::new("tab_gone", "gone")
            .retryable()
            .remedy("list tabs");
        let value = serde_json::to_value(error).unwrap();
        assert_eq!(value["code"], "tab_gone");
        assert_eq!(value["retryable"], true);
        assert_eq!(value["remedy"], "list tabs");
    }
}
