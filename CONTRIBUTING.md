# Contributing

Thanks for looking. This document is about how the code is meant to be written, because most of the
review comments on a project like this are about the same handful of things.

## Getting set up

Rust 1.97.1, pinned in `rust-toolchain.toml`, so `rustup` picks it automatically.

```bash
cargo build
cargo test --workspace
./target/debug/desktop doctor
```

On Linux, `desktop doctor` names anything missing and prints the install command for your
distribution. Nothing extra is needed to read accessibility trees; `Xvfb`, `Xephyr`, `openbox` and
`dbus-daemon` are only needed for `desktop session`.

## Before you open a pull request

```bash
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask architecture
```

All four run in CI. The last one is this project's own rules — see below.

If you touch anything platform-specific, check the other platform too. It costs a minute and the
compiler catches most of it:

```bash
cargo clippy --target aarch64-apple-darwin --workspace --all-targets -- -D warnings
cargo clippy --target x86_64-apple-darwin  --workspace --all-targets -- -D warnings
```

## The rule that matters most

**Never report success for something that did not happen, and never invent a value you do not have.**

Almost every hard-won fix in this repository is an instance of that:

- `focus` used to return success on Wayland while doing nothing, so keystrokes went to whatever the
  user had focused. It now verifies the window actually became active, and refuses where the display
  server has no mechanism at all.
- `snapshot` used to emit `bounds: {x: 0, y: 0, …}` for toolkits that report no position, which
  aimed every coordinate click at the corner of the screen. It emits `null` now.
- A capability that cannot work is `Unsupported` with a reason, never silently absent. An
  undeclared capability fails closed for the same reason.

If you are about to write a fallback, ask whether the caller can tell it happened. If not, it is the
wrong fallback.

## How the code is laid out

```
crates/desktop-core     models, snapshot normalizer, selectors, policy, errors, the Driver,
                        and the four ports. forbid(unsafe_code); builds on any platform.
crates/desktop-browser  direct CDP transport, per-profile daemon and page automation.
crates/desktop-linux    AT-SPI, X11/XTEST/EWMH, xdg-desktop-portal, agent sessions.
crates/desktop-macos    AXUIElement, CGWindowList, ScreenCaptureKit, CGEvent.
crates/desktop-cli      the `desktop` binary.
xtask                   the architecture gates.
```

`desktop-core` must compile and test anywhere, so it may not name a platform crate or a platform
dependency. Platform backends are selected by target `cfg`, never by a Cargo feature — a feature can
be enabled on the wrong platform, and the failure would be a wrong backend rather than a link error.

The four ports (`AccessibilityPort`, `CapturePort`, `InputPort`, `PlatformProbe`) are separate
because on Linux they are separate subsystems that fail independently: the accessibility bus can be
down while screen capture works perfectly. A failure in one must not take the others with it.

## Conventions

- **Errors are typed.** `thiserror` only; no `anyhow` or `eyre`. Every error maps to a semantic exit
  code, and callers branch on those rather than parsing text.
- **Dependencies are pinned exactly** (`=1.2.3`) with `default-features = false` and a hand-picked
  feature list, declared once in `[workspace.dependencies]`.
- **`unsafe` lives only in the platform modules that wrap a C API.** `desktop-core` and
  `desktop-cli` forbid it outright; `desktop-macos` denies the lint at the crate root and grants it
  back on the four modules that call AX, ScreenCaptureKit, CGEvent and CGWindowList, so an `unsafe`
  block anywhere else is a compile error. Never re-grant it crate-wide — a crate-level `allow`
  overrides the `deny` above it and silently removes the boundary. `cargo xtask architecture` checks
  the same list, and every block carries a `// SAFETY:` comment saying which API contract makes it
  sound (most often whether a CoreFoundation reference came back +0 or +1).
- **Async stops at the adapter boundary.** `atspi` and `ashpd` are async-only, so `desktop-linux`
  owns a private current-thread runtime and blocks inside itself. Core and the CLI are synchronous.
- **Comments are rustdoc.** `///` and `//!` only. A fact about how something behaves belongs on the
  item, where `cargo doc` will show it and where a caller will actually read it — not buried in the
  body where only someone already editing that function will find it. The one exception is
  `// SAFETY:` on an `unsafe` block, which the language convention requires and which attaches to a
  block rather than an item.
- **Say why, not what.** A comment that restates the code is noise; one recording which toolkit
  misbehaves, and how that was observed, is why the next person does not have to rediscover it.
  `cargo doc --workspace --no-deps` must stay warning-free.

## Tests

In-file `#[cfg(test)] mod tests`, no `tests/` directory. Names are sentences that state the
behaviour, so a failure reads as a claim that stopped being true:

```rust
#[test]
fn a_wayland_session_is_never_mistaken_for_x11_just_because_display_is_set() { … }
```

Anything that can be a pure function of observable facts should be, so it can be tested on any
machine: backend selection, snapshot pruning, selector matching, coordinate transforms, policy.

Input is tested through `RecordedInput`, which implements `InputPort` and records calls — clicking
and typing are covered without moving the real mouse.

Live tests that drive a real application are `#[ignore]`d, and they refuse to launch anything onto
the display you are sitting in front of — an application started by a test opens over your work and
takes the keystrokes you were in the middle of typing. Give them a display of their own:

```bash
desktop session start --headless
eval "$(desktop session env)"
cargo test --workspace -- --ignored
```

`session env` rather than `session run` — `run` launches detached, so the test results never come
back.

Set `DESKTOP_DRIVER_LIVE_ON_MY_DESKTOP=1` to launch onto your own screen anyway. On macOS that is
the only option, because there is no session mechanism and the only display is yours.

`cargo xtask architecture` keeps both halves of this true: it fails if a test in `live.rs` loses its
`#[ignore]`, and if any other test spawns a process.

`scripts/distro-matrix.sh` runs the whole thing end to end on Debian, Ubuntu, Arch and openSUSE in
containers. Run it if you touch anything about how a session is built, because where distributions
put things is not something you can reason about from one machine — two of the four were broken the
first time it was run.

## Changing what agents are told

`skills/desktop-driver/SKILL.md` is the instructions an agent reads. If you change a command, a
flag, or the guidance about when to use a session, update it in the same pull request. `AGENTS.md`
is a symlink to it.

## Pull requests

Keep them focused, and say what you actually ran to verify the change. For anything touching how a
desktop is read or driven, a before and after — a snapshot, a screenshot, the exact commands — is
worth more than a description.
