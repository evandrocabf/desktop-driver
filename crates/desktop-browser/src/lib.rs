//! Browser-native automation over the Chrome DevTools Protocol.
//!
//! The public API is intentionally a small request/response protocol. The CLI
//! starts one daemon per browser profile; that daemon owns Chromium and keeps
//! CDP state alive between otherwise short-lived `desktop` invocations.

#![forbid(unsafe_code)]

mod cdp;
mod daemon;
mod model;
mod paths;

pub use daemon::{Client, DaemonOptions, run_daemon, spawn_daemon};
pub use model::{
    BrowserError, BrowserResult, Command, GetKind, LoadState, Request, Response, Selector,
};
pub use paths::{
    browser_executable, installed_browser_path as installed_path, profile_name, profile_paths,
};

pub fn daemon_wait(profile: &str) -> BrowserResult<()> {
    daemon::wait_for_socket(&profile_paths(profile)?.socket)
}
