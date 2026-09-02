# desktop-driver

**Playwright for desktop applications, designed for AI agents.**

A deterministic desktop automation layer for macOS and Linux. One CLI, one
vocabulary, and honest reporting of what the current environment can actually
do.

```bash
desktop snapshot --app "Calculator"
# Application: Calculator
# Window: Calculator
#
# [1] button "Backspace"
# [5] button "7"
# [23] button "+"
# [27] button "="

desktop click --role button --name "7"
# clicked [5] via action (press)
```

Every command takes `--json`.

For web pages, the same binary has a browser-native CDP/WebDriver BiDi surface:

```bash
desktop browser open https://example.com --headless
desktop browser snapshot -i
# @e1 link "More information..."
desktop browser click @e1
desktop browser wait --load domcontentloaded
desktop browser get title
```

---

## The idea

An agent that can see and drive a desktop needs three things a screenshot alone
cannot give it: a *semantic* view of the UI, stable references it can act on,
and a truthful account of what will and will not work. This tool provides all
three, and refuses to guess when it cannot.

Two design decisions follow from that.

**Element references are paths, not pointers.** `desktop snapshot` prints
`[42] button "Save"`, then `desktop click --element 42` runs as a *separate
process*. A macOS `AXUIElementRef` cannot survive that boundary, so a snapshot
records how to *find* each element again — role, name and ordinal from the
window root — and acting re-walks the live tree. An element that moved is still
found; one that was replaced is reported `element_stale` rather than clicked at
its old position.

