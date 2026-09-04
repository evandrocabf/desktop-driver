#!/usr/bin/env bash
#
# desktop-driver installer.
#
#   curl -fsSL https://raw.githubusercontent.com/evandrocabf/desktop-driver/main/install.sh | bash
#   ./install.sh --agents codex,cursor
#   ./install.sh --project .
#
# It does two separable things:
#
#   1. puts the `desktop` binary on your PATH — always built from the checked
#      out repository on macOS; Linux may use an existing release;
#   2. installs skills/desktop-driver/ wherever coding agents look for skills;
#      agents without a directory skill loader receive one flattened file.
#
# Piped through `curl | bash`, the source arrives as a tarball when git is
# absent. macOS always needs Cargo because it compiles that source locally;
# Linux may use an existing binary release. Every route says which
# one it took.
#
# Everything it writes is named as it writes it, `--dry-run` shows the plan
# without touching anything, and `--uninstall` removes exactly what was added.

set -eu

ORIGINAL_ARGS=("$@")

REPO_URL="${DESKTOP_DRIVER_REPO:-https://github.com/evandrocabf/desktop-driver.git}"
SKILL_NAME="desktop-driver"
MARKER="desktop-driver-installer"
COPY_STAMP=".desktop-driver-installed"
BIN_NAME="desktop"

MIN_RUST="1.97.1"

# ── options ──────────────────────────────────────────────────────────────────

AGENTS_ARG=""
INSTALL_ALL=0
NO_AGENTS=0
NO_BIN=0
NO_BUILD=0
NO_SETUP=0
PROJECT_DIR=""
PREFIX="${XDG_BIN_HOME:-$HOME/.local/bin}"
SRC_DIR_ARG=""
GIT_REF=""
COPY=0
FORCE=0
DRY_RUN=0
UNINSTALL=0
UPDATE=0
STATIC=0
FROM_SOURCE=0

# ── output ───────────────────────────────────────────────────────────────────

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  B=$(printf '\033[1m'); DIM=$(printf '\033[2m'); R=$(printf '\033[0m')
  GRN=$(printf '\033[32m'); YEL=$(printf '\033[33m'); RED=$(printf '\033[31m')
else
  B=""; DIM=""; R=""; GRN=""; YEL=""; RED=""
fi

