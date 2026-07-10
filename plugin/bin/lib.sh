#!/usr/bin/env bash
# Shared helpers for the herdr-wakeup plugin. Sourced, not executed.

# Resolve the wakeup / wakeup-herdr binaries and export WAKEUP_BIN so the watcher
# can spawn the mechanism. The standalone wakeup binary is expected on PATH.
# The watcher can be installed or loaded from this repo's release build.
resolve_bins() {
  local here repo
  here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"   # .../plugin/bin
  repo="$(cd "$here/../.." && pwd)"                       # plugin repo root

  export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

  WAKEUP="$(command -v wakeup 2>/dev/null || true)"

  WAKEUP_HERDR="$(command -v wakeup-herdr 2>/dev/null || true)"
  [ -z "$WAKEUP_HERDR" ] && [ -x "$repo/target/release/wakeup-herdr" ] && WAKEUP_HERDR="$repo/target/release/wakeup-herdr"

  if [ -z "$WAKEUP" ]; then
    echo "herdr-wakeup: standalone wakeup binary not found on PATH. Install wakeup first." >&2
    return 1
  fi
  if [ -z "$WAKEUP_HERDR" ]; then
    echo "herdr-wakeup: watcher binary not found. Build/install it first: (cd $repo && make install)" >&2
    return 1
  fi
  export WAKEUP WAKEUP_HERDR
  export WAKEUP_BIN="$WAKEUP"
}

# State dir for the background watcher pidfile/log.
state_dir() {
  local d="${HERDR_PLUGIN_STATE_DIR:-$HOME/.local/state/herdr-wakeup}"
  mkdir -p "$d" 2>/dev/null || true
  printf '%s' "$d"
}

pidfile() { printf '%s/watcher.pid' "$(state_dir)"; }
logfile() { printf '%s/watcher.log' "$(state_dir)"; }

write_pidfile() {
  local pid="$1" bin="$2" f
  f="$(pidfile)"
  {
    printf 'pid=%s\n' "$pid"
    printf 'bin=%s\n' "$bin"
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
