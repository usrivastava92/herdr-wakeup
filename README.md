<div align="center">
  <img src="assets/banner.png" alt="herdr-wakeup keeps your machine awake while Herdr agents work and restores normal sleep when they finish" width="100%" />
  <h1>herdr-wakeup</h1>
  <p><strong>Keep macOS or Linux awake while Herdr-managed agents are working.</strong></p>
  <p>
    <a href="https://github.com/usrivastava92/herdr-wakeup/actions/workflows/ci.yml"><img src="https://github.com/usrivastava92/herdr-wakeup/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
    <a href="https://github.com/usrivastava92/herdr-wakeup/releases/latest"><img src="https://img.shields.io/github/v/release/usrivastava92/herdr-wakeup" alt="Release" /></a>
    <a href="LICENSE"><img src="https://img.shields.io/github/license/usrivastava92/herdr-wakeup" alt="License" /></a>
  </p>
</div>

---

`herdr-wakeup` watches the current Herdr session and holds a system wake assertion whenever an agent remains in the `working` state.
It releases the assertion after work stops, while allowing the display to sleep by default.

## Quick start

Install the pinned release:

```bash
herdr plugin install usrivastava92/herdr-wakeup/plugin --ref v0.1.0
```

Installation builds, registers, and enables the plugin, but it does not start the background watcher.
Start it explicitly:

```bash
herdr plugin action invoke start --plugin herdr-wakeup
```

Confirm that it is running:

```bash
herdr plugin action invoke status --plugin herdr-wakeup
```

## Requirements

- Herdr 0.7.0 or later.
- macOS or Linux on ARM64 or x86-64.
- A Rust toolchain to build the watcher during installation.

