# Browser-native page automation

Use `desktop browser` for content inside Chromium or Firefox pages. It provides navigation waits,
DOM-backed selectors, page-scoped `@eN` references, tabs, dialogs, screenshots, and downloads.
Use ordinary desktop accessibility commands for the address bar, permission bubbles, browser menus,
and download UI.

## Start and choose a profile

```bash
desktop --json browser doctor
desktop --json browser open 'https://example.com' --headless
desktop --json browser open 'https://example.com' --browser firefox --headless --profile research
desktop --json browser status --profile research
desktop --json browser close --profile research
```

- Chromium is used for a new profile when `--browser` is omitted.
- A profile remembers Firefox or Chromium after the first successful open. Omit `--browser` when
  reopening it, or repeat the same value. Changing engine or headless/visible mode while it is
  running is rejected; close a browser you own before reopening it in another mode.
- `--profile NAME` selects a persistent browser profile. Without it, the active desktop session
  name is used, otherwise `default`.
- On Linux, visible `browser open` requires an active visible desktop session whose name matches
  the browser profile. Headless work does not require a visible session.
- `browser install` installs pinned Chrome for Testing. Firefox must already be installed.
- `browser connect ENDPOINT` accepts loopback endpoints only. Add `--browser firefox` for a
  WebDriver BiDi endpoint; otherwise it uses or remembers the profile engine.

Use this visible authentication flow for browser-native work:

```bash
desktop --json session start github --visible
desktop --json browser open 'https://github.com' --profile github
```

Pause automation and hand the visible window to the user for credentials. After confirmation,
continue with `--profile github`. Do not substitute `session run firefox`: that drives a regular
application profile through accessibility and is a different automation route.

## Selector grammar

An action accepts exactly one selector strategy:

```text
@e12
css=button[type="submit"]
xpath=//button[@type="submit"]
text=Continue
--role button --name 'Save'
--label 'Email address'
--test-id save-button
```

Rules:

- Prefer `@eN` from the latest `browser snapshot --interactive`.
- Quote the entire `css=...`, `xpath=...`, or `text=...` argument when it contains spaces or shell
  punctuation.
- Raw CSS without `css=` is invalid. `--name` is valid only together with `--role`.
- Do not combine a positional selector with `--role`, `--label`, or `--test-id`.
- Actions must resolve to one element. `get count` may intentionally resolve to several.
- Navigation, tab changes, and substantial DOM replacement invalidate old refs. Snapshot again.

## The reliable page loop

```bash
desktop --json browser goto 'https://example.com/form' --timeout 30000
desktop --json browser snapshot --interactive
desktop --json browser fill @e2 'Evandro'
desktop --json browser click @e3
desktop --json browser wait --url '/complete' --timeout 10000
desktop --json browser snapshot --interactive
desktop --json browser get text @e4
```

Take a new snapshot after any action that navigates, opens a tab, or substantially rerenders the
page. Use `--all --max-nodes N` only when the needed element is absent from the interactive view.

## Exact command shapes

Lifecycle and navigation:

```text
desktop browser install
desktop browser doctor [--profile NAME]
desktop browser open [URL] [--profile NAME] [--executable PATH] [--browser chromium|firefox] [--headless]
desktop browser connect ENDPOINT [--profile NAME] [--browser chromium|firefox]
desktop browser status [--profile NAME]
desktop browser close [--profile NAME]
desktop browser goto URL [--profile NAME] [--timeout MS]
desktop browser back|forward|reload [--profile NAME] [--timeout MS]
```

Reading:

```text
desktop browser snapshot [--interactive] [--all] [--max-nodes N] [--profile NAME]
desktop browser get text|html|value SELECTOR [--profile NAME]
desktop browser get attr SELECTOR ATTRIBUTE [--profile NAME]
desktop browser get count SELECTOR [--profile NAME]
desktop browser get title|url [--profile NAME]
desktop browser screenshot [--output PATH] [--full-page] [--profile NAME]
```

Actions. Positional order matters:

