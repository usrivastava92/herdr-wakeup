#!/usr/bin/env bash
# Shared helpers for the herdr-wakeup plugin. Sourced, not executed.

# Maps the current OS/arch to the plugin/vendor/<combo> directory naming used
# by both bin/vendor-wakeup and resolve_bins below. Keep in sync with the
# ASSET_MAP in bin/vendor-wakeup and the release.yml matrix in the wakeup repo.
_vendor_combo() {
  local os arch
  case "$(uname -s)" in
    Darwin) os="macos" ;;
    Linux) os="linux" ;;
    MINGW* | MSYS* | CYGWIN*) os="windows" ;;
    *) os="unknown" ;;
  esac
  case "$(uname -m)" in
    arm64 | aarch64) arch="arm64" ;;
    x86_64 | amd64) arch="x86_64" ;;
    *) arch="unknown" ;;
  esac
  printf '%s-%s' "$os" "$arch"
}

# Resolve the wakeup / wakeup-herdr binaries and export WAKEUP_BIN so the watcher
# can spawn the mechanism.
resolve_bins() {
  local here repo combo vendor_bin
  here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"   # .../plugin/bin
  repo="$(cd "$here/../.." && pwd)"                       # plugin repo root

  export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

  # `wakeup` is the separate, general-purpose CLI (its own repo). It does not
  # need to be separately installed by the end user: this repo vendors a
  # prebuilt binary per platform under plugin/vendor/<os>-<arch>/ (kept up to
  # date by bin/vendor-wakeup / the vendor-wakeup CI workflow), so a plain
  # `herdr plugin install` of this repo brings a working wakeup along with
  # no Rust toolchain required. A PATH-installed `wakeup` is still honored as
  # a fallback/override, e.g. for local development or an unvendored
  # platform combo.
  combo="$(_vendor_combo)"
  vendor_bin="$repo/plugin/vendor/$combo/wakeup"
  [ "$combo" = "windows-x86_64" ] && vendor_bin="$repo/plugin/vendor/$combo/wakeup.exe"

  WAKEUP=""
  if [ -f "$vendor_bin" ]; then
    chmod +x "$vendor_bin" 2>/dev/null || true
    WAKEUP="$vendor_bin"
  else
    WAKEUP="$(command -v wakeup 2>/dev/null || true)"
  fi

  # `wakeup-herdr` is an internal implementation detail of *this* plugin, not
  # a tool end users are meant to install or run directly. It is never
  # looked up on PATH: it always resolves to the build artifact inside this
  # plugin's own repo/install directory (produced by `bin/build` /
  # `make build`, which `herdr plugin install` runs automatically). This
  # keeps the user's PATH free of plugin-private tooling.
  WAKEUP_HERDR=""
  [ -x "$repo/target/release/wakeup-herdr" ] && WAKEUP_HERDR="$repo/target/release/wakeup-herdr"

  if [ -z "$WAKEUP" ]; then
    echo "herdr-wakeup: no wakeup binary for $combo in plugin/vendor/, and none found on PATH either." >&2
    echo "  Either wait for the next vendor-wakeup CI run, run 'plugin/bin/vendor-wakeup' yourself, or install wakeup on PATH manually." >&2
    return 1
  fi
  if [ -z "$WAKEUP_HERDR" ]; then
    echo "herdr-wakeup: watcher binary not built yet. Build it first: (cd $repo && make build)" >&2
    return 1
  fi
  export WAKEUP WAKEUP_HERDR
  export WAKEUP_BIN="$WAKEUP"
}

# Session-scoped state dir for the background watcher pidfile/log (Milestone
# 5, item 5). Resolved via `wakeup-herdr paths` - the single source of truth
# for the session-key hash - rather than re-deriving it in bash, so this can
# never drift from where the watcher itself writes config.json/state.json.
#
# Falls back to the pre-Milestone-5 shared (non-session-scoped) location if
# $WAKEUP_HERDR is unresolved or `paths` fails for any reason: `stop` in
# particular must always be able to find and kill a running watcher even in
# a broken environment, so this deliberately degrades instead of erroring.
_load_paths() {
  [ -n "${_HERDR_WAKEUP_STATE_DIR:-}" ] && return 0
  local out
  if [ -n "${WAKEUP_HERDR:-}" ] && out="$("$WAKEUP_HERDR" paths 2>/dev/null)"; then
    eval "$out"
    _HERDR_WAKEUP_STATE_DIR="${STATE_DIR:?}"
    _HERDR_WAKEUP_SESSION_KEY="${SESSION_KEY:-unknown}"
  else
    echo "herdr-wakeup: could not resolve session-scoped paths (wakeup-herdr binary unavailable?); falling back to shared state dir" >&2
    _HERDR_WAKEUP_STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-$HOME/.local/state/herdr-wakeup}"
    _HERDR_WAKEUP_SESSION_KEY="unknown"
  fi
}

state_dir() {
  _load_paths
  mkdir -p "$_HERDR_WAKEUP_STATE_DIR" 2>/dev/null || true
  printf '%s' "$_HERDR_WAKEUP_STATE_DIR"
}

session_key() {
  _load_paths
  printf '%s' "$_HERDR_WAKEUP_SESSION_KEY"
}

pidfile() { printf '%s/watcher.pid' "$(state_dir)"; }
logfile() { printf '%s/watcher.log' "$(state_dir)"; }

write_pidfile() {
  local pid="$1" bin="$2" f
  f="$(pidfile)"
  {
    printf 'pid=%s\n' "$pid"
    printf 'bin=%s\n' "$bin"
    printf 'session=%s\n' "$(session_key)"
    printf 'started_at=%s\n' "$(date +%s 2>/dev/null || printf unknown)"
  } > "$f"
}

watcher_pid() {
  local f key value p expected cmd
  f="$(pidfile)"
  [ -f "$f" ] || return 1

  if IFS='=' read -r key value < "$f" && [ "$key" = "pid" ]; then
    p="$value"
    expected="$(sed -n 's/^bin=//p' "$f" 2>/dev/null | head -n 1)"
  else
    # Backward compatibility for pidfiles written by older plugin versions.
    p="$(cat "$f" 2>/dev/null)"
    expected=""
  fi

  case "$p" in
    ''|*[!0-9]*) return 1 ;;
  esac
  kill -0 "$p" 2>/dev/null || return 1

  cmd="$(ps -p "$p" -o command= 2>/dev/null || true)"
  [ -n "$cmd" ] || return 1
  if [ -n "$expected" ]; then
    case "$cmd" in
      "$expected"*|*" $expected"*) printf '%s' "$p"; return 0 ;;
    esac
  fi
  case "$cmd" in
    *wakeup-herdr*) printf '%s' "$p"; return 0 ;;
  esac
  return 1
}
