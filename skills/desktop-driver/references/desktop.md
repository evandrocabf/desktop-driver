# Accessibility-driven desktop applications

Use these commands for non-browser applications and browser chrome. They inspect the accessibility
tree that a screen reader uses. For content inside a web page, prefer `desktop browser`.

## Choose the display safely

- Existing application on the user's desktop: use `desktop --no-steal-focus ...` and only
  element-addressed actions. Do not use `focus`, `key`, `move`, `scroll`, coordinate clicks, or
  `click --via pointer`; the policy will refuse them.
- Application launched for the task: start an isolated session, then omit `--host` so commands
  automatically address the agent display.
- `--host` explicitly bypasses an active session and targets the user's real desktop. Use it only
  when that is the requested target.

## Inspect, act, verify

```bash
desktop --no-steal-focus --json apps
desktop --no-steal-focus --json windows --app Firefox
desktop --no-steal-focus --json snapshot --app Firefox --max-nodes 300
desktop --no-steal-focus --json find --role button --name 'Save'
desktop --no-steal-focus --json click --element 7
desktop --no-steal-focus --json wait --text 'Saved' --app Firefox --timeout 10000
desktop --no-steal-focus --json snapshot --app Firefox
```

Snapshot IDs belong to the last desktop snapshot, not a browser-native snapshot. Snapshot before
using `--element`. IDs are re-resolved against the live tree; if the widget was replaced, handle
`element_stale` by taking a new snapshot. A successful `desktop wait` stores the refreshed snapshot,
so its element IDs are current for the next action.

## Exact command shapes

Environment and reading:

```text
desktop info
desktop capabilities
desktop doctor
desktop setup
desktop apps
desktop windows [--app APP]
desktop snapshot [--app APP | --window ID] [--all] [--max-nodes N] [--max-depth N]
desktop inspect [--app APP | --window ID] [--max-nodes N] [--max-depth N]
desktop find [--role ROLE] [--name NAME] [--text TEXT]
desktop wait [--role ROLE] [--name NAME] [--text TEXT] [--app APP | --window ID] [--timeout MS] [--interval MS]
desktop screenshot [--app APP | --window ID] [--output PATH]
```

Actions:

```text
desktop focus [--app APP | --window ID]
desktop click --element ID [--via auto|action|pointer]
desktop click [--role ROLE] [--name NAME] [--text TEXT] [--via auto|action|pointer]
desktop click --x X --y Y [--button left|right|middle] [--count N]
desktop type TEXT [--element ID]
desktop key SHORTCUT
desktop move --x X --y Y
desktop scroll [--x PX] [--y PX]
```

Put global options before the command:

```bash
desktop --no-steal-focus --json type --element 23 'hello world'
desktop --read-only --json snapshot --app Firefox
```

For desktop selectors, `--role`, `--name`, and `--text` may narrow the same match. `--name` is an
exact accessible-name match; `--text` is a containment search over name, value, or description.
Prefer a snapshot ID when available. If a selector is ambiguous, add role/name constraints or use a
fresh ID; do not fall back to guessed coordinates.

## Pointer and keyboard fallbacks

Use these only inside an isolated session or when the user explicitly permits foreground input:

```bash
desktop --json focus --app Firefox
desktop --json key 'accel+l'
desktop --json type 'https://example.com'
desktop --json key Enter
desktop --json move --x 800 --y 400
desktop --json click --x 800 --y 400 --button right --count 1
desktop --json scroll --y -500
```

`accel` means Command on macOS and Ctrl on Linux. `cmd` means Command/Super and is not rewritten to
Ctrl. `desktop type TEXT` writes to current focus and is racy on a shared desktop; prefer
`type --element ID` there.

## Bounds and screenshots

Accessibility bounds may be `null`, especially for GTK4 and Wayland. Never turn null bounds into
`0,0` or derive coordinates from them:

```bash
desktop --json snapshot | jq '.elements[] | select(.bounds != null)'
desktop --json screenshot --output /tmp/desktop-shot.png
```

Use element actions when bounds are absent. Use a screenshot only when layout, color, canvas
content, or a coordinate-only target must be inspected visually.
