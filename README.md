# herdr-wakeup

A Herdr plugin that keeps macOS awake while Herdr-managed agents are working.

This repo contains the Herdr-specific watcher and plugin wrapper.
The standalone power assertion utility lives in the separate `wakeup` repo and must be installed separately.

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
- `wakeup` installed on `PATH`.
- Rust toolchain for source installs.

## Build and install locally

```bash
make install
make plugin-link
```

Then use the plugin actions:

```bash
herdr plugin action invoke start  --plugin herdr-wakeup
herdr plugin action invoke status --plugin herdr-wakeup
herdr plugin action invoke stop   --plugin herdr-wakeup
```

## Running the watcher directly

```bash
wakeup-herdr
wakeup-herdr -d
wakeup-herdr --once
wakeup-herdr -v
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

## Improvement plan

This repo is developed against an internal, untracked `PLUGIN_IMPROVEMENT_PLAN.md` milestone plan (not published in this repo).
It captures what to borrow from the Amphetamine Herdr plugin reference and what to avoid, plus a state machine, config/state files, and lifecycle-hardening roadmap.