say()  { printf '%s\n' "$*"; }
step() { printf '%s%s%s\n' "$B" "$*" "$R"; }
info() { printf '  %s\n' "$*"; }
ok()   { printf '  %s✓%s %s\n' "$GRN" "$R" "$*"; }
skip() { printf '  %s·%s %s%s%s\n' "$DIM" "$R" "$DIM" "$*" "$R"; }
warn() { printf '  %s!%s %s\n' "$YEL" "$R" "$*" >&2; }
die()  { printf '%serror:%s %s\n' "$RED" "$R" "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# Fails, having first removed a scratch directory. `die` exits, so a trap on
# RETURN would never fire and a half-downloaded tree would be left behind on
# every failed attempt.
scrub() { local dir="$1"; shift; rm -rf "$dir"; die "$@"; }

# Every path this script keeps is absolute.
#
# A relative one survives as far as the first `ln -s`, where it is written into
# the link verbatim and then resolved against the *link's* directory rather than
# against the working directory it was typed in — a skill link that silently
# points at nothing. The same relative path reaches the PATH advice at the end,
# where a `.`-relative entry runs whatever happens to be in the current
# directory. Neither is recoverable later, so nothing is stored relative.
#
# Does not require the path to exist: --src names a directory yet to be cloned.
absolute() {
  case "$1" in
    /*) printf '%s' "$1" ;;
    "") printf '' ;;
    *)  printf '%s/%s' "$(pwd)" "${1#./}" ;;
  esac
}

# Confirmation for something that was actually done. In a dry run the `would:`
# lines already narrate the plan, so a ✓ on top of them would just be a lie.
did()  { if [ "$DRY_RUN" -eq 0 ]; then ok "$@"; fi; }

# Every filesystem mutation goes through here, so --dry-run stays honest by
# construction rather than by remembering to check the flag at each call site.
act() {
  if [ "$DRY_RUN" -eq 1 ]; then
    info "${DIM}would: $*${R}"
  else
    "$@"
  fi
}

usage() {
  cat <<'EOF'
desktop-driver installer

  install.sh [options]

Options
  --agents LIST     comma-separated: claude,codex,cursor,opencode,gemini,agents,cline,windsurf
  --all             install the skill for every known agent, detected or not
  --no-agents       skip the skill entirely, just install the binary
  --no-bin          skip the binary, just install the skill
  --no-build        do not run cargo; use an already-built target/release/desktop
  --no-setup        skip the interactive macOS permission requests
  --from-source     always compile, even where a released binary is available
  --static          build a static musl binary (runs on any Linux, any glibc)
  --project DIR     install into DIR/.claude/... instead of your home directory
  --prefix DIR      where to put the binary (default: ~/.local/bin)
  --src DIR         use or clone the checkout here
  --ref REF         clone/checkout this branch or tag
  --copy            copy the skill instead of symlinking it
  --force           overwrite files this installer did not create
  --dry-run         print the plan, write nothing
  --uninstall       remove what this installer added
  --update          refresh the installed checkout before reinstalling
  -h, --help        this
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --agents)    AGENTS_ARG="${2:-}"; shift 2 ;;
    --all)       INSTALL_ALL=1; shift ;;
    --no-agents) NO_AGENTS=1; shift ;;
    --no-bin)    NO_BIN=1; shift ;;
    --no-build)  NO_BUILD=1; shift ;;
    --no-setup)  NO_SETUP=1; shift ;;
    --from-source) FROM_SOURCE=1; shift ;;
    --static)    STATIC=1; shift ;;
    --project)   PROJECT_DIR="${2:-}"; shift 2 ;;
    --prefix)    PREFIX="${2:-}"; shift 2 ;;
    --src)       SRC_DIR_ARG="${2:-}"; shift 2 ;;
    --ref)       GIT_REF="${2:-}"; shift 2 ;;
    --copy)      COPY=1; shift ;;
    --force)     FORCE=1; shift ;;
    --dry-run)   DRY_RUN=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --update)    UPDATE=1; shift ;;
    -h|--help)   usage; exit 0 ;;
    *)           die "unknown option: $1 (try --help)" ;;
  esac
done

# ── the agent table ──────────────────────────────────────────────────────────
#
# Paths come from each tool's own documentation. `~/.agents/skills` is the
# cross-tool convention that opencode, Cursor and Gemini/Antigravity all read,
# so it is worth having even when no single agent is detected.

ALL_AGENTS="claude codex cursor opencode gemini agents cline windsurf"

agent_label() {
  case "$1" in
    claude)   echo "Claude Code" ;;
    codex)    echo "Codex CLI" ;;
    cursor)   echo "Cursor" ;;
    opencode) echo "opencode" ;;
    gemini)   echo "Gemini CLI / Antigravity" ;;
    agents)   echo "AGENTS.md standard (.agents)" ;;
    cline)    echo "Cline" ;;
    windsurf) echo "Windsurf" ;;
    *)        echo "$1" ;;
  esac
}

# "dir"  → the skill directory is linked whole and loaded on demand.
# "file" → the agent has no skill loader, only an always-on rules directory,
#          so a single markdown file goes there instead.
agent_kind() {
  case "$1" in
    cline|windsurf) echo "file" ;;
    *)              echo "dir" ;;
  esac
}

cline_rules_dir() {
  if [ -d "$HOME/Cline/Rules" ] && [ ! -d "$HOME/Documents/Cline/Rules" ]; then
    echo "$HOME/Cline/Rules"
  else
    echo "$HOME/Documents/Cline/Rules"
  fi
}

agent_home_target() {
  case "$1" in
    claude)   echo "$HOME/.claude/skills/$SKILL_NAME" ;;
    codex)    echo "$HOME/.codex/skills/$SKILL_NAME" ;;
    cursor)   echo "$HOME/.cursor/skills/$SKILL_NAME" ;;
    opencode) echo "${XDG_CONFIG_HOME:-$HOME/.config}/opencode/skills/$SKILL_NAME" ;;
    gemini)   echo "$HOME/.gemini/skills/$SKILL_NAME" ;;
    agents)   echo "$HOME/.agents/skills/$SKILL_NAME" ;;
    cline)    echo "$(cline_rules_dir)/$SKILL_NAME.md" ;;
    # Windsurf's only global slot is memories/global_rules.md: one shared file,
    # always on, capped at 6000 characters. SKILL.md neither fits nor is that
    # file ours to own, so Windsurf is project-scoped only.
    windsurf) echo "" ;;
  esac
}

agent_project_target() {
  case "$1" in
    claude)   echo "$PROJECT_DIR/.claude/skills/$SKILL_NAME" ;;
    codex)    echo "$PROJECT_DIR/.codex/skills/$SKILL_NAME" ;;
    cursor)   echo "$PROJECT_DIR/.cursor/skills/$SKILL_NAME" ;;
    opencode) echo "$PROJECT_DIR/.opencode/skills/$SKILL_NAME" ;;
    gemini)   echo "$PROJECT_DIR/.gemini/skills/$SKILL_NAME" ;;
    agents)   echo "$PROJECT_DIR/.agents/skills/$SKILL_NAME" ;;
    cline)    echo "$PROJECT_DIR/.clinerules/$SKILL_NAME.md" ;;
    windsurf) echo "$PROJECT_DIR/.windsurf/rules/$SKILL_NAME.md" ;;
  esac
}

agent_target() {
  if [ -n "$PROJECT_DIR" ]; then agent_project_target "$1"; else agent_home_target "$1"; fi
}

agent_detected() {
  if [ -n "$PROJECT_DIR" ]; then
    case "$1" in
      claude)   [ -d "$PROJECT_DIR/.claude" ] ;;
      codex)    [ -d "$PROJECT_DIR/.codex" ] || [ -e "$PROJECT_DIR/AGENTS.md" ] ;;
      cursor)   [ -d "$PROJECT_DIR/.cursor" ] ;;
      opencode) [ -d "$PROJECT_DIR/.opencode" ] ;;
      gemini)   [ -d "$PROJECT_DIR/.gemini" ] ;;
      agents)   [ -d "$PROJECT_DIR/.agents" ] ;;
      cline)    [ -d "$PROJECT_DIR/.clinerules" ] ;;
      windsurf) [ -d "$PROJECT_DIR/.windsurf" ] ;;
      *)        false ;;
    esac
    return
  fi
  case "$1" in
    claude)   [ -d "$HOME/.claude" ] || have claude ;;
    codex)    [ -d "$HOME/.codex" ] || have codex ;;
    cursor)   [ -d "$HOME/.cursor" ] || have cursor-agent ;;
    opencode) [ -d "${XDG_CONFIG_HOME:-$HOME/.config}/opencode" ] || have opencode ;;
    gemini)   [ -d "$HOME/.gemini" ] || [ -d "$HOME/.antigravity" ] || have gemini ;;
    agents)   [ -d "$HOME/.agents" ] ;;
    cline)    [ -d "$HOME/Documents/Cline/Rules" ] || [ -d "$HOME/Cline/Rules" ] ;;
    windsurf) [ -d "$HOME/.codeium/windsurf" ] ;;
    *)        false ;;
  esac
}

known_agent() {
  local a
  for a in $ALL_AGENTS; do
    if [ "$a" = "$1" ]; then return 0; fi
  done
  return 1
}

# ── locating the source checkout ─────────────────────────────────────────────

is_checkout() {
  [ -f "$1/Cargo.toml" ] &&
    [ -d "$1/crates/desktop-cli" ] &&
    [ -f "$1/skills/$SKILL_NAME/SKILL.md" ]
}

# A subdirectory, not the namespace itself: `desktop session` keeps the agent's
# private homes under $XDG_DATA_HOME/desktop-driver/sessions, so cloning over the parent
# either refuses to run — on any machine where a session came first — or, worse,
# succeeds and leaves the agent's browser profile living inside a git working
# tree, where `git status` reports it and `git clean -xfd` deletes it.
default_src_dir() {
  echo "${XDG_DATA_HOME:-$HOME/.local/share}/desktop-driver/checkout"
}

resolve_source() {
  local self script_dir
  # bootstrap_update runs from a temporary copy, so BASH_SOURCE no longer
  # identifies the checkout the user invoked. Carry its classification across
  # that boundary: a Git checkout must still install its exact source after it
  # is pulled, rather than becoming eligible for a same-version release asset.
  if [ "${DESKTOP_DRIVER_UPDATE_SOURCE_MODE:-}" = "local" ] &&
     [ "${DESKTOP_DRIVER_UPDATE_BOOTSTRAPPED:-0}" -eq 1 ] &&
     [ -n "$SRC_DIR_ARG" ]; then
    SRC="$SRC_DIR_ARG"
    SRC_MODE="local"
    return
  fi
  # Empty when piped through `curl | bash`, which is exactly when we must clone.
  self="${BASH_SOURCE[0]:-}"
  if [ -n "$self" ] && [ -f "$self" ]; then
    script_dir="$(cd "$(dirname "$self")" && pwd)"
    if is_checkout "$script_dir" && [ "$UPDATE" -eq 0 ] && [ -z "$GIT_REF" ] && [ -z "$SRC_DIR_ARG" ]; then
      SRC="$script_dir"
      SRC_MODE="local"
      return
    fi
  fi
  SRC="$(absolute "${SRC_DIR_ARG:-$(default_src_dir)}")"
  SRC_MODE="clone"
}

# Updating a tarball checkout overwrites install.sh itself. Bash does not
# promise to have read the whole file before that happens, so run the updater
# from a temporary copy that is outside the checkout. The original checkout
# path is passed explicitly because the temporary script is not in a source tree.
bootstrap_update() {
  local self script_dir tmp status source_mode
  [ "$UPDATE" -eq 1 ] || return 0
  [ "${DESKTOP_DRIVER_UPDATE_BOOTSTRAPPED:-0}" -eq 0 ] || return 0
  self="${BASH_SOURCE[0]:-}"
  [ -n "$self" ] && [ -f "$self" ] || return 0
  script_dir="$(cd "$(dirname "$self")" && pwd)"
  is_checkout "$script_dir" || return 0

  tmp="$(mktemp /tmp/desktop-driver-update.XXXXXX)"
  cp "$self" "$tmp"
  chmod 700 "$tmp"
  source_mode="clone"
  if [ -z "$SRC_DIR_ARG" ] && [ -d "$script_dir/.git" ]; then
    source_mode="local"
  fi
  if [ -z "$SRC_DIR_ARG" ]; then
    if DESKTOP_DRIVER_UPDATE_BOOTSTRAPPED=1 \
       DESKTOP_DRIVER_UPDATE_SOURCE_MODE="$source_mode" \
       bash "$tmp" "${ORIGINAL_ARGS[@]}" --src "$script_dir"; then
      status=0
    else
      status=$?
    fi
  else
    if DESKTOP_DRIVER_UPDATE_BOOTSTRAPPED=1 bash "$tmp" "${ORIGINAL_ARGS[@]}"; then
      status=0
    else
      status=$?
    fi
  fi
  rm -f "$tmp"
  exit "$status"
}

# owner/repo out of REPO_URL, or failure when it does not name GitHub — a
# mirror, a fork over ssh, a path on disk. Everything downloaded rather than
# cloned is derived from this, so a repository that cannot be named here simply
# keeps the git path.
repo_slug() {
  local slug=""
  case "$REPO_URL" in
    https://github.com/*) slug="${REPO_URL#https://github.com/}" ;;
    git@github.com:*)     slug="${REPO_URL#git@github.com:}" ;;
    *) return 1 ;;
  esac
  printf '%s' "${slug%.git}"
}

# The source, without git.
#
# `curl | bash` on a machine that has never cloned anything is the case this
# exists for: git is a 40MB dependency for a copy of a directory, and codeload
# hands over the same tree in one request. The tarball cannot be updated in
# place the way a clone can, so it is unpacked over whatever was there — which
# is only ever a directory this installer wrote, guarded below.
fetch_tarball() {
  local url tmp root
  url="https://codeload.github.com/$(repo_slug)/tar.gz/${GIT_REF:-HEAD}"

  step "Downloading the source"
  if have git; then
    info "${DIM}$SRC came down as a tarball rather than a clone, so this refreshes it the same way${R}"
  else
    info "${DIM}git is not installed, so this is a tarball rather than a clone${R}"
  fi
  if [ "$DRY_RUN" -eq 1 ]; then
    info "${DIM}would: replace $SRC with the source from $url, preserving target/${R}"
    return
  fi

  tmp="$(mktemp -d)"
  curl -fsSL "$url" -o "$tmp/source.tar.gz" ||
    scrub "$tmp" "cannot download $url (a private repository needs git and its credentials)"
  tar -xzf "$tmp/source.tar.gz" -C "$tmp" ||
    scrub "$tmp" "the downloaded archive could not be unpacked"

  root=""
  for candidate in "$tmp"/*/; do
    if [ -d "$candidate" ]; then root="${candidate%/}"; break; fi
  done
  [ -n "$root" ] || scrub "$tmp" "the downloaded archive was empty"
  is_checkout "$root" ||
    scrub "$tmp" "the downloaded archive is not a desktop-driver source tree"

  if is_checkout "$SRC"; then
    replace_tarball_checkout "$root"
  else
    mkdir -p "$(dirname "$SRC")"
    mv "$root" "$SRC"
  fi
  rm -rf "$tmp"
  did "unpacked into $SRC"
}

# Replace a tarball checkout as a tree instead of copying the new archive over
# it. Overlaying cannot remove files deleted upstream, which leaves linked and
# copied skills exposing obsolete resources. The build cache is the sole local
# directory retained; both staging directories live beside SRC so the final
# swaps are filesystem-local renames.
replace_tarball_checkout() {
  local root="$1" parent stage backup
  parent="$(dirname "$SRC")"
  stage="$(mktemp -d "$parent/.desktop-driver-new.XXXXXX")"
  backup="$(mktemp -d "$parent/.desktop-driver-old.XXXXXX")"
  rmdir "$backup"

  cp -R "$root/." "$stage/" || scrub "$stage" "cannot stage the downloaded source"
  if ! mv "$SRC" "$backup"; then
    scrub "$stage" "cannot move the existing checkout aside"
  fi

  if [ -d "$backup/target" ]; then
    rm -rf "$stage/target"
    if ! mv "$backup/target" "$stage/target"; then
      mv "$backup" "$SRC" 2>/dev/null || true
      scrub "$stage" "cannot preserve the existing build cache"
    fi
  fi

  if ! mv "$stage" "$SRC"; then
    if [ -d "$stage/target" ]; then
      mv "$stage/target" "$backup/target" 2>/dev/null || true
    fi
    mv "$backup" "$SRC" 2>/dev/null || true
    scrub "$stage" "cannot replace the existing tarball checkout"
  fi
  rm -rf "$backup"
}

# The source, by whichever route this machine supports.
#
# git is preferred where it is present *and* the tree it would act on is one it
# can act on. A tree that arrived as a tarball has no .git in it, and asking git
# to clone into a directory that already exists fails outright — so once a tree
# is a tarball it stays one, refreshed the same way it arrived.
clone_or_update() {
  local tarball_only=0
  if ! have git; then tarball_only=1; fi
  if [ ! -d "$SRC/.git" ] && is_checkout "$SRC"; then tarball_only=1; fi

  if [ "$tarball_only" -eq 1 ]; then
    have tar  || die "git or tar is required to fetch the source"
    have curl || die "git or curl is required to fetch the source"
    repo_slug >/dev/null ||
      die "git is required to fetch the source from $REPO_URL"
    if [ -e "$SRC" ] && ! is_checkout "$SRC"; then
      die "$SRC exists and is not a desktop-driver checkout"
    fi
    fetch_tarball
    return
  fi

  if [ -d "$SRC/.git" ]; then
    step "Updating $SRC"
    act git -C "$SRC" fetch --quiet --tags origin
    if [ -n "$GIT_REF" ]; then
      act git -C "$SRC" checkout --quiet "$GIT_REF"
      act git -C "$SRC" pull --quiet --ff-only origin "$GIT_REF" 2>/dev/null || true
    else
      act git -C "$SRC" pull --quiet --ff-only
    fi
    did "updated"
    return
  fi

  if [ -e "$SRC" ] && ! is_checkout "$SRC"; then
    die "$SRC exists and is not a desktop-driver checkout"
  fi

  step "Cloning $REPO_URL"
  act mkdir -p "$(dirname "$SRC")"
  if [ -n "$GIT_REF" ]; then
    act git clone --quiet --depth 1 --branch "$GIT_REF" "$REPO_URL" "$SRC"
  else
    act git clone --quiet --depth 1 "$REPO_URL" "$SRC"
  fi
  did "cloned into $SRC"
}

# ── dependency checks ────────────────────────────────────────────────────────

# Deliberately not `sort -V`: BSD sort on older macOS does not have it.
version_at_least() {
  local have_v="$1" want_v="$2" oldifs h1 h2 h3 w1 w2 w3
  oldifs="${IFS:- }"
  IFS='.'
  # shellcheck disable=SC2086
  set -- $have_v; h1="${1:-0}"; h2="${2:-0}"; h3="${3:-0}"
  # shellcheck disable=SC2086
  set -- $want_v; w1="${1:-0}"; w2="${2:-0}"; w3="${3:-0}"
  IFS="$oldifs"

  h1=$(digits "$h1"); h2=$(digits "$h2"); h3=$(digits "$h3")
  w1=$(digits "$w1"); w2=$(digits "$w2"); w3=$(digits "$w3")

  if [ "$h1" -ne "$w1" ]; then [ "$h1" -gt "$w1" ]; return; fi
  if [ "$h2" -ne "$w2" ]; then [ "$h2" -gt "$w2" ]; return; fi
  [ "$h3" -ge "$w3" ]
}

digits() {
  local n
  n="$(printf '%s' "${1:-0}" | tr -cd '0-9')"
  # 10# keeps "08" from being read as an invalid octal literal.
  printf '%s' "$((10#${n:-0}))"
}

# One place that knows how to name a package for whatever this is.
package_hint() {
  local pkgs="$1"
  case "$(uname -s)" in
    Darwin) echo "brew install $pkgs"; return ;;
  esac
  if have dnf;    then echo "sudo dnf install $pkgs";    return; fi
  if have apt-get; then echo "sudo apt install $pkgs";   return; fi
  if have pacman; then echo "sudo pacman -S $pkgs";      return; fi
  if have zypper; then echo "sudo zypper install $pkgs"; return; fi
  echo "install: $pkgs"
}

