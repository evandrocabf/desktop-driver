## What this changes

<!-- One or two sentences. What behaviour is different afterwards? -->

## Why

<!-- The problem this solves. Link an issue if there is one. -->

## How it was verified

<!--
What you actually ran, and what you observed. If it touches how a desktop is read or driven,
paste the before/after — the snapshot, the capability line, the commands.
-->

## Checklist

- [ ] `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` pass
- [ ] `cargo test --workspace` passes
- [ ] `cargo xtask architecture` passes
- [ ] A test covers this, and it would have failed before the change
- [ ] Checked the other platform: `cargo clippy --target aarch64-apple-darwin --workspace --all-targets`
- [ ] `scripts/distro-matrix.sh` run, if this changes how a session is built
- [ ] `skills/desktop-driver/SKILL.md` updated if the agent-facing behaviour changed
- [ ] `README.md` updated if a command, flag or capability changed
- [ ] `CHANGELOG.md` updated under `Unreleased`
