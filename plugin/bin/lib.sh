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
lockdir() { printf '%s/watcher.start.lock' "$(state_dir)"; }
reclaimdir() { printf '%s/watcher.start.reclaim.lock' "$(state_dir)"; }
lock_grace_seconds() { printf '%s' "${HERDR_WAKEUP_START_LOCK_GRACE_SECONDS:-2}"; }

_lock_dir_owned_by_self() {
  local d pidfile pid
  d="$1"
  pidfile="$d/pid"
  [ -d "$d" ] || return 1
  [ -f "$pidfile" ] || return 1
  IFS= read -r pid < "$pidfile" 2>/dev/null || return 1
  [ "$pid" = "$$" ]
}

_cleanup_lockdir_if_owned() {
  local d
  d="$1"
  _lock_dir_owned_by_self "$d" && rm -rf "$d" 2>/dev/null || true
}

release_start_locks() {
  _cleanup_lockdir_if_owned "$(lockdir)"
  _cleanup_lockdir_if_owned "$(reclaimdir)"
}

_lock_mtime() {
  case "$(uname -s)" in
    Darwin) stat -f %m "$1" 2>/dev/null || return 1 ;;
    Linux) stat -c %Y "$1" 2>/dev/null || return 1 ;;
    *) return 1 ;;
  esac
}

_lock_age_at_least_grace() {
  local d now mtime grace age
  d="$(lockdir)"
  grace="$(lock_grace_seconds)"
  mtime="$(_lock_mtime "$d" 2>/dev/null || true)"
  now="$(date +%s 2>/dev/null || printf 0)"
  case "$mtime" in ''|*[!0-9]*) return 1 ;; esac
  case "$now" in ''|*[!0-9]*) return 1 ;; esac
  case "$grace" in ''|*[!0-9]*) return 1 ;; esac
  age=$((now - mtime))
  [ "$age" -ge "$grace" ]
}

_mutex_is_stale() {
  local d pidfile pid mtime now grace age
  d="$1"
  pidfile="$d/pid"
  grace="$(lock_grace_seconds)"
  [ -d "$d" ] || return 1
  mtime="$(_lock_mtime "$d" 2>/dev/null || true)"
  now="$(date +%s 2>/dev/null || printf 0)"
  case "$mtime" in ''|*[!0-9]*) return 1 ;; esac
  case "$now" in ''|*[!0-9]*) return 1 ;; esac
  case "$grace" in ''|*[!0-9]*) return 1 ;; esac
  if [ ! -f "$pidfile" ]; then
    age=$((now - mtime))
    [ "$age" -ge "$grace" ]
    return $?
  fi
  IFS= read -r pid < "$pidfile" 2>/dev/null || {
    age=$((now - mtime))
    [ "$age" -ge "$grace" ]
    return $?
  }
  case "$pid" in
    ''|*[!0-9]*) age=$((now - mtime)); [ "$age" -ge "$grace" ]; return $? ;;
  esac
  kill -0 "$pid" 2>/dev/null || return 0
  return 1
}

acquire_reclaim_mutex() {
  local d
  d="$(reclaimdir)"
  while :; do
    if mkdir "$d" 2>/dev/null; then
      printf '%s\n' "$$" > "$d/pid"
      trap 'release_start_locks' EXIT INT TERM HUP
      return 0
    fi
    if _mutex_is_stale "$d"; then
      rm -rf "$d" 2>/dev/null || true
      continue
    fi
    sleep 0.05
  done
}

lock_is_stale() {
  local d f pid
  d="$(lockdir)"
  f="$(lockdir)/pid"
  [ -d "$d" ] || return 1
  if [ ! -f "$f" ]; then
    _lock_age_at_least_grace
    return $?
  fi
  IFS= read -r pid < "$f" 2>/dev/null || {
    _lock_age_at_least_grace
    return $?
  }
  case "$pid" in
    ''|*[!0-9]*) _lock_age_at_least_grace; return $? ;;
  esac
  kill -0 "$pid" 2>/dev/null || return 0
  return 1
}

acquire_start_lock() {
  local mode="${1:-wait}" d
  d="$(lockdir)"
  while :; do
    if mkdir "$d" 2>/dev/null; then
      printf '%s\n' "$$" > "$d/pid"
      trap 'release_start_locks' EXIT INT TERM HUP
      return 0
    fi
    if lock_is_stale; then
      acquire_reclaim_mutex
      if lock_is_stale; then
        rm -rf "$d" 2>/dev/null || true
      fi
      _cleanup_lockdir_if_owned "$(reclaimdir)"
      continue
    fi
    [ "$mode" = try ] && return 1
    sleep 0.05
  done
}

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