check_deps() {
  step "Checking dependencies"

  local v
  if have cargo; then
    v="$(cargo --version 2>/dev/null | awk '{print $2}')"
    if [ -z "$v" ]; then
      # rustup on PATH with no toolchain behind it: `cargo` exists, answers
      # nothing, and fails at the first build. Reported as missing rather than
      # as "cargo  is older than 1.97.1", which is what an empty version read
      # as when it reached the comparison.
      CARGO_MISSING=1
      warn "cargo is on PATH but no toolchain is installed behind it:"
      info "      rustup default stable"
    elif version_at_least "$v" "$MIN_RUST"; then
      ok "cargo $v"
    else
      warn "cargo $v is older than the required $MIN_RUST — upgrade with: rustup update"
    fi
  elif [ "$NO_BUILD" -eq 1 ]; then
    skip "cargo (not needed with --no-build)"
  elif [ "$(uname -s)" != "Darwin" ] && [ "$SRC_MODE" = "clone" ] &&
       [ "$FROM_SOURCE" -eq 0 ] &&
       release_target >/dev/null && repo_slug >/dev/null; then
    skip "cargo (not needed unless the download falls through to a source build)"
  else
    CARGO_MISSING=1
    warn "cargo is not installed. desktop-driver is built from source; install Rust with:"
    info "      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  fi

  # Everything below is about *running* it, and differs by platform.
  if [ "$(uname -s)" = "Darwin" ]; then
    skip "no packages needed on macOS — permissions are granted in System Settings"
    return
  fi

  # The one hard requirement: without it there is no accessibility tree and
  # nothing works at all.
  if [ -e /usr/lib/at-spi-bus-launcher ] ||
     [ -e /usr/libexec/at-spi-bus-launcher ] ||
     [ -e /usr/libexec/at-spi2/at-spi-bus-launcher ] ||
     have at-spi-bus-launcher; then
    ok "at-spi2-core"
  else
    ATSPI_MISSING=1
    warn "at-spi2-core is not installed — without it no UI tree can be read:"
    info "      $(package_hint at-spi2-core)"
  fi

  # Optional, and only for `desktop session`. Named individually because a
  # partial install is the confusing case.
  local session_missing=""
  have Xvfb        || session_missing="$session_missing Xvfb"
  have openbox     || session_missing="$session_missing openbox"
  have dbus-daemon || session_missing="$session_missing dbus-daemon"

  if [ -z "$session_missing" ]; then
    ok "Xvfb, openbox, dbus-daemon ${DIM}(agent sessions)${R}"
  else
    local pkgs=""
    case "$session_missing" in *Xvfb*)
      if have dnf || have zypper; then pkgs="$pkgs xorg-x11-server-Xvfb"
      elif have pacman;            then pkgs="$pkgs xorg-server-xvfb"
      else                              pkgs="$pkgs xvfb"; fi ;;
    esac
    case "$session_missing" in *openbox*) pkgs="$pkgs openbox" ;; esac
    case "$session_missing" in *dbus-daemon*)
      if have apt-get;   then pkgs="$pkgs dbus-bin"
      elif have zypper;  then pkgs="$pkgs dbus-1-daemon"
      elif have pacman;  then pkgs="$pkgs dbus"
      else                    pkgs="$pkgs dbus-daemon"; fi ;;
    esac
    warn "missing for \`desktop session\`:$session_missing"
    info "      $(package_hint "$(echo "$pkgs" | sed 's/^ *//')")"
    info "${DIM}      optional — everything except agent sessions works without it${R}"
  fi
}