The platform-specific [`wakeup`](https://github.com/usrivastava92/wakeup) binary is bundled with the plugin.
You do not need to install it separately.

## Plugin lifecycle

The plugin has several distinct states:

| State | Meaning |
| --- | --- |
| Installed | Herdr has registered the plugin. |
| Enabled | Herdr exposes the plugin actions, but the watcher is not necessarily running. |
| Running | A background watcher is active for the current Herdr session. |
| Armed | The watcher is allowed to acquire a wake assertion. |
| Keeping awake | A matching agent has worked past the start grace period and the assertion is held. |

### 1. Install

```bash
herdr plugin install usrivastava92/herdr-wakeup/plugin --ref v0.1.0
```

Herdr downloads the pinned source, runs the plugin build hook, registers the plugin, and enables its actions.
Install does not launch a persistent process.

### 2. Start

```bash
herdr plugin action invoke start --plugin herdr-wakeup
```

`start` launches one detached watcher for the current Herdr socket and session.
The command is idempotent, so invoking it again does not create a duplicate watcher.

The watcher does not start automatically after installation, login, machine restart, or an unexpected watcher exit.
Invoke `start` again whenever a watcher is needed.

### 3. Monitor agents

The watcher responds to Herdr events and evaluates agents whose status is `working` by default.
Its default behavior is:

1. Wait for an agent to remain working for 5 seconds.
2. Acquire a system wake assertion while allowing display sleep.
3. Keep the assertion for 30 seconds after the last matching agent stops working.
4. Release the assertion when the stop grace period expires.

The grace periods prevent brief status changes from repeatedly acquiring and releasing the assertion.

### 4. Arm or disarm

Disarm wake decisions without stopping the watcher:

```bash
herdr plugin action invoke disarm --plugin herdr-wakeup
```

Disarming releases any held assertion on the next evaluation, within 60 seconds by default or sooner when a Herdr event arrives.
The watcher continues observing Herdr while disarmed.

Resume wake decisions:

```bash
herdr plugin action invoke arm --plugin herdr-wakeup
```

The armed setting is persisted and can be changed whether or not the watcher is running.
A running watcher picks up the change on its next evaluation.

### 5. Inspect status and diagnostics

```bash
herdr plugin action invoke status --plugin herdr-wakeup
herdr plugin action invoke doctor --plugin herdr-wakeup
```

`status` reports whether the watcher is running, its last persisted state, and a fresh check of matching Herdr agents.
`doctor` reports binary resolution, socket connectivity, configuration and state paths, and process diagnostics.

### 6. Stop

```bash
herdr plugin action invoke stop --plugin herdr-wakeup
```

Stopping terminates the watcher for the current Herdr session and releases its active wake assertion.
Before disabling, upgrading, or uninstalling, run `stop` in every Herdr session where the watcher was started because each session has an independent detached watcher.

### Enable or disable

```bash
herdr plugin enable herdr-wakeup
herdr plugin disable herdr-wakeup
```

Enabling makes the actions available but does not start the watcher.
Before disabling, run `stop` in every Herdr session where the watcher was started.

### Upgrade

Replace `vX.Y.Z` with the release you want to install:

First, run the following command in every Herdr session where the watcher was started:

```bash
herdr plugin action invoke stop --plugin herdr-wakeup
```

Then install the new version and restart the watcher in each session where it is needed:

```bash
herdr plugin install usrivastava92/herdr-wakeup/plugin --ref vX.Y.Z
herdr plugin action invoke doctor --plugin herdr-wakeup
herdr plugin action invoke start --plugin herdr-wakeup
```

### Uninstall

First, run `stop` in every Herdr session where the watcher was started.
Then uninstall the plugin:

```bash
herdr plugin uninstall herdr-wakeup
```

Stopping every session first ensures that no detached watcher or wake assertion remains active.
To locate session-specific configuration, state, and logs before uninstalling, run `doctor` and remove those files manually if desired.

## Configuration

Configuration is created automatically the first time `start`, `status`, `doctor`, `arm`, or `disarm` reads or changes it.
Run the following command to display its exact path along with the corresponding state and log paths:

```bash
herdr plugin action invoke doctor --plugin herdr-wakeup
```

The default configuration is:

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

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `armed` | boolean | `true` | Allow the watcher to acquire and release wake assertions. |
| `display` | boolean | `false` | Keep the display awake in addition to the system. |
| `start_grace_seconds` | integer | `5` | Require continuous matching activity for this long before acquiring an assertion. |
| `stop_grace_seconds` | integer | `30` | Keep the assertion for this long after matching activity ends. |
| `statuses` | string array | `["working"]` | Agent statuses that count as active work. |
| `notify` | boolean | `true` | Show notifications when the assertion is acquired or released. |
| `allow_cli_fallback` | boolean | `false` | Use `herdr agent list` when the Herdr socket is unreachable. |

Only `armed` is hot-reloaded.
After changing any other setting, restart the watcher:

```bash
herdr plugin action invoke stop --plugin herdr-wakeup
herdr plugin action invoke start --plugin herdr-wakeup
```

## Safety and failure behavior

- A second `start` is a no-op when the watcher is already running.
- `stop` validates the recorded process before terminating it, so it does not kill an unrelated reused process ID.
- Temporary Herdr snapshot errors preserve the current assertion rather than guessing that work has stopped.
- Herdr unavailability lasting 120 seconds by default causes the watcher to exit and release its assertion.
- If the child process holding the assertion exits unexpectedly, the watcher recreates it on the next evaluation.
- Configuration and runtime state are written atomically.

## Troubleshooting

Check the watcher and current wake decision:

```bash
herdr plugin action invoke status --plugin herdr-wakeup
```

Inspect socket connectivity, resolved binaries, paths, and process state:

```bash
herdr plugin action invoke doctor --plugin herdr-wakeup
```

Read recent action output:

```bash
herdr plugin log list --plugin herdr-wakeup --limit 10
```

If the watcher is not running, invoke `start` and check the action logs again if startup fails.

## Development

Build and link the local checkout into Herdr:

```bash
make plugin-link
```

Run the project checks:

```bash
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
plugin/bin/vendor-wakeup --verify
```

The watcher binary is an implementation detail of the plugin and is not installed on `PATH`.
For local debugging only, run it from the repository:

```bash
./target/release/wakeup-herdr --once
./target/release/wakeup-herdr -v
```

[`plugin/vendor/provenance.json`](plugin/vendor/provenance.json) records the source release and SHA-256 digest for each bundled `wakeup` artifact.

## License

Licensed under the [MIT License](LICENSE).
