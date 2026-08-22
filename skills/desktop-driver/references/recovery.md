# Errors and recovery

Use `--json` and branch on the process exit code plus the JSON `error` field.

| Exit | Meaning | Agent response |
|---:|---|---|
| 0 | Success | Verify the requested state. |
| 2 | Setup or unsupported capability | Run the relevant `doctor` or `capabilities`; follow its remedy. |
| 3 | Policy denied | Do not weaken the safety flag automatically. Choose a permitted element action or report the boundary. |
| 4 | Human interaction or permission required | Ask for the specific user action without requesting credentials. |
| 5 | Backend failure | Inspect diagnostics; retry only when the error says it is retryable. |
| 6 | Target or argument failure | Correct the selector or syntax; do not repeat unchanged. |
| 7 | Timeout | Read current state, then choose a more precise condition or report the timeout. |
| 70 | Internal invariant failure | Capture diagnostics and report the defect. |

## Diagnose the right interface

```bash
desktop --json capabilities
desktop --json doctor
desktop --json session status
desktop --json browser doctor
desktop --json browser status
```

- Accessibility tree empty or application missing: `desktop doctor`. Electron/Chromium may need
  `--force-renderer-accessibility` or `ACCESSIBILITY_ENABLED=1` when launched.
- Browser daemon, executable, engine, or profile issue: `desktop browser doctor` and `browser
  status` for the same `--profile`.
- Session start issue: `desktop doctor` names missing Xvfb, openbox, or D-Bus packages.

## Recover from common target failures

- `element_stale`: take a new snapshot and use its new ID or `@eN` reference.
- `ambiguous_selector`: narrow the selector. For browser commands, use exactly one strategy and
  make it unique; `get count` can confirm how many matched.
- `element_not_found`: snapshot current state before changing the selector.
- `element_not_actionable`: inspect whether it is hidden, disabled, obscured, readonly, or the
  wrong control; wait for the real state rather than switching to coordinates immediately.
- `selector_required`, `invalid_selector`, `value_required`, `attribute_required`, or
  `condition_required`: run that exact subcommand with `--help` and correct positional order.
- `invalid_key`: use one character or a documented named key. Do not type the rejected key name.
- `browser_engine_mismatch` or `browser_mode_mismatch`: do not force a running profile to change.
  Close it only if this task owns it, then reopen with the intended engine/mode.
- `browser_not_running`: confirm the profile name. Use `browser open` only when starting a managed
  browser is within scope.
- `visible_session_required`: start a matching visible session, or use headless only when no login
  or user handoff is involved.
- `password_field_denied`: hand the visible browser to the user. There is no agent bypass.

## Timeouts are evidence, not a retry instruction

After exit 7, inspect the current snapshot, URL, title, or session status. Retry only with a
condition supported by observed state, such as waiting for the destination URL before taking a new
snapshot. Do not add an arbitrary sleep or repeatedly increase the timeout without evidence.
