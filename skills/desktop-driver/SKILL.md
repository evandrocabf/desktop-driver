---
name: desktop-driver
description: See, inspect and control desktop GUI applications from the shell — read a window's widgets as text, click buttons by name, type into fields, take screenshots, and give the agent its own display so it never fights the user for the mouse. Use whenever the task involves a graphical application rather than a terminal one — a browser, an editor, a settings dialog, an installer, an Electron/GTK/Qt/Cocoa app — or asks to click something on screen, read what a window shows, fill in a form, screenshot an application, or check that a desktop UI is correct.
---

# Driving a desktop application with desktop-driver

A GUI cannot be driven by running it as a shell command: it opens a window, draws pixels, and
prints nothing useful. `desktop-driver` reads the same accessibility tree a screen reader uses, so
a window becomes a numbered list of widgets you can act on by name. Everything below is a shell
command.

**`desktop` is the instrument, never the subject.** The application you are driving is whatever the
project you are working in builds or depends on. Nothing here asks you to test desktop-driver
itself.

The command is `desktop`. `desktop doctor` checks the environment and names anything missing;
`desktop capabilities` says what this machine can and cannot do *before* you rely on it. Every
command takes `--json`.

## Decide first: whose screen?

This is the one decision that matters, and getting it wrong means typing into somebody's window.

**If the task is about an application the user already has open** — read their screen, fill a form
they opened, check what their editor shows — you are sharing one keyboard, one pointer and one
screen with a person. Work through elements and never touch the shared devices:

```bash
desktop --no-steal-focus snapshot --app Firefox     # a background window is fine
desktop --no-steal-focus click --element 12         # activates, no pointer movement
desktop --no-steal-focus type --element 23 "hello"  # writes into the field, no keystrokes
```

`--no-steal-focus` enforces it: element-addressed work goes through, and anything that would seize
the pointer or keyboard is refused with exit 3 rather than racing the user.

**If the task is to drive an application from scratch** — open a browser and check a page, exercise
an installer, test the app you are building — give the agent a display of its own:

```bash
desktop session create example      # a durable, isolated browser profile
desktop session start example --visible
desktop session run firefox https://example.com
desktop snapshot                     # the agent's windows, not the user's
desktop screenshot --output /tmp/shot.png
desktop session stop                 # when finished
```

**Authentication is a user handoff, never an agent task.** For a new login, create a named
session, start it with `--visible`, and launch the browser. Then ask the user to click into the
visible `desktop-driver` window and enter passwords, passkeys and one-time codes themselves.
Never ask them to paste credentials into chat, and never type credentials on their behalf. Wait
for the user to confirm that login is complete before continuing. If `--visible` cannot be
provided, stop and explain why; do not fall back to a headless credential flow.

Inside a session nothing is shared, so focus, clicks, typing and screenshots all work and none of
them can reach the user's screen. **Once a session exists every command addresses it by default**
and says so — human output starts with `[agent display :90]`, JSON carries a `"display"` field.
`--host` targets the user's real desktop for a single command.

A named session also gets its own persistent home directory, so a browser reuses its cookies and
saved login after `session stop` and a later `session start <name>`. Different names do not share
profiles. `desktop session delete <name>` permanently removes that state. `--share-home` applies
only to the backwards-compatible `default` session and should not be used for a credential handoff.

**The user can watch by default.** A session opens as a window titled `desktop-driver` on their
desktop, so they can see what you are doing and click in to take over. That changes nothing about
the isolation — still a separate X server with its own pointer, keyboard and framebuffer, your
input still cannot reach their applications, your screenshots still contain only your own windows.

Where a window is impossible — a CI runner with no display, or a machine without Xephyr — it
starts headless instead and says so. `desktop session status` reports which you have. Pass
`--headless` to opt out deliberately (a long unattended run), or `--visible` to refuse to start at
all unless the user can watch.

Sessions are Linux-only. On macOS `desktop session status` says so; use `--no-steal-focus`.

## The loop

```bash
desktop apps                             # what is running and can be inspected
desktop snapshot --app "Calculator"      # the window as a numbered widget list
desktop click --element 7                # act by id from that snapshot
desktop snapshot --app "Calculator"      # verify by reading, not by assuming
```

A snapshot looks like this, and the ids are what you act on:

```
Application: Calculator
Window: Calculator

[1] textbox = "42"
[5] button "7"
[23] button "+"
[27] button "="
```

Ids come from the last snapshot, so **snapshot before you act**. They are re-resolved against the
live tree on use: an element that moved is still found, one that was replaced is reported
`element_stale` rather than clicked at a stale position.

## Reading

```bash
desktop apps                                  # applications with an accessibility tree
desktop windows --app Firefox
desktop snapshot --app Firefox                # pruned, numbered, token-cheap — start here
desktop snapshot --app Firefox --all          # include off-screen elements
desktop snapshot --max-nodes 300              # bound a huge tree (a browser page is enormous)
desktop inspect --app Firefox                 # the raw tree, before pruning; for debugging only
desktop find --role button --name Save        # search the last snapshot
desktop find --text "Continue"
desktop wait --text "Build complete" --timeout 10000
```