# ── the binary ───────────────────────────────────────────────────────────────

MANIFEST_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/desktop-driver"
MANIFEST="$MANIFEST_DIR/installed-binary"

build_target_dir() {
  if [ "$STATIC" -eq 1 ]; then
    echo "$SRC/target/x86_64-unknown-linux-musl/release"
  else
    echo "$SRC/target/release"
  fi
}

# The optional Linux release asset this machine can run, or failure where there
# is no such build. Names are cargo target triples, which is what `rustc -vV`
# calls this machine. macOS deliberately has no entry and never takes this route.
release_target() {
  case "$(uname -s):$(uname -m)" in
    Linux:x86_64)              echo "x86_64-unknown-linux-musl" ;;
    Linux:aarch64|Linux:arm64) echo "aarch64-unknown-linux-musl" ;;
    *) return 1 ;;
  esac
}

# Tries to fetch a released binary outside macOS, and reports whether it got one.
#
# Every failure here is soft. No release yet, an architecture nobody publishes
# for, a repository that is not on GitHub, a machine with no network — all of
# them mean "compile it instead", which is what this installer did before
# releases existed. The one thing that is *not* soft is a checksum that does not
# match: that is a corrupted or tampered download, and falling back to a source
# build would bury it.
download_binary() {
  local target slug url tmp sum_url expected actual expected_version actual_version

  [ "$FROM_SOURCE" -eq 0 ] || return 1
  [ "$STATIC" -eq 0 ] || return 1
  [ "$(uname -s)" != "Darwin" ] || return 1
  # Run from a checkout, the checkout is the point. Someone standing in their
  # own working tree typed ./install.sh to install *that*, and handing them a
  # released binary would quietly discard whatever they had just changed.
  [ "$SRC_MODE" = "clone" ] || return 1
  have curl || return 1
  have tar  || return 1
  target="$(release_target)" || return 1
  slug="$(repo_slug)" || return 1

  if [ -n "$GIT_REF" ]; then
    url="https://github.com/$slug/releases/download/$GIT_REF/$BIN_NAME-$target.tar.gz"
  else
    url="https://github.com/$slug/releases/latest/download/$BIN_NAME-$target.tar.gz"
  fi

  step "Downloading a released binary"
  if [ "$DRY_RUN" -eq 1 ]; then
    info "${DIM}would: curl -fsSL $url${R}"
    info "${DIM}would: verify it against $url.sha256${R}"
    BUILT_BIN="the downloaded $BIN_NAME-$target"
    return 0
  fi

  tmp="$(mktemp -d)"
  if ! curl -fsSL "$url" -o "$tmp/$BIN_NAME.tar.gz" 2>/dev/null; then
    rm -rf "$tmp"
    skip "no released build for $target — compiling instead"
    return 1
  fi

  sum_url="$url.sha256"
  if curl -fsSL "$sum_url" -o "$tmp/sha256" 2>/dev/null; then
    expected="$(tr -d '\r' <"$tmp/sha256" | awk '{print $1}')"
    actual="$(checksum "$tmp/$BIN_NAME.tar.gz")"
    if [ -z "$actual" ]; then
      warn "no sha256 tool here, so the download could not be verified"
    elif [ "$expected" != "$actual" ]; then
      scrub "$tmp" "checksum mismatch on $url (expected $expected, got $actual)"
    else
      ok "sha256 verified"
    fi
  fi

  tar -xzf "$tmp/$BIN_NAME.tar.gz" -C "$tmp" ||
    scrub "$tmp" "the downloaded archive could not be unpacked"
  [ -f "$tmp/$BIN_NAME" ] ||
    scrub "$tmp" "the downloaded archive does not contain $BIN_NAME"
  chmod +x "$tmp/$BIN_NAME"

  expected_version="$(source_version)"
  actual_version="$(binary_version "$tmp/$BIN_NAME")"
  if [ -z "$actual_version" ]; then
    scrub "$tmp" "the downloaded binary does not report a version"
  fi
  if [ "$actual_version" != "$expected_version" ]; then
    rm -rf "$tmp"
    skip "latest released build is $actual_version, but source is $expected_version — compiling instead"
    return 1
  fi

  DOWNLOAD_TMP="$tmp"
  BUILT_BIN="$tmp/$BIN_NAME"
  did "downloaded $BIN_NAME-$target"
  return 0
}

