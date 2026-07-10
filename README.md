# herdr-wakeup

A Herdr plugin that keeps macOS awake while Herdr-managed agents are working.

This repo contains the Herdr-specific watcher and plugin wrapper.
The standalone power assertion utility lives in the separate `wakeup` repo and must be installed separately.

`wakeup-herdr` (this repo's watcher binary) is an **internal implementation detail of the plugin**, not a user-facing CLI.
It is never installed to `PATH`: it is built in place inside this repo (`target/release/wakeup-herdr`) and the plugin's own action scripts find it there.
The supported, user-facing interface is entirely `herdr plugin action invoke ...` (see below); running `wakeup-herdr` directly is only useful for local development/debugging of this plugin itself.

## What it does

`herdr-wakeup` watches Herdr agent state.
When at least one agent is `working`, it runs `wakeup` to hold a macOS power assertion.
When agents stop working, it releases the assertion after a short grace period.

The watcher is event-driven.
It subscribes to Herdr socket events, re-checks agent state on changes via the socket `agent.list` RPC, and uses a slow backstop tick only for recovery.
No `herdr` process is spawned during normal operation; a CLI shellout only happens for `--once`, or if `--allow-cli-fallback` is set and the socket itself is unreachable.

## Requirements

- macOS.
- Herdr `>= 0.7.0`.
- Rust toolchain, to build this repo's own small watcher binary (`wakeup-herdr`) from source.
- **No separate `wakeup` install and no cargo build for it**: a prebuilt `wakeup` binary for your platform is vendored inside this repo (see below). It is only necessary to have `wakeup` on `PATH` yourself if your platform/arch isn't one of the vendored combos yet.

## Vendored `wakeup` binary

`plugin/vendor/<os>-<arch>/wakeup` in this repo holds a prebuilt copy of the standalone `wakeup` CLI (separate repo) for each supported platform, refreshed automatically by [`.github/workflows/vendor-wakeup.yml`](.github/workflows/vendor-wakeup.yml) whenever `wakeup` cuts a new release, and committed straight into git (these binaries are tiny - `wakeup` has zero external dependencies).
`resolve_bins` in `plugin/bin/lib.sh` picks the file matching `uname -s`/`uname -m` automatically and makes it executable; it only falls back to a `wakeup` on `PATH` if no vendored binary matches your platform/arch.
This means cloning or `herdr plugin install`-ing this repo is enough - there's no separate `wakeup` install step, and no cargo/Rust needed for `wakeup` itself (only for this repo's own `wakeup-herdr` watcher, which is a tiny, dependency-free crate).

Currently vendored: `macos-arm64`, `linux-x86_64`, `linux-arm64`, `windows-x86_64` (Linux binaries are static `musl` builds, so they run on any distro regardless of glibc version).
To refresh manually: `WAKEUP_REPO=<owner>/wakeup plugin/bin/vendor-wakeup [tag]` (defaults to the latest release).

## Build and link locally

```bash
make plugin-link   # builds target/release/wakeup-herdr, then `herdr plugin link`s this repo
```

