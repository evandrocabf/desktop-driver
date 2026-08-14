//! Where async stops.
//!
//! Both `atspi` and `ashpd` are async-only — `atspi` pins `zbus` with
//! `default-features = false`, which switches off zbus's blocking API entirely,
//! so there is no synchronous proxy to fall back on. Rather than let that
//! propagate into the core and the CLI, this module owns one process-wide
//! current-thread runtime and every adapter blocks on it.
//!
//! The runtime is shared rather than created per call because `zbus`
//! connections are bound to the runtime that created them; building a fresh
//! one for each command would reconnect to D-Bus every time.

use std::sync::OnceLock;

use desktop_core::errors::DesktopError;

static RUNTIME: OnceLock<Option<tokio::runtime::Runtime>> = OnceLock::new();

fn runtime() -> Option<&'static tokio::runtime::Runtime> {
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .ok()
        })
        .as_ref()
}

/// Runs a future to completion, panicking only if the runtime itself could not
/// be built — which means the process has no usable I/O reactor and nothing
/// else would work either.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    runtime()
        .expect("tokio current-thread runtime is available")
        .block_on(future)
}

/// Runs a future, converting a runtime failure into a structured error rather
/// than a panic. Used on paths that can report a backend problem.
pub fn try_block_on<F: std::future::Future>(future: F) -> Result<F::Output, DesktopError> {
    let runtime = runtime().ok_or_else(|| {
        DesktopError::backend("could not start an async runtime for D-Bus communication")
    })?;
    Ok(runtime.block_on(future))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_is_reused_across_calls_rather_than_rebuilt() {
        let first = block_on(async { 1 + 1 });
        let second = block_on(async { 2 + 2 });
        assert_eq!(first, 2);
        assert_eq!(second, 4);
        assert!(std::ptr::eq(
            runtime().expect("runtime exists"),
            runtime().expect("runtime exists")
        ));
    }

    #[test]
    fn nested_futures_complete_normally() {
        let value = block_on(async {
            let inner = async { 21 };
            inner.await * 2
        });
        assert_eq!(value, 42);
    }
}