**Acting prefers the accessibility API over synthetic input.** When an element
advertises an action, `desktop click` activates it directly instead of moving
the pointer; `desktop type --element N` writes into a field instead of sending
keystrokes. Neither touches the pointer, the keyboard focus, or the window
stack — see [Sharing a desktop with a human](#sharing-a-desktop-with-a-human).
`--via pointer` forces coordinates when you want them.

---

## Browser-native automation

`desktop browser` is the page-navigation path for Chromium and Firefox. It uses the Chrome
DevTools Protocol for Chromium and WebDriver BiDi for Firefox directly—there is no Node,
Playwright, Selenium, embedded model, MCP server, or hidden selector guessing. A small local daemon
owns the protocol connection for each profile so separate CLI calls share tabs, cookies and a
document-scoped ref map.

```bash
desktop browser doctor
desktop browser install                         # only when no Chrome/Chromium is found
desktop browser open https://example.com --headless
# Or choose Firefox once when opening the profile:
desktop browser open https://example.com --browser firefox --headless
desktop browser snapshot -i                     # compact interactive snapshot
desktop browser fill @e2 "Evandro"
desktop browser click @e3
desktop browser wait --url /complete
desktop browser get text @e4
desktop browser close
```

Targets are `@eN`, `css=...`, `xpath=...`, `text=...`, `--role/--name`, `--label`, or
`--test-id`. Actions require one strategy and one match. Clicks wait two animation frames, verify
visibility and hit testing, scroll into view, then dispatch native protocol pointer events.
Navigation and fresh snapshots replace refs; take another snapshot instead of reusing a stale page
reference.

The namespace also provides history/reload, `get text|html|value|attr|title|url|count`,
fill/type/press/select/check/hover/scroll, condition waits, screenshots, tabs, JavaScript dialog
accept/dismiss, and download-trigger clicks into an explicit directory. `--json` responses include
`profile`, `tab_id`, `document_id`, and `url`, while
errors carry a stable code, retryability, and a remedy when one is known.

On Linux a visible managed browser must run in a matching visible session:

```bash
desktop session start github --visible
desktop browser open https://github.com --profile github
```

The user enters passwords, passkeys and one-time codes directly in that visible browser. Agents
must never request or type them. Password values are redacted and browser fill/type refuses those
fields. The profile keeps cookies for later runs. Headless operation is explicit with `--headless`.
Attaching to an existing browser is loopback-only. Select Firefox for a Remote Agent endpoint:

```bash
desktop browser connect http://127.0.0.1:9222
desktop browser connect ws://127.0.0.1:9223/session --browser firefox
```

Use ordinary `desktop snapshot/click/type` for browser chrome and desktop applications. Use
`desktop browser` for page content in either supported engine.

---

## Sharing a desktop with a human

A desktop has exactly one keyboard focus, one pointer and one screen. An agent
that types "into whatever is focused" is racing the person sitting there, and
loses in both directions: keystrokes land in the wrong window, and the user's
cursor jumps mid-sentence.

The way out is to stop using the shared devices. Every element-addressed
operation goes through the accessibility API instead:

```bash
desktop snapshot --app Firefox          # works on a background window
desktop type --element 23 "x.com"       # writes into the field directly
desktop click --element 5               # activates, no pointer movement
```

`--no-steal-focus` enforces this. It permits element-addressed work and refuses
anything that would seize a shared device — `move`, `focus`, `scroll`, bare
`type` and `key`, and any click that would fall back to the pointer:

```bash
desktop --no-steal-focus type --element 23 "x.com"   # allowed
desktop --no-steal-focus key "ctrl+l"                # policy_denied, exit 3
```

Three honest limits:

- **Not every widget honours it.** Firefox's address bar returns success from
  the accessibility text API and then ignores the write. Rather than report a
  write that never happened, `set_text` reads the field back and fails with the
  reason. GTK text views, and most native controls, work.
- **Focus under Wayland is a request, not a command.** There is no
  client-initiated raise, so the only lever is asking the application to present
  itself; applications that do not speak D-Bus cannot be asked, and the
  compositor may decline. `desktop focus` verifies and refuses rather than
  reporting a change that did not happen, so keystrokes go wherever *you* last
  clicked until one succeeds.
- **Screenshots are still whole-screen.** Capture sees your windows too, which
  matters if the images go to a model. Per-window capture is the thing GNOME
  Wayland does not offer.

All three stop being problems once the agent stops sharing the desktop at all.

### Giving the agent its own display

`desktop session` starts an X server nobody is looking at, with its own window
manager, its own D-Bus and its own accessibility bus:

```bash
desktop session create github          # a durable, isolated browser profile
desktop session start github --visible # a 1920x1080 display the user can take over
desktop session run firefox            # launch something onto it
desktop snapshot                       # the agent's windows, not yours
desktop screenshot                     # pixels that contain nothing of yours
desktop session stop
```

Once a session exists, **every command addresses it by default**, and says so:

```
$ desktop apps
[agent display :90]

   451362  gnome-calculator                 1 window(s)
```

In `--json` the same fact is a `"display"` field on every document, errors
included. `--host` opts back out for a single command. The default runs this
way round on purpose: forgetting a flag should leave the agent on its own
screen, not on yours.

Inside a session all three shared devices stop being shared, so the things that
are impossible on GNOME Wayland simply work — `desktop focus` raises a window
through EWMH and verifies it, screenshots contain only what the agent started,
and the pointer is not yours. The tradeoff is that the agent can no longer see
or drive the applications *you* are running; for those, `--host` with
`--no-steal-focus` remains the safe combination.

The isolated display is X11 rather than a nested Wayland compositor because
this project's X11 backend is the complete one: `GetImage` for capture, XTEST
for input, and — the thing Wayland has no protocol for — actually raising a
window. It needs `Xvfb`, `openbox` and `dbus-daemon` installed; `desktop
doctor` names the package for your distribution, and `desktop capabilities`
reports `agent_session` as unsupported until they are there.

### You can watch it, by default

`desktop session start` renders the agent's display into a window titled `desktop-driver` on your
desktop. An agent driving your computer where you cannot see it is asking you to take its word for
it, and there is no reason to.

It is still a real, separate X server — its own framebuffer, its own pointer, its own keyboard — so
the isolation is byte-for-byte the same: the agent's input cannot reach your applications, and
`desktop screenshot` still captures only the agent's windows and never yours. What you gain is
being able to watch, and to click into that window and take over.

Verified rather than assumed: with a visible session running, an agent that focuses a window,
types twenty-five characters, clicks and scrolls leaves the rest of the screen **pixel-identical**.

Watching needs Xephyr and a desktop to open the window on. Where either is missing — a CI runner,
a headless server — the session starts anyway and *says* it cannot be watched, rather than failing
or quietly going dark:

```
Started an agent display on :90 (1920x1080).
...
You cannot watch it: Xephyr is not installed.
`desktop screenshot` is how to see what the agent sees.
```

```bash
desktop session start github --headless # opt out: a long unattended run
desktop session start github --visible  # refuse unless the user can watch and take over
```

**A session gets its own home directory too**, at
`$XDG_DATA_HOME/desktop-driver/sessions/<name>/home`, with `HOME` and the `XDG_*` directories
redirected into it. A separate display alone is not separate enough: Firefox,
Chrome and VS Code are all single-instance and coordinate through a lock file
in the profile, so an agent launching one with your `HOME` either drives *your*
window or holds the lock and leaves you unable to start your own browser at
all. Sharing a home also means the agent arrives logged in to everything you
are logged in to, and screenshots of those pages are as private as the pages.

Each named home persists between starts, so closing the display does not discard
cookies or saved logins. The legacy global home is migrated into the `default`
session on first use. `XDG_RUNTIME_DIR` is deliberately not redirected — the accessibility
socket lives there and must stay the real per-user directory.

For an initial login, start with `--visible`, launch the browser, and hand the
visible window to the user. The user enters passwords and one-time codes directly
in the browser; credentials must never be requested through the agent or model.
Afterwards, `desktop session stop` preserves the profile and
`desktop session start <name>` reuses it. `desktop session delete <name>` is the
explicit destructive operation that removes its cookies and saved logins.

`--share-home` opts out when you actually want the agent working with your own
profiles and logins:

```bash
desktop session start --share-home
```

One trap worth naming, because it fails silently. Setting `DISPLAY` is not
enough to move an application: GTK4 and Qt6 both prefer Wayland when
`WAYLAND_DISPLAY` is set, so an application launched with only `DISPLAY`
changed opens its window on *your* compositor while registering on the agent's
accessibility bus — every read looks healthy and the window is on your screen
collecting your keystrokes. `desktop session run` unsets the Wayland handles
and pins each toolkit to X11 by name. `desktop session env` prints the same
environment, `unset` lines first, for running something by hand.

---

## What works where

Run `desktop capabilities` on the machine in question — it reports this table
for your actual session. `✓` supported, `~` supported with a caveat, `✗` not
available.

| | macOS | Linux / X11 | GNOME / Wayland | wlroots, other Wayland | agent session |
|---|---|---|---|---|---|
| accessibility tree | ✓ | ✓ | ✓ | ✓ | ✓ |
| element actions | ✓ | ✓ | ✓ | ✓ | ✓ |
| applications | ✓ | ✓ | ~ | ~ | ✓ |
| windows | ✓ | ✓ | ~ | ~ | ✓ |
| screenshot (screen) | ✓ | ✓ | ~ | ~ | ✓ |
| screenshot (window) | ✓ | ✓ | ✗ | ✗ | ✓ |
| mouse / keyboard / scroll | ✓ | ✓ | ~ | ~ | ✓ |
| focus | ✓ | ✓ | ~ | ~ | ✓ |
| agent session | ✗ | ✓ | ✓ | ✓ | — |

**KDE is not on this table because it is not supported.** Every command that
would touch a KDE desktop refuses with `unsupported_capability`; see
[below](#kde-is-not-supported). `desktop session` still works there.

The accessibility tree is the one row that is green everywhere, because AT-SPI
is D-Bus and never talks to the compositor. That is also why it carries window
enumeration on Wayland, where no protocol lets a client enumerate anything else.
Under X11 the window manager knows more than AT-SPI does, so the list comes from
EWMH and AT-SPI is joined onto it for the tree: `desktop info` reports `windows:
ewmh` there and `windows: at-spi` under Wayland.

The fourth column is `~` rather than `✗` because the portals are freedesktop's
rather than GNOME's, and are selected wherever a session advertises them. What
that column does *not* have is verification: the caveat on every cell in it is
that this build has only been run against GNOME's portal backend.

The last column is the point of `desktop session`: an agent display is X11 that
nobody else is using, so every row is green there regardless of what the user's
own session can do. `agent session` itself is unavailable on macOS — there is
one window server per login and no supported way to make another.

### Why the caveats are caveats

- **Wayland input** goes through the RemoteDesktop portal. The first use shows an
  approval dialog; `desktop setup` gets it over with, and a stored restore token
  means it does not come back. It needs the ScreenCast portal too, because
  absolute pointer positioning interprets its coordinates in a screencast
  stream's space — a session offering only one of the two is reported as having
  no input backend rather than failing on its first click.
- **Wayland window lists** come from AT-SPI frames. No stacking order, no screen
  position, and applications without accessibility support are invisible.
- **Wayland window capture** is unavailable. The Screenshot portal has no window
  target that any backend implements, and the ScreenCast route requires a human
  to pick the window in a dialog. Capture the screen instead.
- **Away from GNOME**, every portal-backed capability carries the same caveat:
  the interface is advertised by the session and is therefore used, but only
  GNOME's backend has actually been run against. `desktop capabilities` names the
  desktop in the note rather than leaving it implied.
- **X11 window lists** come from EWMH, so they carry stacking order (topmost
  first), real screen geometry and the minimized state, and include applications
  with no accessibility support at all. Those last ones are marked
  `"accessible": false`: they can be screenshotted and clicked by coordinate, but
  `desktop snapshot` on one refuses rather than inventing an empty tree.
- **Focus under Wayland** has no protocol behind it: nothing lets a client raise
  a window, and AT-SPI's `GrabFocus` returns success while doing nothing. What
  is left is asking the *application* to present itself, over
  `org.freedesktop.Application.Activate` — a request the compositor weighs quite
  differently from a stranger demanding focus. It reaches only applications that
  export that interface, and mutter may still answer by marking the window as
  demanding attention rather than raising it, so the driver checks whether the
  window actually became active and fails when it did not. Under X11 focus goes
  through `_NET_ACTIVE_WINDOW` and the result is read back from the root window,
  so a window manager that declines is reported as a failure rather than as
  success.
- **GNOME's own window APIs are closed to us**, which is why none of this goes
  through them. `org.gnome.Shell.Introspect` — the window list, with titles and
  app ids — and `org.gnome.Shell.Screenshot` both check the caller against an
  allowlist containing the two portal implementations and nothing else. The
  portal is the whole of the sanctioned surface on GNOME.
- **wlroots** has native mechanisms upstream — `wlr-screencopy`,
  `zwlr_virtual_pointer_v1`, `zwlr_foreign_toplevel_management_v1` — that this
  build does not implement. They would buy what the portals cannot: per-window
  capture, and a real window raise. Until then those rows stay `✗` there,
  refused with a structured error rather than falling through to something that
  appears to work.

### KDE is not supported

`desktop` refuses on KDE. Not partially, not with caveats: `desktop info` names
the desktop, `desktop capabilities` marks every capability
`unsupported_desktop`, and every command that would read or drive it exits 2
with a structured error.

KDE's own interfaces are shut to a program like this one.
`org.kde.KWin.ScreenShot2` — which offers exactly what no other Wayland desktop
does, per-window capture with no picker — answers
`org.kde.KWin.ScreenShot2.Error.NoAuthorized` unless the caller was started from
a desktop entry declaring `X-KDE-DBUS-Restricted-Interfaces`, which a
command-line tool is not. Verified against KWin 6.7.4: the interface is there,
version 5, all seven methods, and it refuses. Installing a declaring entry and
rebuilding the service cache did not change the answer. KWin also implements
none of the `ext-*` capture protocols, so there is no way round it.

What would be left is the accessibility tree plus portal behaviour this build
has never been run against — on a desktop that has already closed its own
interfaces. That is how a tool ends up with a scattering of commands that work
and no way to tell which; one clear refusal is worth more.

The tree *does* work there, and saying so matters more than the decision:
verified on KWin 6.7.4, `desktop apps`, `desktop windows` and a full
`desktop snapshot` of a GTK4 application all answered correctly before support
was removed. If that trade is wrong for you, the mechanism is one function —
`is_supported` in `desktop_core::models::backend`.

**`desktop session` is unaffected.** It starts an X11 display of its own with
its own window manager, and nothing about the desktop you are running reaches
inside it, so an agent gets every capability on a KDE machine exactly as it does
anywhere else.

### Two things that are not fixable here

**Absolute coordinates do not exist under Wayland.** A toolkit cannot learn its
own screen position, so `Component.GetExtents(Screen)` returns surface-relative
numbers. Verified on GNOME 49: a maximized window whose true position is y=32
reports y=0. Snapshots therefore declare their coordinate space
(`"coordinate_space": {"window": 3}`) and `bounds` is `null` where a position is
genuinely unknowable — never zero, which would read as a real position at the
origin.

**`ext-foreign-toplevel-list-v1` and `ext-image-copy-capture-v1` will not come
to GNOME.** Mutter has no plans, and KWin's `src/wayland` carries no
implementation of them either. Those protocols are a wlroots-family path in
practice, which is why the architecture has slots for them but the GNOME path
uses portals.

---

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/evandrocabf/desktop-driver/main/install.sh | bash
```

The source arrives as a tarball when `git` is missing and as a clone when it is
not. On macOS the installer always compiles that repository locally, so it
requires Cargo/Rust; it never downloads a prebuilt Mac binary. On Linux it may
use an existing matching release — verified against its
published SHA-256 — and otherwise falls back to `cargo build`. A checksum that
does not match is fatal rather than a reason to compile instead.

From a checkout it always builds *that* checkout, which is what you want when
you are working on it:

```bash
git clone https://github.com/evandrocabf/desktop-driver
cd desktop-driver && ./install.sh
```

`install.sh` puts the binary on your `PATH` and links the agent skill into
wherever your coding agents look for one. The skill itself is
tool-agnostic — plain shell commands, no vendor's API — so every agent gets the
same file. Codex, Claude Code, Cursor, opencode, Gemini/Antigravity, Cline and
Windsurf are each detected and given it in their own layout, and
`~/.agents/skills` — the cross-tool convention — is written even when none of
them are found. It names everything it writes as it writes it; `--dry-run`
shows the plan without touching anything and `--uninstall` removes exactly what
it added.

```bash
./install.sh --dry-run               # see the plan first
./install.sh --project .             # install into this project instead of $HOME
./install.sh --agents codex,cursor   # pick the agents yourself
./install.sh --all                   # every known agent, detected or not
./install.sh --no-agents             # just the binary
./install.sh --from-source           # compile, even where a release exists (automatic on macOS)
./install.sh --static                # a musl binary that runs on any Linux
./install.sh --update                # refresh checkout, binary and installed skills
./install.sh --uninstall
```

Updating is the same safe, atomic installation path as the first install. Run
the checkout's `install.sh --update`, or rerun the one-line `curl` command if
the existing installation predates the `--update` option. Git checkouts are
fast-forwarded; tarball checkouts are refreshed in place. The installer checks
that the replacement binary reports the same version as the fetched source
before replacing the installed executable, and leaves persistent browser
profiles outside the checkout untouched.

Version `0.1.0` was the first release; the current source version is `0.2.0`.
There is currently no automated release or binary-publication pipeline. The
supported macOS installation path is a local build from this repository.

Or build it yourself — Rust 1.97.1, pinned in `rust-toolchain.toml`:

```bash
cargo build --release
./target/release/desktop doctor
```

### The agent skill

`skills/desktop-driver/SKILL.md` is written for the agent rather than for you:
which commands to reach for, when to work through elements instead of the
pointer, and — the decision that matters most — whether to drive the user's
desktop or start a display of its own. `AGENTS.md` in this repo is a symlink to
it, so an agent working *on* desktop-driver reads the same guidance.

It names no particular agent and depends on no particular runtime. Everything
it describes is a shell command with a `--json` form, which is the reason one
file serves every tool: an agent that can run `desktop snapshot --json` and read
the result has everything it needs, whichever vendor built it.

### What has actually been run

Portability here is tested, not asserted. `scripts/distro-matrix.sh` builds the
binary and then, on each distribution, starts a session, launches a GTK
application onto it, reads its accessibility tree and captures the screen:

| | build | session | tree | capture |
|---|---|---|---|---|
| Fedora 43 (GNOME 49, Wayland) | ✓ | ✓ | ✓ | ✓ |
| Debian 13 | ✓ | ✓ | ✓ | ✓ |
| Ubuntu 24.04 | ✓ | ✓ | ✓ | ✓ |
| Arch | ✓ | ✓ | ✓ | ✓ |
| openSUSE Tumbleweed | ✓ | ✓ | ✓ | ✓ |
| macOS hosted (Apple silicon + Intel) | ✓ | refused, as it should be | no TCC | no TCC |

The Linux rows are containers with no systemd, no desktop and no login session,
which is a harsher environment than a real machine and the same one CI runs in.

The hosted macOS row proves clean-machine behavior without TCC grants. For a
native check, build locally, grant the three permissions to the terminal or app
running the binary, and run `scripts/macos-e2e.sh`. It builds a deterministic
AppKit fixture and verifies app/window identity across CLI processes, tree
reads, AX actions, exact AXValue read-back, focus, pointer input, Unicode
keyboard input, and screen/app/window capture. Missing permissions or desktop
capabilities fail the script instead of becoming a successful skip.

### One binary for every distribution

`desktop` links no C libraries beyond libc — AT-SPI, D-Bus and X11 are all
spoken in pure Rust — so a static build is a single file that runs anywhere:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

Worth doing if you distribute it. A normal glibc build carries the glibc
version of the machine that produced it: one built on Fedora 43 requires
`GLIBC_2.39` and will not start on Debian 12 or Ubuntu 22.04. The musl build has
no such floor, and the session path was verified with it.

The repository does not infer native compatibility from cross-compilation.
Run `scripts/macos-e2e.sh` on every macOS version and architecture for which you
want native evidence. Missing WindowServer access, TCC grants, fixture controls
or capture output fails the check instead of turning it into a successful skip.

### Linux

`desktop doctor` prints this table for your machine, marks what is missing, and
gives you a single install command for your distribution. It is the answer to
"what does this need", not just "what is broken today".

| Package | Need | Enables |
|---|---|---|
| `at-spi2-core` | **required** | reading the accessibility tree — snapshots, selectors, element actions |
| `xdg-desktop-portal` + a backend | recommended (Wayland) | screenshots and input |
| `Xvfb` | optional | `desktop session` — a display of the agent's own |
| `openbox` | optional | window management inside that display: the window list, its stacking order and geometry, and focus |
| `dbus-daemon` | optional | that display's private bus, which is what isolates its accessibility tree |

The first two ship with any modern desktop, so a normal install needs nothing
extra. The rest are what you install to stop the agent sharing your screen — on
Fedora that is:

```bash
sudo dnf install xorg-x11-server-Xvfb openbox
```

`at-spi2-core` also supplies `at-spi-bus-launcher` and `at-spi2-registryd`,
which a session starts itself. It does not rely on D-Bus activation for them:
their `.service` files name systemd units that are one-per-user, so activating
them from a second bus fails, and under SELinux the `Exec=` fallback is refused
too. Starting them directly works on both and needs no policy change.

```bash
sudo dnf install xorg-x11-server-Xvfb openbox     # Fedora
sudo apt install xvfb openbox                     # Debian / Ubuntu
sudo pacman -S xorg-server-xvfb openbox           # Arch
```

**Why X11 for the agent's display, on a Wayland machine?** Because that is where
this tool is strongest: under X11 it captures with `GetImage`, injects with
XTEST, and can actually raise a window. Under Wayland it can do none of those
without a portal grant, and cannot raise a window at all. `Xvfb` is also tiny
and is what every CI system already uses.

If a window looks empty, run `desktop doctor`. The usual causes:

- **Firefox, Chromium and Qt** build no tree until `org.a11y.Status.IsEnabled`
  is set. `desktop setup` sets it. Symptom: the application shows up in
  `desktop apps` with a window, and the window has no contents.
- **Electron** needs its own switch even then: `--force-renderer-accessibility`,
  or `ACCESSIBILITY_ENABLED=1`.

> Do **not** set `org.a11y.Status.ScreenReaderEnabled` to force a tree. On GNOME
> that starts Orca reading the screen aloud. `desktop doctor` never suggests it.

### macOS

Requires **macOS 14 (Sonoma)** or later: `SCScreenshotManager` needs it, and
`CGWindowListCreateImage` is obsoleted in the macOS 15 SDK.

Three separate permissions, with separate prompts:

- **Accessibility** — the UI tree and element actions.
- **Screen Recording** — screenshots, *and* window titles in the window list.
- **Posting events** — the pointer and the keyboard. This is the quiet one: a
  process trusted for Accessibility but not for posting reads every tree
  perfectly and has every click discarded without an error, so `desktop
  capabilities` preflights it separately and reports `mouse`, `keyboard` and
  `scroll` as unavailable rather than letting them fail silently.

`desktop setup` requests all three grants. macOS owns the dialogs; approve them
in the visible system UI, then rerun `desktop capabilities` to verify the
result. The tool never reads or types authentication material.

The one that confuses everyone: a command-line tool inherits the permission of
**the terminal that launched it**. Grant access to iTerm2 / Ghostty / Terminal,
not to `desktop`. The error message names the launching application for you.

TCC can also use code identity when deciding whether a grant still applies.
For local development, install and run one stable compiled copy; after replacing
it, rerun `desktop setup` if macOS no longer recognizes the previous grant. An
embedding application can sign the helper with its own existing app-signing
process, but desktop-driver itself requires no signing credentials.

---

## Commands

```bash
desktop info                 # detected platform, display server, selected backends
desktop capabilities         # what works here, and why the rest does not
desktop doctor               # why the tree is empty or shallow, and how to fix it
desktop setup                # perform the one-time interactive grant

desktop apps
desktop windows [--app NAME]

desktop snapshot [--app NAME | --window ID] [--all]
desktop inspect  [--app NAME | --window ID]     # raw tree, before pruning
desktop find --role button [--name "Save"] [--text "Continue"]
desktop wait --text "Build complete" [--timeout 5000]

desktop screenshot [--window ID | --app NAME] [--output shot.png]

desktop focus --app "Visual Studio Code"
desktop move  --x 800 --y 400
desktop click --element 42 | --role button --name Save | --x 800 --y 400
desktop type  "Hello world"              # to whatever has focus (racy if shared)
desktop type --element 23 "x.com"        # straight into the field (no keystrokes)
desktop key   "cmd+s"
desktop scroll --y -500

desktop browser doctor
desktop browser open [URL] [--browser chromium|firefox] [--headless] [--profile NAME] [--executable PATH]
desktop browser snapshot [-i | --all] [--max-nodes N]
desktop browser goto URL | back | forward | reload
desktop browser click TARGET
desktop browser fill TARGET VALUE
desktop browser type TARGET VALUE [--delay MS]
desktop browser get text|html|value TARGET
desktop browser get attr TARGET ATTRIBUTE
desktop browser get title|url
desktop browser wait [TARGET] [--text TEXT | --url URL | --load STATE]
desktop browser screenshot [--full-page] [--output page.png]
desktop browser download TARGET [--output DIR]
desktop browser tab list|new|use|close
desktop browser close

desktop session create NAME
desktop session list
desktop session start [NAME] [--size 1440x900] [--display 90] [--visible]
desktop session status
desktop session run firefox https://x.com
desktop session env                      # exports for running something by hand
desktop session stop
desktop session delete NAME             # permanently removes saved browser state

desktop --host screenshot                # your desktop, ignoring the session
```

`cmd` means the Command/Super key on both platforms — it is never silently
rewritten to Ctrl. Use `accel` for "this platform's menu modifier": Command on
macOS, Ctrl on Linux. On macOS, character shortcuts carry their Unicode value
with the modifier mask, so `accel+s` does not assume a US physical layout;
only non-text keys such as Return, arrows and function keys use fixed Apple
virtual keycodes.

### Exit codes

Callers branch on these rather than parsing output, so they are part of the
public interface.

| Code | Meaning |
|---|---|
| 0 | success |
| 2 | setup or configuration failure (including `unsupported_capability`) |
| 3 | policy denied |
| 4 | interaction required (a permission or grant is missing) |
| 5 | backend failure |
| 6 | target failure (no such element, stale, ambiguous, bad argument) |
| 7 | timeout |
| 70 | internal invariant violated |

### Errors

Every failure is a tagged JSON object with the fields needed to decide what to
do next:

```json
{
  "error": "unsupported_capability",
  "capability": "mouse",
  "backend": "none",
  "platform": "linux",
  "display_server": "wayland",
  "desktop_environment": "sway"
}
```

```json
{ "error": "element_stale", "element": 42, "reason": "role_changed",
  "expected": "button", "found": "text_box" }
```

In `--json` mode the process writes exactly one JSON document to stdout and
nothing else.

---

## Security

### What is written to disk, and who can read it

Everything this tool writes is owner-only, and the directories it creates are
`0700` — including ones an earlier version left at `0755`, which are tightened
when found rather than trusted.

| | where | mode |
|---|---|---|
| screenshots (default) | `$XDG_RUNTIME_DIR/desktop-driver/` | `0600` |
| snapshots | `$XDG_RUNTIME_DIR/desktop-driver/snapshot.json` | `0600` |
| session record (holds the display cookie) | `$XDG_RUNTIME_DIR/desktop-driver/` | `0600` |
| the display's `Xauthority` | `$XDG_RUNTIME_DIR/desktop-driver/sessions/<name>/` | `0600` |
| portal restore token | `$XDG_STATE_HOME/desktop-driver/` | `0600` |
| named browser profiles | `$XDG_DATA_HOME/desktop-driver/sessions/<name>/home` | `0700` |

The default capture location matters more than it looks: it used to be the
shared temporary directory, which is mode `1777`, so every account on the
machine could read every screenshot the agent took — and a screenshot is
whatever was on its screen. Snapshots are the same problem in text form, and
both fall back to that directory when there is no runtime directory at all.
The file being owner-only is what actually protects them.

Processes a session owns are recorded as `(pid, start time)` rather than pid
alone, because the kernel reuses pid numbers: `desktop session stop` will not
signal something it did not start.


- **Password values never leave the process.** Any element whose role is a
  password field, or whose platform state marks it protected, is emitted with
  `value: null` and `redacted: true`. This is unconditional, not a policy
  setting, and it is enforced at the single point every snapshot passes through.
  `desktop inspect` redacts too — it is a debugging view, not a bypass.
  `--text` search never matches inside a redacted value.
- Observation and action are separated at the type level, so `--read-only` is
  one check rather than twelve.
- `--allow-app`, `--deny-app` and `--deny-role` are evaluated before dispatch.
  Deny always wins over allow.

---

## Architecture

```
crates/
├── desktop-core     models · ports · snapshot · selectors · policy · errors
├── desktop-browser  CDP + WebDriver BiDi · daemon · profiles · page automation
├── desktop-linux    AT-SPI · X11/XTEST · xdg-desktop-portal
├── desktop-macos    AXUIElement · CGWindowList · ScreenCaptureKit · CGEvent
└── desktop-cli      the `desktop` binary
```

Platform crates are selected by target `cfg`, never by a Cargo feature, so the
wrong backend cannot be enabled by accident.

`desktop-core` defines four narrow ports — accessibility, capture, input, probe
— rather than one wide trait, because on Linux those are four subsystems that
fail independently. A composed `Driver` puts every call through the same two
gates before it reaches a backend:

```
Driver::click(…)
  ├ capabilities().require(Mouse)?   → unsupported_capability {backend, display_server, de}
  ├ policy.check(Click, &target)?    → policy_denied
  └ input.click(…)
```

Because the gate lives in core, a backend *cannot* forget to declare something
unsupported — anything undeclared fails closed.

`atspi` and `ashpd` are async-only, and ScreenCaptureKit is completion-handler
based. All three are bridged back to sync inside their adapters, so the core and
the CLI stay synchronous.

---

## Development

```bash
cargo test --workspace                             # no desktop needed
cargo xtask architecture                           # layering and pinning gates
cargo clippy --all-targets -- -D warnings
cargo check --target aarch64-apple-darwin --workspace   # type-check macOS from anywhere
```

The core is tested against JSON tree fixtures, so snapshot pruning, selector
matching, path resolution, shortcut parsing, HiDPI transforms, backend selection
and error shapes are all covered without a desktop present. `RecordingInput`
implements `InputPort` and records calls, which is how clicking and typing are
tested **without moving the real mouse**.

Live smoke tests are `#[ignore]`d and symmetric across platforms: they drive
`gnome-calculator` on Linux and `Calculator.app` on macOS through `7 + 3 =` and
assert the display reads `10`.

```bash
cargo test --workspace -- --ignored
```

Set `DESKTOP_DRIVER_LIVE_REQUIRED=1` in automation. With it, a missing display,
permission, backend, application, or accessibility tree is a test failure, not
a skip. The complete macOS native fixture is run with `scripts/macos-e2e.sh`;
it requires a dedicated logged-in Mac because macOS cannot create a second
WindowServer session for an agent.

---

## Not in this version

MCP server, OCR, visual reasoning, remote control over a network, Windows, arbitrary page-script
execution, recording and replay.

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) covers how to build it, what the architecture gates enforce, and
the rule the rest of the code follows: never report success for something that did not happen, and
never invent a value you do not have. Most of the hard-won fixes here are instances of it.

- [SECURITY.md](SECURITY.md) — what the tool does by design, what it writes to disk and who can read
  it, what isolation does and does not give you, and how to report a vulnerability privately.
- [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
- [CHANGELOG.md](CHANGELOG.md)

## License

MIT — see [LICENSE](LICENSE).
