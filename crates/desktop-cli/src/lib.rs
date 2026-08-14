//! The `desktop` command-line interface.
//!
//! Exposed as a library so the whole command surface can be driven in-process
//! against fake ports: `run(&cli, &driver, &sessions, &mut buffer)` is exactly
//! what `main` calls, so the tests exercise the real dispatch path without
//! spawning a process or touching a real desktop.

#![forbid(unsafe_code)]

pub mod cli;
mod live;
pub mod output;
pub mod png;
pub mod policy;
pub mod run;

pub use cli::{Cli, Command};
pub use run::run;
