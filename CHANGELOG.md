# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing is published to a registry yet. Install from the repository with `install.sh`, which builds
the binary and links the agent skill into whichever coding agents are present.

### Added

- **Reading a desktop.** `desktop apps`, `windows`, `inspect`, `snapshot` and `find` turn a running
  application into a numbered list of widgets. Snapshots are pruned to what an agent can act on and
  carry a re-resolvable path per element, so `desktop click --element 42` works from a *different
  process* — an element that moved is still found, one that was replaced is reported `element_stale`
  rather than clicked at a stale position. Every command has a `--json` form.
- **Acting on it.** `click`, `type`, `key`, `scroll`, `move` and `focus`. Element-addressed work goes
  through the accessibility API rather than synthetic input, so it needs no pointer, no focus change
  and no coordinates. `desktop type --element N` writes into a field directly, and verifies the
  write landed rather than trusting the toolkit's return value.
- **Screenshots**, through `GetImage` under X11, the Screenshot portal under Wayland, and
  `SCScreenshotManager` on macOS.
- **Window lists from the window manager under X11.** EWMH supplies the windows, their stacking
  order (topmost first), their real screen geometry and the minimized state, with AT-SPI joined onto
  it for the tree behind each. An application with no accessibility support is listed rather than
  invisible, marked `"accessible": false`: it can be screenshotted and clicked by coordinate, and
  `desktop snapshot` on it refuses instead of inventing an empty tree. Under Wayland the list still
  comes from AT-SPI frames, which is all a client there can see.
- **Installation in one line** — `curl -fsSL .../install.sh | bash`, needing only curl and tar. The
  source arrives as a tarball where git is absent, and the binary is downloaded from the matching
  release and verified against its published SHA-256, falling back to a source build where no
  release covers the platform. Releases are built for x86_64 and aarch64 Linux (static musl) and
  both macOS architectures by `.github/workflows/release.yml` on a `v*` tag.
- **`desktop session`** — a display of the agent's own: its own X server, D-Bus, accessibility bus
  and window manager, plus its own home directory so a browser opens a clean profile instead of
  yours and the two do not contend for one profile lock. Inside a session nothing is shared, so
  focus, window capture and pointer input all work where they cannot on GNOME Wayland. It is
  **visible by default**, rendered into a window you can watch and click into to take over; where a
  window is impossible it starts headless and says so.
- **Honest capability reporting.** `desktop capabilities` reports every operation as supported,
  degraded with a stated caveat, or unsupported with a machine-readable reason. `desktop doctor`
  explains why a tree is empty and prints the exact install command for the distribution in hand.
- **Policy flags** — `--read-only`, `--no-steal-focus`, `--allow-app`, `--deny-app`, `--deny-role` —
  enforced in the core before dispatch, so a backend cannot forget them.
- **An agent skill**, `skills/desktop-driver/SKILL.md`, and `install.sh` to place it.

### Security

- Password fields are redacted unconditionally — `value: null`, `redacted: true` — in the one place
  every read path converges on, with an architecture gate that fails the build if a second path
  appears.
- Everything written to disk is owner-only and every directory created is `0700`, including ones an
  earlier version left wider, which are tightened rather than trusted. Captures previously defaulted
  to the shared temporary directory at `0644`, where any local account could read them.
- Session processes are identified by `(pid, start time)` rather than pid alone, so `session stop`
  cannot signal a process that merely inherited a recycled pid.

### Notes

- macOS compiles and lints clean for `aarch64-apple-darwin` and `x86_64-apple-darwin` and its known
  platform traps are handled, but **it has not been run on hardware**. Treat it as unverified.
- Linux is verified end to end on Fedora, Debian, Ubuntu, Arch and openSUSE by
  `scripts/distro-matrix.sh`.
- Backends are selected from what a session advertises rather than from its name, so KDE, wlroots
  and other non-GNOME Wayland compositors get the freedesktop portals their own desktop implements.
  Only GNOME's portal backend has been run against, and every capability note away from GNOME says
  so. Where a portal is genuinely absent the refusal is still a structured error rather than an
  unverified path, and `desktop session` works there regardless.
- Input under Wayland needs both the RemoteDesktop and ScreenCast portals, because absolute pointer
  positioning interprets its coordinates in a screencast stream's space. A session offering only one
  of the two reports no input backend instead of failing on its first click.