```text
desktop browser click SELECTOR [--profile NAME]
desktop browser fill SELECTOR VALUE [--profile NAME]
desktop browser type SELECTOR VALUE [--delay MS] [--profile NAME]
desktop browser press KEY [SELECTOR] [--profile NAME]
desktop browser select SELECTOR VALUE... [--profile NAME]
desktop browser check|uncheck|hover SELECTOR [--profile NAME]
desktop browser scroll [SELECTOR] [--x PX] [--y PX] [--profile NAME]
desktop browser download SELECTOR [--output DIRECTORY] [--profile NAME]
```

Option selectors work directly for commands with no later positional value:

```bash
desktop --json browser click --role button --name 'Save'
desktop --json browser get count 'css=.result'
desktop --json browser get text --label 'Email address'
```

For `fill`, `type`, `select`, and `get attr`, use a positional `@eN`, `css=...`, `xpath=...`, or
`text=...` selector so the following value or attribute cannot be mistaken for the optional target:

```bash
desktop --json browser fill @e2 'user@example.com'
desktop --json browser get attr 'css=a.download' href
desktop --json browser scroll --y -500
```

`fill` replaces the current value. `type` inserts characters into the current value. Password
fields are refused by both. `download` reports that the download started; verify the expected file
in the output directory before treating it as complete. Relative screenshot and download paths are
resolved from the invoking CLI process, but an explicit absolute path is clearer in agent logs.

`press` takes the key first and the optional selector second. It sends one key, not a chord; do not
invent values such as `Control+A`. A single character is valid. Named Firefox keys, including
accepted aliases, are:

```text
Null Unidentified Cancel Help Backspace Tab Clear Return Enter NumpadEnter
Shift ShiftLeft ShiftRight Control Ctrl ControlLeft CtrlLeft ControlRight CtrlRight
Alt Option AltLeft OptionLeft AltRight OptionRight Pause Escape Esc Space Spacebar
PageUp PgUp PageDown PgDown PgDn End Home
ArrowLeft Left ArrowUp Up ArrowRight Right ArrowDown Down Insert Delete Del
Semicolon Equals Equal NumpadEqual NumpadEquals
Numpad0 Numpad1 Numpad2 Numpad3 Numpad4 Numpad5 Numpad6 Numpad7 Numpad8 Numpad9
Multiply NumpadMultiply Add NumpadAdd Separator NumpadComma
NumpadSeparator Subtract NumpadSubtract Decimal NumpadDecimal Divide NumpadDivide
F1 F2 F3 F4 F5 F6 F7 F8 F9 F10 F11 F12
Meta Command Cmd Super MetaLeft MetaRight CommandRight CmdRight SuperRight
ZenkakuHankaku NumpadPageUp NumpadPageDown NumpadEnd NumpadHome
NumpadArrowLeft NumpadArrowUp NumpadArrowRight NumpadArrowDown NumpadInsert NumpadDelete
```

Treat `invalid_key` as final instead of typing the rejected key name as text.

## Waits: choose exactly one condition

```bash
desktop --json browser wait @e4 --timeout 10000
desktop --json browser wait 'css=.spinner' --hidden --timeout 10000
desktop --json browser wait --text 'Finished' --timeout 10000
desktop --json browser wait --url '/complete' --timeout 10000
desktop --json browser wait --load domcontentloaded --timeout 10000
desktop --json browser wait --load networkidle --timeout 30000
```

Choose one of selector, `--text`, `--url`, or `--load`; do not combine them. `--hidden` is meaningful
only with a selector. Valid load states are `load`, `domcontentloaded`, and `networkidle`.

## Tabs and dialogs

```bash
desktop --json browser tab list
desktop --json browser tab new 'https://example.com'
desktop --json browser tab use 2
desktop --json browser tab close 2
desktop --json browser tab close
desktop --json browser dialog accept
desktop --json browser dialog accept --prompt-text 'value'
desktop --json browser dialog dismiss
```

`tab use` and `tab close` accept a 1-based list index, target id, or title substring. Prefer the
index or target id because a title substring selects the first match. Take a new snapshot after
switching tabs. Dialog commands apply only while a JavaScript dialog is open.