checksum() {
  if have sha256sum; then sha256sum "$1" | awk '{print $1}'
  elif have shasum;   then shasum -a 256 "$1" | awk '{print $1}'
  else printf ''
  fi
}

source_version() {
  sed -n 's/^version = "\([0-9][0-9.]*\)"$/\1/p' "$SRC/Cargo.toml" | head -n 1
}

binary_version() {
  "$1" --version 2>/dev/null | awk '{print $2}'
}

verify_binary_version() {
  local binary="$1" expected actual
  expected="$(source_version)"
  [ -n "$expected" ] || die "cannot read the workspace version from $SRC/Cargo.toml"
  actual="$(binary_version "$binary")"
  [ -n "$actual" ] || die "$binary does not report its version"
  [ "$actual" = "$expected" ] ||
    die "$binary is version $actual, but the source being installed is $expected"
}

build_binary() {
  local built="$(build_target_dir)/$BIN_NAME"

  if [ "$NO_BUILD" -eq 1 ]; then
    [ -x "$built" ] || die "--no-build was given but $built does not exist"
    verify_binary_version "$built"
    ok "using the existing $built"
    BUILT_BIN="$built"
    return
  fi

  if download_binary; then
    return
  fi

  if ! have cargo; then
    if [ "$(uname -s)" = "Darwin" ]; then
      die "desktop-driver is always compiled from the repository on macOS,
       and cargo is not installed. Install Rust and run this again:
         curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    fi
    die "nothing to install from: no released binary was available for this platform
       and cargo is not installed. Install Rust and run this again:
         curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  fi

  step "Building"
  if [ "$STATIC" -eq 1 ]; then
    info "${DIM}static musl build — runs on any Linux regardless of glibc${R}"
    act rustup target add x86_64-unknown-linux-musl
    act cargo build --release --manifest-path "$SRC/Cargo.toml" \
      --target x86_64-unknown-linux-musl
  else
    act cargo build --release --manifest-path "$SRC/Cargo.toml"
  fi
  did "built $built"
  BUILT_BIN="$built"
  if [ "$DRY_RUN" -eq 0 ]; then verify_binary_version "$BUILT_BIN"; fi
}

