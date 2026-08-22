# Security

## Reporting a vulnerability

Please report privately through
[GitHub security advisories](https://github.com/evandrocabf/desktop-driver/security/advisories/new)
rather than opening a public issue. We aim to acknowledge a report within a week.

## What this tool does, by design

`desktop-driver` exists to read the accessibility tree of running applications, capture the screen,
and inject keyboard and pointer input. Several capabilities are intentional and are not
vulnerabilities in themselves:

- **It reads other applications' contents.** The accessibility API is how a screen reader works, and
  `desktop snapshot` returns whatever a window exposes through it — message bodies, document text,
  field values. That is the entire point of the tool.
- **It types and clicks.** On the user's own desktop there is one keyboard focus and one pointer, so
  input goes wherever focus happens to be at that moment. `--no-steal-focus` refuses everything that
  would seize a shared device, and `desktop session` removes the problem by giving the agent a
  display of its own.
- **`desktop session run` executes whatever you pass it**, with the arguments you pass. There is no
  shell in between — arguments are passed as an argument vector, never as a command string — but the
  program itself runs with your privileges.
- **A session's applications run as you.** A separate display is not a sandbox. Anything launched
  into one can read your files, reach the network, and use any credential in the home directory it
  was given.
- **Captured images and snapshots are written to disk unencrypted.** If a window is showing a secret,
  that secret lands in the PNG and in `snapshot.json`. They are owner-only (see below), not
  protected from you or from root.

## What is deliberately never exposed

Password fields are redacted unconditionally, not as a policy the caller can turn off. A field whose
role is a password type, or which carries AT-SPI's `PROTECTED` state, is emitted with `value: null`
and `redacted: true`. On macOS the signal is a subrole rather than a state — `AXSecureTextField`
maps to the same role, so the same rule applies.

Three output paths can put a value in front of a caller: the snapshot normalizer, and the two
renderers behind `desktop inspect`, which shows the raw tree and is a debugging view rather than a
bypass. `cargo xtask architecture` fails the build if a file that both reads a value and renders,
serializes or builds an `Element` does not consult `is_secure()` — and refuses to pass at all if it
can no longer find at least two such paths, so the gate cannot quietly stop checking anything.

## What is written to disk, and who can read it

| | where | mode |
|---|---|---|
| screenshots (default) | `$XDG_RUNTIME_DIR/desktop-driver/` | `0600` |
| snapshots | `$XDG_RUNTIME_DIR/desktop-driver/snapshot.json` | `0600` |
| session record, incl. the display cookie | `$XDG_RUNTIME_DIR/desktop-driver/` | `0600` |
| the agent display's `Xauthority` | `$XDG_RUNTIME_DIR/desktop-driver/sessions/<name>/` | `0600` |
| portal restore token | `$XDG_STATE_HOME/desktop-driver/` | `0600` |
| named browser homes | `$XDG_DATA_HOME/desktop-driver/sessions/<name>/home` | `0700` |
| browser CDP socket directory | `$XDG_RUNTIME_DIR/desktop-driver/browser/` | `0700` |

Where there is no runtime directory — a container, a cron job, a login without systemd — the first
four fall back to the shared temporary directory, which is world-writable. The files are owner-only
in either place, which is what actually protects them.

Directories are created `0700`, and one found at a wider mode is tightened rather than trusted, so a
session created by an older version is corrected on the next run.

## Isolation, and its limits

`desktop session` starts an X server, a D-Bus daemon, an accessibility bus and a window manager of
its own. That gives the agent its own framebuffer, pointer, keyboard focus and accessibility
registry: input injected into a session cannot reach your applications, and a capture of a session
contains only that session's windows. The agent display is protected by an `MIT-MAGIC-COOKIE-1`
generated per session from `/dev/urandom` and stored `0600`, and the X server is started with
`-nolisten tcp`.

It is **not** a security boundary against the applications inside it. It separates two users of one
machine from each other's *screens*; it does not confine what those applications may do. For that,
use a container or a VM.

The browser-native daemon listens only on a local Unix socket. Managed Chromium exposes CDP on
`127.0.0.1` with an ephemeral port and a dedicated user-data directory; explicit attachment
refuses non-loopback WebSocket endpoints. `browser close` terminates only a managed Chromium child
and merely disconnects from an attached browser. Browser password inputs are returned as
`value: null, redacted: true`, and `fill`/`type` refuse them independently of caller policy.

## What we do treat as a vulnerability

- Any secret-bearing value reaching output through a path that bypasses redaction.
- A predictable path in a shared directory that another local user could pre-create or substitute.
- Any file or directory created wider than the table above.
- Signalling or terminating a process the tool did not start.
- Reading or writing outside the paths above and the ones you pass on the command line.
- Any argument, window title or element name that reaches a shell, a format string, or a D-Bus
  method unquoted or unescaped.
- A capability reported as available that silently does nothing, since acting on a false report is
  how an agent ends up typing into the wrong window.

## Permissions on macOS

Three separate grants are involved, and the tool preflights each rather than discovering the
failure mid-action: Accessibility (reading the tree), Screen Recording (captures, and window titles),
and posting events (pointer and keyboard). A process trusted for the first but not the third reads
everything correctly and has every click discarded, which is why they are reported separately.

macOS attributes a grant to the *launching application* — your terminal — not to `desktop`, and it
identifies a binary by code signature, so an unsigned build gets a new identity on every rebuild and
silently loses the grant.
