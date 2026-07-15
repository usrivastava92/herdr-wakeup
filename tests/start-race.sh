#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

mkdir -p "$tmp_dir/plugin" "$tmp_dir/target/release" "$tmp_dir/bin"
cp -R "$repo_root/plugin/bin" "$tmp_dir/plugin/"

cat > "$tmp_dir/bin/wakeup" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$tmp_dir/bin/wakeup"

cat > "$tmp_dir/target/release/wakeup-herdr" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  paths)
    printf "CONFIG_DIR='%s'\n" "$TMPDIR/herdr-config"
    printf "STATE_DIR='%s'\n" "$TMPDIR/herdr-state"
    printf "SESSION_KEY='%s'\n" "test-session"
    exit 0
    ;;
  autostart-decision)
    exit 0
    ;;
esac
started_file="${STARTED_FILE:?}"
printf '%s\n' "$$" >> "$started_file"
trap 'exit 0' TERM HUP INT
while :; do sleep 1; done
EOF
chmod +x "$tmp_dir/target/release/wakeup-herdr"

export PATH="$tmp_dir/bin:$PATH"
export TMPDIR="$tmp_dir"
export STARTED_FILE="$tmp_dir/started.txt"
export HERDR_WAKEUP_START_LOCK_GRACE_SECONDS=1
mkdir -p "$TMPDIR/herdr-state/watcher.start.lock"
printf 'pid=%s\n' 999999 > "$TMPDIR/herdr-state/watcher.start.lock/pid"
if [ "$(uname -s)" = Darwin ]; then
  touch -t 200001010000 "$TMPDIR/herdr-state/watcher.start.lock"
else
  touch -d '2000-01-01 00:00:00' "$TMPDIR/herdr-state/watcher.start.lock"
fi
mkdir -p "$TMPDIR/herdr-state/watcher.start.reclaim.lock"
printf 'pid=%s\n' 999999 > "$TMPDIR/herdr-state/watcher.start.reclaim.lock/pid"
if [ "$(uname -s)" = Darwin ]; then
  touch -t 200001010000 "$TMPDIR/herdr-state/watcher.start.reclaim.lock"
else
  touch -d '2000-01-01 00:00:00' "$TMPDIR/herdr-state/watcher.start.reclaim.lock"
fi
: > "$STARTED_FILE"

start_script="$tmp_dir/plugin/bin/start"
stop_script="$tmp_dir/plugin/bin/stop"

for _ in 1 2 3 4 5 6 7 8; do
  "$start_script" --auto >/dev/null 2>&1 &
done
wait

pidfile="$TMPDIR/herdr-state/watcher.pid"

if [ ! -f "$pidfile" ]; then
  echo "missing pidfile" >&2
  exit 1
fi

started_count="$(wc -l < "$STARTED_FILE" | tr -d ' ')"
if [ "$started_count" != 1 ]; then
  echo "expected exactly one watcher start, got $started_count" >&2
  exit 1
fi

pid="$(sed -n 's/^pid=//p' "$pidfile" | head -n 1)"
if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
  echo "watcher is not controllable" >&2
  exit 1
fi

# A delayed contender after the winner has already started must still stay quiet.
before="$(wc -l < "$STARTED_FILE" | tr -d ' ')"
if ! "$start_script" --auto >/dev/null 2>&1; then
  echo "delayed autostart failed" >&2
  exit 1
fi
after="$(wc -l < "$STARTED_FILE" | tr -d ' ')"
if [ "$before" != "$after" ]; then
  echo "delayed contender spawned a duplicate watcher" >&2
  exit 1
fi

if [ ! -f "$pidfile" ]; then
  echo "winner pidfile vanished unexpectedly" >&2
  exit 1
fi

"$stop_script" >/dev/null
sleep 1.5
if kill -0 "$pid" 2>/dev/null; then
  echo "watcher still alive after stop" >&2
  exit 1
fi

echo "ok"