install_bin() {
  step "Installing the binary into $PREFIX"

  local dest="$PREFIX/$BIN_NAME" previous_version="" installed_version

  if [ -e "$dest" ] && ! is_ours "$dest" && [ "$FORCE" -eq 0 ]; then
    warn "$dest exists and was not written by this installer — skipping (use --force)"
    return
  fi

  INSTALLED_BIN="$dest"

  if [ "$DRY_RUN" -eq 1 ]; then
    info "${DIM}would: install $BUILT_BIN -> $dest${R}"
    info "${DIM}would: record it in $MANIFEST${R}"
    return
  fi

  if [ -x "$dest" ]; then previous_version="$(binary_version "$dest")"; fi

  mkdir -p "$PREFIX" "$MANIFEST_DIR"
  # Installed rather than copied in place: replacing a running binary in-place
  # would break whatever is executing it right now.
  local tmp
  tmp="$(mktemp "$PREFIX/.desktop.XXXXXX")"
  cp "$BUILT_BIN" "$tmp"
  chmod 755 "$tmp"
  mv -f "$tmp" "$dest"
  printf '%s\n' "$dest" >"$MANIFEST"
  installed_version="$(binary_version "$dest")"
  if [ -n "$previous_version" ] && [ "$previous_version" != "$installed_version" ]; then
    ok "$dest ${DIM}($previous_version → $installed_version)${R}"
  else
    ok "$dest ${DIM}(version $installed_version)${R}"
  fi

  case ":${PATH}:" in
    *":$PREFIX:"*) ;;
    *) PATH_HINT="$PREFIX" ;;
  esac
}

setup_macos_permissions() {
  local setup_json setup_compact

  if [ "$(uname -s)" != "Darwin" ] || [ "$NO_BIN" -eq 1 ]; then
    return 0
  fi

  if [ "$NO_SETUP" -eq 1 ]; then
    skip "macOS permission requests (--no-setup)"
    return 0
  fi
  if [ -z "$INSTALLED_BIN" ]; then
    warn "the binary was not installed, so macOS permissions were not requested"
    return 0
  fi

  step "Requesting macOS permissions"
  if [ "$DRY_RUN" -eq 1 ]; then
    info "${DIM}would: $INSTALLED_BIN setup${R}"
    info "${DIM}would: wait for approval, then verify the grants${R}"
    say ""
    return 0
  fi

  # A person running `curl | bash` still has a controlling terminal on
  # /dev/tty even though stdin is the script pipe. Automated jobs do not, and
  # must never hang waiting for a privacy dialog nobody can answer.
  if [ ! -t 1 ] || [ ! -r /dev/tty ]; then
    warn "non-interactive install — macOS permission prompts were skipped"
    info "Run:      $INSTALLED_BIN setup"
    say ""
    return 0
  fi

  info "macOS may ask for Accessibility, Screen Recording and Post Events."
  info "Approve each request in the visible system UI. The installer cannot approve them for you."
  if ! setup_json="$("$INSTALLED_BIN" --json setup)"; then
    warn "desktop could not open the macOS permission requests"
    info "Run later: $INSTALLED_BIN setup"
    say ""
    return 0
  fi

  setup_compact="$(printf '%s' "$setup_json" | tr -d '[:space:]')"
  case "$setup_compact" in
    *'"ready":true'*)
      ok "macOS permissions were already granted"
      say ""
      return 0
      ;;
  esac

  printf '  Press Return here after approving the macOS permissions... ' >/dev/tty
  IFS= read -r _ </dev/tty
  say ""
  info "Verifying the grants:"
  if ! "$INSTALLED_BIN" setup; then
    warn "desktop could not verify the macOS permissions"
    info "Run later: $INSTALLED_BIN setup"
  fi
  say ""
}