`wait` re-snapshots until the selector matches, so ids are fresh afterwards. Use it instead of
sleeping.

To *look* at the application rather than read it — layout, colour, a canvas, anything the tree
cannot express — take a screenshot and open the file with whatever image tool you have:

```bash
desktop screenshot --output /tmp/shot.png
desktop screenshot --app Firefox --output /tmp/ff.png
```

Text is cheaper and usually enough. Reach for pixels when the question is visual.

## Acting

Prefer the accessibility action. It is deterministic, needs no coordinates, and moves neither the
pointer nor the focus:

```bash
desktop click --element 7                     # by snapshot id (preferred)
desktop click --role button --name "Save"     # by selector, no snapshot id needed
desktop click --text "Continue"
desktop type --element 23 "user@example.com"  # straight into the field
```

Keyboard and pointer, for when there is no element to address:

```bash
desktop focus --app Firefox
desktop key "ctrl+l"                          # accel = Cmd on macOS, Ctrl on Linux
desktop type "hello world"                    # goes to whatever has focus
desktop move  --x 800 --y 400
desktop click --x 800 --y 400 --button right --count 2
desktop scroll --y -500                       # negative scrolls up, like a page moves
desktop click --element 7 --via pointer       # force coordinates for a widget that needs a real click
```

## Coordinates are often unavailable — check before aiming

`bounds` is `null` whenever the toolkit did not report a position, which is **most GTK4
applications, and everything under Wayland**. It is never a fabricated `0,0`.

So do not compute a click position from a snapshot without looking:

```bash
desktop snapshot --json | jq '.elements[] | select(.bounds != null)'
```

If `bounds` is null, use `--element` or a selector. If you truly need coordinates, take a
screenshot and read them off the image — that is always correct, because it is what is on screen.

## Before you rely on something

```bash
desktop capabilities            # ✓ works · ~ works with a caveat · ✗ not available here
desktop doctor                  # why the tree is empty, and the exact install command
desktop info                    # platform, display server, chosen backends
```

Capabilities differ enormously by environment and the tool refuses rather than pretending. On GNOME
Wayland, for instance, `focus` is genuinely impossible and window screenshots need a human to pick
the window — but both work inside `desktop session`. Check rather than assume, and if something is
`✗`, an agent session is usually the answer.

## Exit codes

`0` success · `2` setup or configuration (including `unsupported_capability`) · `3` policy denied
· `4` interaction required (a permission grant is missing) · `5` backend failure · `6` target
failure (no such element, stale, ambiguous, bad argument) · `7` timeout · `70` internal.

Branch on these rather than parsing output. In `--json`, errors are a single object with an
`"error"` field naming the kind.

## Rules that matter

1. **Snapshot before acting, and read after acting.** Ids come from a snapshot, and reading back is
   how you know the click did what you expected.
2. **Prefer `--element` and selectors over `--x/--y`.** Coordinates are a fallback, and often not
   even available.
3. **Never `sleep` and hope** — `desktop wait --text ...` is the synchronising primitive.
4. **On the user's desktop, always `--no-steal-focus`.** Anything else races a person.
5. **Stop sessions you start.** They hold an X server and everything running on it until stopped.
6. **Passwords are never readable.** Secure fields come back `redacted: true` with a null value, by
   design and not as policy — do not try to work around it.
7. **For login, hand the visible window to the user.** Never request, observe or type their
   credentials. Continue only after the user says the login is complete.

## Safety flags

```bash
desktop --read-only snapshot           # refuse anything that would change the desktop
desktop --no-steal-focus ...           # refuse anything that seizes pointer or keyboard
desktop --deny-app 1Password ...       # refuse operations on named applications
desktop --deny-role password ...       # refuse actions on password fields
desktop --host ...                     # the user's desktop even when a session is running
```

## Troubleshooting

- **`desktop apps` is empty or an app is missing** — run `desktop doctor`. Usually the accessibility
  bus is off (`desktop setup` turns it on) or the app is Electron/Chromium, which builds no tree
  until told: launch it with `--force-renderer-accessibility` or `ACCESSIBILITY_ENABLED=1`.
- **A window has no contents** — same cause. Firefox, Chromium and Qt all build their tree lazily.
- **`element_stale`** — the UI changed under you. Re-snapshot and use the new id.
- **`unsupported_capability`** — that operation genuinely cannot work here. `desktop capabilities`
  explains why; `desktop session` usually can do it.
- **Typing goes to the wrong window** — you are on a shared desktop. `desktop focus` first, or
  better, use `--element`, or better still, work in a session.
- **Clicks land in the wrong place** — you used coordinates from a snapshot whose `bounds` were
  null, or from a different screen size. Screenshot and re-measure.
- **`desktop session start` fails** — `desktop doctor` names the missing package (`Xvfb`, `openbox`,
  `dbus-daemon`) and the install command for that distribution.
