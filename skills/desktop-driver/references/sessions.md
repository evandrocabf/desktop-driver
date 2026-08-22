# Isolated desktop sessions

Sessions give the agent a separate Linux display, pointer, keyboard, framebuffer, D-Bus session,
and persistent home. Use one when launching a GUI for the task. Sessions are Linux-only; on macOS,
use the user's desktop with `--no-steal-focus` and element actions.

## Lifecycle

```bash
desktop --json session list
desktop --json session start task-name --visible
desktop --json session status
desktop --json session run firefox 'https://example.com'
desktop --json apps
desktop --json snapshot --app Firefox
desktop --json session stop
```

Choose a stable, task-specific name when state must persist. `session start task-name` creates the
workspace if absent and reuses it if present. `session create task-name` is only for provisioning a
workspace without starting it and fails if that name already exists. `session stop` ends the
display and its processes but preserves the named workspace. `session delete task-name`
permanently removes the workspace, including saved browser state, and requires explicit user intent.

Exact forms:

```text
desktop session create NAME
desktop session list
desktop session start [NAME] [--size WIDTHxHEIGHT] [--display N] [--visible | --headless] [--share-home]
desktop session status
desktop session run PROGRAM [ARGUMENT...]
desktop session env
desktop session stop
desktop session delete NAME
```

Put global `desktop` options before `session run`. Everything after `PROGRAM` is passed to that
program, so `desktop session run firefox --json` passes `--json` to Firefox; it does not enable
desktop-driver JSON output. The correct form is:

```bash
desktop --json session run firefox 'https://example.com'
```

Do not use `--share-home` for credential handoff. It is only a backwards-compatible option for the
`default` session and exposes the user's application profiles and their lock files.

## Visible versus headless

- Watching is the default where supported. `--visible` makes watchability mandatory and fails
  instead of silently falling back.
- `--headless` explicitly opts out of a watchable window for unattended work.
- Use `session status` to verify the actual mode and display before starting a GUI.
- Stop every session started for the task, including after a failed test.

## Authentication handoff

Authentication always requires `--visible`:

1. Create and start a named visible session.
2. Open the browser using the same automation route that will be used after login:
   - Page-native route: `desktop browser open URL --profile NAME`.
   - Accessibility/browser-chrome route: `desktop session run firefox URL`.
3. Tell the user the visible `desktop-driver` window is ready and stop issuing observation or input
   commands while they authenticate.
4. Wait for the user to confirm completion.
5. Continue in the same named profile or session.

Never ask the user to paste a password, passkey, recovery code, or one-time code into chat. Never
snapshot, screenshot, inspect, or type during the handoff. If the visible window cannot be
provided, report the blocker and stop.

## Which browser persistence is being used?

There are two persistent but separate routes:

- `desktop browser open --profile NAME` stores the managed Chromium/Firefox page-automation
  profile and remembers its engine.
- `desktop session run firefox|chromium ...` uses the named session's persistent home and is driven
  through desktop accessibility.

Do not authenticate through one route and expect the other route to share its cookies. Choose the
route based on whether the task needs page semantics or browser chrome, then keep using it.