# A copied binary carries no marker of its own, so ownership is recorded beside
# it. Anything not in the manifest is somebody else's file and is left alone.
is_ours() {
  [ -f "$MANIFEST" ] && grep -qxF "$1" "$MANIFEST" 2>/dev/null
}

uninstall_bin() {
  step "Removing the binary from $PREFIX"
  local dest="$PREFIX/$BIN_NAME"

  if is_ours "$dest"; then
    act rm -f "$dest"; did "removed $dest"
    act rm -f "$MANIFEST"
  elif [ -e "$dest" ]; then
    warn "$dest was not written by this installer — left alone"
  else
    skip "$dest (not present)"
  fi
}

# ── the skill ────────────────────────────────────────────────────────────────

links_to() {
  [ -L "$1" ] && [ "$(readlink "$1")" = "$2" ]
}

is_our_file() {
  [ -f "$1" ] && grep -q "$MARKER" "$1" 2>/dev/null
}

is_our_dir() {
  [ -f "$1/$COPY_STAMP" ]
}

install_skill_dir() {
  local target="$1" src="$SRC/skills/$SKILL_NAME"

  if links_to "$target" "$src"; then
    ok "$target ${DIM}(already linked)${R}"
    return
  fi

  if [ -L "$target" ] || [ -e "$target" ]; then
    # A symlink under this exact name is ours to replace; a real directory only
    # if it carries our stamp, or the user insists.
    if [ -L "$target" ] || is_our_dir "$target" || [ "$FORCE" -eq 1 ]; then
      act rm -rf "$target"
    else
      warn "$target exists and was not created by this installer — skipping (use --force)"
      return
    fi
  fi

  act mkdir -p "$(dirname "$target")"
  if [ "$COPY" -eq 1 ]; then
    act cp -R "$src" "$target"
    if [ "$DRY_RUN" -eq 0 ]; then
      printf '%s\n' "written by $MARKER from $SRC" >"$target/$COPY_STAMP"
    fi
    did "$target ${DIM}(copied)${R}"
  else
    act ln -s "$src" "$target"
    did "$target ${DIM}→ $src${R}"
  fi
}

install_skill_file() {
  local target="$1" src="$SRC/skills/$SKILL_NAME/SKILL.md"

  if links_to "$target" "$src"; then
    ok "$target ${DIM}(already linked)${R}"
    return
  fi

  if [ -L "$target" ] || [ -e "$target" ]; then
    if [ -L "$target" ] || is_our_file "$target" || [ "$FORCE" -eq 1 ]; then
      act rm -f "$target"
    else
      warn "$target exists and was not created by this installer — skipping (use --force)"
      return
    fi
  fi

  act mkdir -p "$(dirname "$target")"
  if [ "$DRY_RUN" -eq 1 ]; then
    info "${DIM}would: flatten $SRC/skills/$SKILL_NAME into $target${R}"
  else
    {
      cat "$src"
      for reference in "$SRC/skills/$SKILL_NAME"/references/*.md; do
        [ -f "$reference" ] || continue
        printf '\n---\n\n# Bundled reference: %s\n\n' "$(basename "$reference")"
        cat "$reference"
      done
      # This inert marker lets --uninstall distinguish our generated file.
      printf '\n<!-- %s: from %s -->\n' "$MARKER" "$SRC"
    } >"$target"
  fi
  did "$target ${DIM}(flattened)${R}"
}

uninstall_skill() {
  local target="$1" kind="$2" dest

  if [ ! -L "$target" ] && [ ! -e "$target" ]; then
    skip "$target (not present)"
    return
  fi

  if [ -L "$target" ]; then
    dest="$(readlink "$target")"
    case "$dest" in
      */skills/"$SKILL_NAME"|*/skills/"$SKILL_NAME"/SKILL.md)
        act rm -f "$target"; did "removed $target"; return ;;
    esac
    if [ "$FORCE" -eq 1 ]; then
      act rm -f "$target"; did "removed $target ${DIM}(forced)${R}"
    else
      warn "$target points at $dest, not at a desktop-driver skill — left alone"
    fi
    return
  fi

  if [ "$kind" = "dir" ] && is_our_dir "$target"; then
    act rm -rf "$target"; did "removed $target"
  elif [ "$kind" = "file" ] && is_our_file "$target"; then
    act rm -f "$target"; did "removed $target"
  elif [ "$FORCE" -eq 1 ]; then
    act rm -rf "$target"; did "removed $target ${DIM}(forced)${R}"
  else
    warn "$target is not something this installer created — left alone (use --force)"
  fi
}

# ── choosing agents ──────────────────────────────────────────────────────────

SELECTED=""

