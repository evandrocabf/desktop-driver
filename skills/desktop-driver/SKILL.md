---
name: desktop-driver
description: Inspect and control desktop GUIs and browser pages from the shell. Use for browser navigation and forms, desktop applications, screenshots, visible login handoffs, or isolated GUI sessions. Route page content through browser-native commands and browser chrome or other applications through accessibility commands.
---

# Use desktop-driver without guessing its CLI

`desktop` is the instrument, not the application under test. It has three distinct interfaces:

| Need | Interface | Read before acting |
|---|---|---|
| Navigate a web page, query the DOM, fill forms, manage tabs or downloads | `desktop browser` | [references/browser.md](references/browser.md) |
| Inspect or act on browser chrome or another GUI application | accessibility commands such as `snapshot`, `click`, and `type` | [references/desktop.md](references/desktop.md) |
| Launch a GUI away from the user's keyboard and pointer | `desktop session` | [references/sessions.md](references/sessions.md), then the reference for the chosen interface |

Read only the reference for the route being used. Read
[references/recovery.md](references/recovery.md) after a failed command or when scripting around exit
codes. Do not translate options between interfaces: `desktop click --element 7` and
`desktop browser click @e7` are unrelated syntaxes.

## Command discipline

1. If the exact syntax is not present in the relevant reference, run
   `desktop <namespace> <command> --help` before composing the command. Never invent a flag, selector
   prefix, key chord, or positional order.
2. Put global options immediately after `desktop` so they cannot be consumed by a launched program:
   `desktop --json ...`, `desktop --no-steal-focus ...`, `desktop --read-only ...`.
3. Use `--json` when subsequent logic depends on the result. Branch on the exit code and the JSON
   `error` field; do not parse human prose.
4. Quote user text, paths, URLs containing shell metacharacters, and complete browser selectors.
   Pass literal values as one argument.
5. Observe, act, then observe again. Use `wait`; never use a fixed sleep to assume a GUI or page
   changed.
6. Stop sessions and managed browsers that this task started. Do not close a pre-existing user
   application or delete a persistent profile unless the user asked.

## Safety boundary

When using the user's existing desktop, add `--no-steal-focus` and use element-addressed
accessibility actions. It refuses pointer or keyboard operations that could race the user. Use
`--host` only when the task explicitly targets the user's desktop while an agent session is active.

For a new login, start a named session with `--visible`, open a visible browser, and hand the window
to the user. The user enters passwords, passkeys, and one-time codes. Never request, read, capture,
or type those values. Continue only after the user confirms login. If a visible session is
unavailable, stop instead of falling back to headless authentication.

## Minimal valid loops

Browser page content:

```bash
desktop --json browser doctor
desktop --json browser open 'https://example.com' --headless
desktop --json browser snapshot --interactive
desktop --json browser click @e1
desktop --json browser wait --load domcontentloaded --timeout 10000
desktop --json browser snapshot --interactive
desktop --json browser close
```

Existing GUI on the user's desktop:

```bash
desktop --no-steal-focus --json apps
desktop --no-steal-focus --json snapshot --app 'Calculator'
desktop --no-steal-focus --json click --element 7
desktop --no-steal-focus --json snapshot --app 'Calculator'
```

New isolated GUI:

```bash
desktop --json session list
desktop --json session start task-name --visible
desktop --json session run firefox 'https://example.com'
desktop --json snapshot --app Firefox
desktop --json session stop
```

The examples above show syntax, not permission to perform unrelated mutations. Authentication and
profile deletion retain the stricter boundaries above.