(`herdr plugin install <repo-url>`, the non-local/from-GitHub path, builds the same binary automatically via the plugin's own `bin/build` hook - no separate install step needed either way.)

Then use the plugin actions - this is the supported, user-facing interface:

```bash
herdr plugin action invoke start   --plugin herdr-wakeup
herdr plugin action invoke status  --plugin herdr-wakeup
herdr plugin action invoke arm     --plugin herdr-wakeup   # resume wake/sleep decisions
herdr plugin action invoke disarm  --plugin herdr-wakeup   # pause without stopping the watcher
herdr plugin action invoke doctor  --plugin herdr-wakeup   # diagnostics: binaries, socket, config/state, pidfile
herdr plugin action invoke stop    --plugin herdr-wakeup
```

## Running the watcher directly (development only)

`wakeup-herdr` is not installed on `PATH`; run it via its build path from this repo when developing/debugging the plugin itself:

```bash
./target/release/wakeup-herdr
./target/release/wakeup-herdr -d
./target/release/wakeup-herdr --once
./target/release/wakeup-herdr -v
```

## Options

| Flag | Default | Meaning |
| --- | --- | --- |
| `-d`, `--display` | off | Also keep the display awake. |
| `--start-grace <secs>` | `5` | Require this much sustained working time before acquiring the assertion. |
| `--grace <secs>` | `30` | Stay awake this long after the last working agent (stop grace). |
| `--backstop <secs>` | `60` | Safety re-evaluation interval. |
| `--exit-after <secs>` | `120` | Exit if the Herdr server stays unreachable this long. |
| `--debounce <ms>` | `400` | Coalesce bursts of Herdr events. |
| `--statuses <list>` | `working` | Comma-separated statuses that count as active. |
| `--socket <path>` | `$HERDR_SOCKET_PATH` | Herdr socket path. |
| `--wakeup <path>` | `wakeup` | Path to the standalone `wakeup` binary. |
| `--herdr <path>` | `herdr` | Path to the Herdr binary (used only for `--once` and CLI fallback). |
| `--allow-cli-fallback` | off | Shell out to `herdr agent list` if the socket itself is unreachable. |
| `--no-notify` | off | Do not post wake/sleep toast notifications. |

## State machine

The watcher's wake/sleep decision is a small, pure, unit-tested state machine (`crates/wakeup-herdr/src/state.rs`):

```text
Off          -- working --> PendingWake
PendingWake -- idle    --> Off
PendingWake -- sustained working past start_grace --> Awake   (acquires the assertion)
Awake        -- idle    --> PendingSleep
PendingSleep -- working --> Awake
PendingSleep -- sustained idle past stop_grace --> Off        (releases the assertion)
Error        -- recovered --> Off, PendingWake, or Awake
```

`start_grace` and `grace` (stop grace) exist specifically to absorb brief status flicker: a one-second blip of `working` does not wake the machine, and a one-second blip of idle does not put it back to sleep.
While a snapshot fetch fails (`Error`), the watcher holds whatever it was already holding rather than guessing; it only changes wake/sleep state again once a snapshot succeeds.

## Config and state files

`wakeup-herdr` persists two small JSON files, deliberately kept separate: a rarely-changing, user-tunable **config** and a frequently-changing, watcher-owned **runtime state**.

| File | Location | Purpose |
| --- | --- | --- |
| `config.json` | `$HERDR_PLUGIN_CONFIG_DIR/sessions/<key>` (or `~/.config/herdr-wakeup/sessions/<key>` standalone) | `armed`, `display`, `start_grace_seconds`, `stop_grace_seconds`, `statuses`, `notify`, binary paths, `allow_cli_fallback`. Seeds defaults *underneath* CLI flags; CLI flags always win. |
| `state.json` | `$HERDR_PLUGIN_STATE_DIR/sessions/<key>` (or `~/.local/state/herdr-wakeup/sessions/<key>` standalone) | Current state, `armed`, whether the assertion is held, working agents, last transition, `checked_at_unix` (for staleness detection), last error. Written after every evaluation; never read back by the watcher itself. |

Both are written atomically (temp file + rename), and a missing or corrupt file always falls back safely to defaults rather than crashing or blocking startup - a corrupt config is logged once and defaults to armed.

**`config.json` is bootstrapped automatically** (Herdr itself has no standard plugin-config-init mechanism - `herdr plugin config-dir <id>` only prints a path, it does not create anything - so this plugin does it itself): the first `start`, or the first `doctor`, writes the full set of defaults below to disk if the file doesn't already exist yet, so it's always present and hand-editable after that, not only as a side effect of `arm`/`disarm`. A file that already exists - valid or corrupt - is never touched or "repaired" automatically, so an in-progress edit is never clobbered.

`<key>` is a short hash derived from the resolved Herdr socket path (Herdr does not expose a session name to plugin action scripts), so concurrent Herdr sessions never share one watcher's config/state/pidfile/log. Run `herdr plugin action invoke doctor --plugin herdr-wakeup` (or `./target/release/wakeup-herdr paths` locally) to see the resolved directories and key for the current socket; `herdr plugin config-dir herdr-wakeup` shows Herdr's own (non-session-scoped) root above it.

### Configuration reference

Every field in `config.json`, with its default and what it does. All of these mirror an equivalent CLI flag (see [Options](#options)); the config file only sets the *starting* value, and any CLI flag passed to `wakeup-herdr`/`./target/release/wakeup-herdr` always wins over it.

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `armed` | bool | `true` | Whether the watcher makes wake/sleep decisions at all. Hot-reloaded every evaluation - the only field that takes effect on a *running* watcher without a restart (see `arm`/`disarm` above). |
| `display` | bool | `false` | Also keep the display awake (runs `wakeup -di` instead of `wakeup -i`). Read once at startup only - changing it requires `stop` + `start`. |
| `start_grace_seconds` | integer | `5` | How long an agent must stay `working` before the assertion is acquired; absorbs brief status flicker. |
| `stop_grace_seconds` | integer | `30` | How long to keep the assertion after the last agent stops working, before releasing. |
| `statuses` | array of strings | `["working"]` | Which Herdr agent statuses count as "active" for wake purposes. |
| `notify` | bool | `true` | Post a toast notification on wake/sleep transitions. |
| `allow_cli_fallback` | bool | `false` | If the Herdr socket itself is unreachable, shell out to `herdr agent list` instead of just reporting unavailable. |
| `wakeup_bin` | string, **optional, absent by default** | auto (vendored binary, else `PATH`) | Force a specific `wakeup` binary path/name, bypassing the vendored-binary resolution entirely. This key is *not* written into a freshly bootstrapped config.json - it only appears if you add it yourself. |
| `herdr_bin` | string, **optional, absent by default** | resolved on `PATH` | Same idea as `wakeup_bin`, for the `herdr` binary (used only for `--once` and CLI fallback; not vendored). |

A freshly bootstrapped config.json therefore looks like this - 7 fields, no `wakeup_bin`/`herdr_bin` clutter:

```json
{
  "allow_cli_fallback": false,
  "armed": true,
  "display": false,
  "notify": true,
  "start_grace_seconds": 5,
  "statuses": ["working"],
  "stop_grace_seconds": 30
}
```

`wakeup-herdr doctor` (and `herdr plugin action invoke doctor --plugin herdr-wakeup`) prints every field above, including `wakeup_bin`/`herdr_bin`'s *effective* value and exactly where it came from - a config override, the `$WAKEUP_BIN`/`$HERDR_BIN_PATH` env var (what the plugin scripts set after resolving a vendored binary), or plain `PATH` auto-detection - so you never have to open the file or guess just to see what's actually in effect.

`armed` is re-read from config on every evaluation, not just at startup. The supported way to flip it is the plugin actions:

```bash
herdr plugin action invoke disarm --plugin herdr-wakeup   # pause: release any held assertion immediately, keep observing
herdr plugin action invoke arm    --plugin herdr-wakeup   # resume normal wake/sleep decisions
herdr plugin action invoke status --plugin herdr-wakeup   # print the last-persisted runtime state
herdr plugin action invoke doctor --plugin herdr-wakeup   # one-shot diagnostic dump: socket reachability, config/state validity, pidfile identity
```

(The same subcommands - `arm`/`disarm`/`state`/`doctor`/`paths` - also exist directly on `./target/release/wakeup-herdr` for local development; the plugin actions just wrap them with pidfile/log context.)

`disarm`/`arm` just flip and atomically save `config.json` - they work whether or not the watcher is running (so `disarm` survives a restart), and an already-running watcher picks up the change on its very next evaluation without a restart or signal.

## Lifecycle hardening

- **Pidfile identity validation**: the pidfile records `pid`, `bin`, `session`, and `started_at`. Before trusting it, the plugin scripts confirm the PID is alive *and* its running command matches the recorded binary - so a stale pidfile left behind after a crash, or a PID later reused by an unrelated process, is never mistaken for a live watcher. `start` is idempotent (a second `start` while one is already running is a no-op) and `stop` never kills an unrelated process.
- **Dead-child recovery**: if the spawned `wakeup` child exits unexpectedly (e.g. killed out-of-band) while the state machine still believes it should be holding the assertion, the watcher notices on its next evaluation and respawns it automatically.
- **`doctor`**: a single command/action that reports binary resolution, socket reachability, config/state file validity, the session key, and whether the pidfile's recorded PID is actually alive - meant to be the first thing to run when something seems wrong.

## Improvement plan

This repo is developed against an internal, untracked `PLUGIN_IMPROVEMENT_PLAN.md` milestone plan (not published in this repo).
It captures what to borrow from the Amphetamine Herdr plugin reference and what to avoid, plus a state machine, config/state files, and lifecycle-hardening roadmap.