select_agents() {
  local a

  if [ -n "$AGENTS_ARG" ]; then
    for a in $(printf '%s' "$AGENTS_ARG" | tr ',' ' '); do
      if ! known_agent "$a"; then
        die "unknown agent: $a (known: $(printf '%s' "$ALL_AGENTS" | tr ' ' ','))"
      fi
      SELECTED="$SELECTED $a"
    done
    return
  fi

  if [ "$INSTALL_ALL" -eq 1 ]; then
    SELECTED="$ALL_AGENTS"
    return
  fi

  for a in $ALL_AGENTS; do
    if agent_detected "$a"; then SELECTED="$SELECTED $a"; fi
  done

  # Nothing detected is not the same as nothing wanted: ~/.agents/skills is read
  # by several agents and costs nothing if none of them ever turn up.
  if [ -z "$(printf '%s' "$SELECTED" | tr -d '[:space:]')" ]; then
    SELECTED="agents"
    NO_AGENT_DETECTED=1
  fi
}

# ── main ─────────────────────────────────────────────────────────────────────

CARGO_MISSING=0
ATSPI_MISSING=0
PATH_HINT=""
NO_AGENT_DETECTED=0
SRC=""
SRC_MODE=""
BUILT_BIN=""
INSTALLED_BIN=""
DOWNLOAD_TMP=""

if [ -n "$PROJECT_DIR" ]; then
  [ -d "$PROJECT_DIR" ] || die "no such directory: $PROJECT_DIR"
  PROJECT_DIR="$(cd "$PROJECT_DIR" && pwd)"
fi

PREFIX="$(absolute "$PREFIX")"
SRC_DIR_ARG="$(absolute "$SRC_DIR_ARG")"

if [ "$STATIC" -eq 1 ] && [ "$(uname -s)" = "Darwin" ]; then
  die "--static builds a Linux musl binary and cannot be used on macOS"
fi

if [ "$UPDATE" -eq 1 ] && [ "$UNINSTALL" -eq 1 ]; then
  die "--update and --uninstall cannot be used together"
fi

bootstrap_update
resolve_source

if [ "$UNINSTALL" -eq 0 ] && { [ "$SRC_MODE" = "clone" ] || [ "$UPDATE" -eq 1 ]; }; then
  clone_or_update
fi

say ""
if [ "$UNINSTALL" -eq 1 ]; then
  step "Uninstalling desktop-driver"
else
  step "Installing desktop-driver"
  # A dry run elides the clone, so on a machine with no checkout yet there is
  # nothing here to recognise — which is the ordinary first-install case, and
  # printing the plan is the whole point of asking for one.
  if [ "$DRY_RUN" -eq 1 ] && [ "$SRC_MODE" = "clone" ] && [ ! -d "$SRC" ]; then
    info "${DIM}the plan below assumes the clone above succeeded${R}"
  elif ! is_checkout "$SRC"; then
    die "$SRC is not a desktop-driver checkout"
  fi
fi
info "source:  $SRC"
if [ -n "$PROJECT_DIR" ]; then info "project: $PROJECT_DIR"; fi
if [ "$DRY_RUN" -eq 1 ]; then info "${DIM}dry run — nothing will be written${R}"; fi
say ""

select_agents

if [ "$UNINSTALL" -eq 1 ]; then
  if [ "$NO_BIN" -eq 0 ]; then
    uninstall_bin
    say ""
  fi
  if [ "$NO_AGENTS" -eq 0 ]; then
    step "Removing skill links"
    for agent in $SELECTED; do
      target="$(agent_target "$agent")"
      if [ -n "$target" ]; then uninstall_skill "$target" "$(agent_kind "$agent")"; fi
    done
    say ""
  fi
  say "The checkout at $SRC was left in place; delete it yourself if you want it gone."
  say ""
  exit 0
fi

check_deps
say ""

if [ "$NO_BIN" -eq 0 ]; then
  build_binary
  say ""
  install_bin
  if [ -n "$DOWNLOAD_TMP" ]; then rm -rf "$DOWNLOAD_TMP"; DOWNLOAD_TMP=""; fi
  say ""
fi

if [ "$NO_AGENTS" -eq 0 ]; then
  step "Installing the skill"
  if [ "$NO_AGENT_DETECTED" -eq 1 ]; then
    info "${DIM}no agent detected — using the shared .agents location${R}"
  fi

  for agent in $SELECTED; do
    target="$(agent_target "$agent")"
    label="$(agent_label "$agent")"
    if [ -z "$target" ]; then
      warn "$label: no global skill location — use --project DIR to install it per project"
      continue
    fi
    info "${B}$label${R}"
    if [ "$(agent_kind "$agent")" = "dir" ]; then
      install_skill_dir "$target"
    else
      install_skill_file "$target"
      info "${DIM}  no on-demand skill loader here — this file is read on every request${R}"
    fi
  done
  say ""
fi

setup_macos_permissions

# ── what to do next ──────────────────────────────────────────────────────────

step "Done"
if [ -n "$PATH_HINT" ]; then
  warn "$PATH_HINT is not on your PATH. Add it:"
  case "$(basename "${SHELL:-sh}")" in
    fish) info "      fish_add_path $PATH_HINT" ;;
    zsh)  info "      echo 'export PATH=\"$PATH_HINT:\$PATH\"' >> ~/.zshrc && exec zsh" ;;
    *)    info "      echo 'export PATH=\"$PATH_HINT:\$PATH\"' >> ~/.bashrc && exec bash" ;;
  esac
fi
if [ "$CARGO_MISSING" -eq 1 ]; then warn "install Rust before building desktop"; fi
if [ "$ATSPI_MISSING" -eq 1 ]; then warn "install at-spi2-core before running desktop"; fi

say ""
info "Verify:   desktop doctor"
info "Try it:   desktop apps && desktop snapshot --app <something you have open>"
info "Browser:  desktop browser doctor && desktop browser open https://example.com --headless"
if [ "$(uname -s)" != "Darwin" ]; then
  info "Session:  desktop session start NAME --visible"
fi
if [ -d "$SRC/.git" ]; then
  info "Update:   $SRC/install.sh --update"
else
  # No .git to pull: this tree came down as a tarball, and re-running the
  # installer is what replaces it.
  info "Update:   $SRC/install.sh --update"
fi
info "Remove:   $SRC/install.sh --uninstall"
say ""
