//! Durable config and runtime state files (Milestone 4).
//!
//! Config (user-tunable, rarely changes; edited via `arm`/`disarm`) and state
//! (watcher-owned, written on every evaluation) are deliberately separate
//! files, so toggling `armed` never races with the watcher's own frequent
//! state writes and vice versa. Both use a shared JSON format for consistency
//! with the rest of this crate (it already depends on serde_json for the
//! Herdr socket protocol) rather than pulling in a separate TOML dependency.
//!
//! Both directories resolve through the `HERDR_PLUGIN_CONFIG_DIR` /
//! `HERDR_PLUGIN_STATE_DIR` environment variables Herdr injects into plugin
//! action scripts (confirmed against a real Herdr binary), falling back to a
//! standalone-friendly default consistent with `plugin/bin/lib.sh`'s existing
//! state-dir convention for when the watcher is run outside a plugin action
//! (e.g. directly from a terminal for development).

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// The Herdr socket path, resolved the same way `Opts`'s own default is
/// (`HERDR_SOCKET_PATH` env var, else the standard default) - deliberately
/// *not* including any `--socket` CLI override, so config/state loading
/// (which happens before CLI args are parsed) and the running watcher always
/// agree on the same session-scoped directory. Used only to derive
/// `session_key`, never to actually connect.
pub fn socket_hint() -> String {
    std::env::var("HERDR_SOCKET_PATH").unwrap_or_else(|_| {
        home_dir()
            .join(".config/herdr/herdr.sock")
            .display()
            .to_string()
    })
}

/// Derive a short, filesystem-safe key that uniquely identifies which Herdr
/// server/session this watcher is bound to (Milestone 5, item 5: concurrent
/// Herdr sessions must not share one config/state/pidfile/log set).
///
/// Herdr does not expose a session name to plugin action scripts (verified
/// empirically - only `HERDR_SOCKET_PATH` and similar are injected), so the
/// key is derived from the resolved socket path itself via FNV-1a: cheap,
/// dependency-free, and more than collision-resistant enough for a handful
/// of concurrent local sessions (this is not adversarial input).
pub fn session_key(socket_path: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in socket_path.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config/herdr-wakeup"));
    base.join("sessions").join(session_key(&socket_hint()))
}

pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/state/herdr-wakeup"));
    base.join("sessions").join(session_key(&socket_hint()))
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn state_path() -> PathBuf {
    state_dir().join("state.json")
}

/// Write `contents` to `path` atomically: write to a sibling temp file, then
/// rename over the target. Rename is atomic on the same filesystem, so a
/// reader never observes a partially-written file.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("wakeup-herdr.tmp");
    let tmp_name = format!(".{file_name}.tmp-{}", std::process::id());
    let tmp_path = path.with_file_name(tmp_name);
    fs::write(&tmp_path, contents)?;
    fs::rename(&tmp_path, path)
}

// --------------------------------------------------------------------------- //
// Config: user-tunable, persisted separately from runtime state.
// --------------------------------------------------------------------------- //

/// `wakeup` is looked up on `PATH` by default; `wakeup-herdr` (this binary)
/// is not, on purpose - see the crate-level docs.
pub const DEFAULT_WAKEUP_BIN: &str = "wakeup";
pub const DEFAULT_HERDR_BIN: &str = "herdr";

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub armed: bool,
    pub display: bool,
    pub start_grace_seconds: u64,
    pub stop_grace_seconds: u64,
    pub statuses: Vec<String>,
    pub notify: bool,
    /// Override for the `wakeup` binary path/name. `None` (the default, and
    /// what a freshly bootstrapped config.json contains) means "resolve
    /// `wakeup` on PATH" - there is no meaningful fixed default value to
    /// show here, it's auto-detected, so it is deliberately omitted from
    /// the JSON file unless a user adds it to override the lookup. Use
    /// [`Config::effective_wakeup_bin`] to read the resolved value.
    pub wakeup_bin: Option<String>,
    /// Same as `wakeup_bin`, for the `herdr` binary. See
    /// [`Config::effective_herdr_bin`].
    pub herdr_bin: Option<String>,
    pub allow_cli_fallback: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            armed: true,
            display: false,
            start_grace_seconds: 5,
            stop_grace_seconds: 30,
            statuses: vec!["working".to_string()],
            notify: true,
            wakeup_bin: None,
            herdr_bin: None,
            allow_cli_fallback: false,
        }
    }
}

impl Config {
    /// Load config from `path`.
    ///
    /// - Missing file: defaults, no error (normal first run).
    /// - Unreadable/corrupt file: defaults, with an error message the caller
    ///   should surface (log it, and/or record it as `last_error`), per the
    ///   "corrupt config falls back safely and reports an error" acceptance
    ///   criterion. This function never panics.
    pub fn load(path: &Path) -> (Config, Option<String>) {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Config::default(), None),
            Err(e) => {
                return (
                    Config::default(),
                    Some(format!("failed to read {}: {e}", path.display())),
                )
            }
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => (Config::from_value(&v), None),
            Err(e) => (
                Config::default(),
                Some(format!("failed to parse {}: {e}", path.display())),
            ),
        }
    }

    /// Like [`Config::load`], but if `path` does not exist at all, writes
    /// out the full set of defaults to it first (best-effort - a save
    /// failure is folded into the returned error rather than blocking
    /// startup) so the file is immediately present and editable after the
    /// very first run, instead of only ever appearing as a side effect of
    /// `arm`/`disarm`. Never overwrites a file that already exists, valid
    /// or corrupt - a corrupt file is still reported as an error and never
    /// touched on disk, so a user's in-progress edit is never clobbered.
    pub fn ensure_bootstrapped(path: &Path) -> (Config, Option<String>) {
        if path.exists() {
            return Config::load(path);
        }
        let cfg = Config::default();
        match cfg.save(path) {
            Ok(()) => (cfg, None),
            Err(e) => (
                cfg,
                Some(format!(
                    "failed to write default config to {}: {e}",
                    path.display()
                )),
            ),
        }
    }

    fn from_value(v: &Value) -> Config {
        let d = Config::default();
        let statuses = v
            .get("statuses")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or(d.statuses);
        Config {
            armed: v.get("armed").and_then(Value::as_bool).unwrap_or(d.armed),
            display: v
                .get("display")
                .and_then(Value::as_bool)
                .unwrap_or(d.display),
            start_grace_seconds: v
                .get("start_grace_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(d.start_grace_seconds),
            stop_grace_seconds: v
                .get("stop_grace_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(d.stop_grace_seconds),
            statuses,
            notify: v.get("notify").and_then(Value::as_bool).unwrap_or(d.notify),
            // No `.unwrap_or(default)` here on purpose: unlike the other
            // fields, `None` *is* the valid, distinct default - it means
            // "auto-detect on PATH", not "value was missing so fall back to
            // a fixed string". A present-but-wrong-type value still falls
            // back to None (auto-detect) rather than erroring.
            wakeup_bin: v
                .get("wakeup_bin")
                .and_then(Value::as_str)
                .map(str::to_string),
            herdr_bin: v
                .get("herdr_bin")
                .and_then(Value::as_str)
                .map(str::to_string),
            allow_cli_fallback: v
                .get("allow_cli_fallback")
                .and_then(Value::as_bool)
                .unwrap_or(d.allow_cli_fallback),
        }
    }

    /// The resolved `wakeup` binary: the config override if one was set,
    /// else the plain PATH-resolvable name.
    pub fn effective_wakeup_bin(&self) -> String {
        self.wakeup_bin
            .clone()
            .unwrap_or_else(|| DEFAULT_WAKEUP_BIN.to_string())
    }

    /// The resolved `herdr` binary: the config override if one was set,
    /// else the plain PATH-resolvable name.
    pub fn effective_herdr_bin(&self) -> String {
        self.herdr_bin
            .clone()
            .unwrap_or_else(|| DEFAULT_HERDR_BIN.to_string())
    }

    fn to_value(&self) -> Value {
        // wakeup_bin/herdr_bin are deliberately omitted from the JSON
        // entirely when unset (None), rather than serialized as null or as
        // their resolved default string: they are auto-detected via PATH,
        // not a real "default value" worth showing in a freshly bootstrapped
        // file, and a bare override key is the clearest way to say "this is
        // a user-added override" if someone does set one. Every other field
        // is a genuine user preference with a meaningful default, so those
        // stay always-present for discoverability (see `doctor`/README).
        let mut map = serde_json::Map::new();
        map.insert("armed".to_string(), serde_json::json!(self.armed));
        map.insert("display".to_string(), serde_json::json!(self.display));
        map.insert(
            "start_grace_seconds".to_string(),
            serde_json::json!(self.start_grace_seconds),
        );
        map.insert(
            "stop_grace_seconds".to_string(),
            serde_json::json!(self.stop_grace_seconds),
        );
        map.insert("statuses".to_string(), serde_json::json!(self.statuses));
        map.insert("notify".to_string(), serde_json::json!(self.notify));
        if let Some(bin) = &self.wakeup_bin {
            map.insert("wakeup_bin".to_string(), serde_json::json!(bin));
        }
        if let Some(bin) = &self.herdr_bin {
            map.insert("herdr_bin".to_string(), serde_json::json!(bin));
        }
        map.insert(
            "allow_cli_fallback".to_string(),
            serde_json::json!(self.allow_cli_fallback),
        );
        Value::Object(map)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text =
            serde_json::to_string_pretty(&self.to_value()).unwrap_or_else(|_| "{}".to_string());
        write_atomic(path, &format!("{text}\n"))
    }
}

// --------------------------------------------------------------------------- //
// Runtime state: watcher-owned, written on every evaluation.
// --------------------------------------------------------------------------- //

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeState {
    pub state: String,
    pub armed: bool,
    pub watcher_pid: u32,
    pub assertion_active: bool,
    pub working_agents: Vec<String>,
    pub agent_count: usize,
    pub last_transition_unix: u64,
    pub last_error: Option<String>,
    /// Wall-clock time this file was last written, updated on *every*
    /// evaluation (unlike `last_transition_unix`, which only updates on an
    /// Acquire/Release). Lets `status`/`doctor` detect a stuck or crashed
    /// watcher: if this is old but the pidfile claims the watcher is
    /// running, something is wrong (Milestone 5: "status identifies stale
    /// watcher state").
    pub checked_at_unix: u64,
    /// The session key this state belongs to, echoed for `doctor` display.
    pub session_key: String,
}

impl RuntimeState {
    fn to_value(&self) -> Value {
        serde_json::json!({
            "state": self.state,
            "armed": self.armed,
            "watcher_pid": self.watcher_pid,
            "assertion_active": self.assertion_active,
            "working_agents": self.working_agents,
            "agent_count": self.agent_count,
            "last_transition_unix": self.last_transition_unix,
            "last_error": self.last_error,
            "checked_at_unix": self.checked_at_unix,
            "session_key": self.session_key,
        })
    }

    fn from_value(v: &Value) -> Option<RuntimeState> {
        Some(RuntimeState {
            state: v.get("state")?.as_str()?.to_string(),
            armed: v.get("armed").and_then(Value::as_bool).unwrap_or(true),
            watcher_pid: v.get("watcher_pid").and_then(Value::as_u64)? as u32,
            assertion_active: v
                .get("assertion_active")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            working_agents: v
                .get("working_agents")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            agent_count: v.get("agent_count").and_then(Value::as_u64).unwrap_or(0) as usize,
            last_transition_unix: v
                .get("last_transition_unix")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            last_error: v
                .get("last_error")
                .and_then(|e| e.as_str())
                .map(str::to_string),
            checked_at_unix: v
                .get("checked_at_unix")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            session_key: v
                .get("session_key")
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .to_string(),
        })
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text =
            serde_json::to_string_pretty(&self.to_value()).unwrap_or_else(|_| "{}".to_string());
        write_atomic(path, &format!("{text}\n"))
    }

    /// Load state from `path`.
    ///
    /// Returns `(None, None)` if there is no state file yet (the watcher has
    /// never run), `(Some(state), None)` on success, or `(None, Some(err))`
    /// if the file exists but is unreadable/corrupt/missing required fields.
    /// Never panics; a corrupt state file must never block anything that
    /// reads it (or, per the acceptance criterion, the watcher's own
    /// startup - though the watcher never reads this file back for its own
    /// decisions in the first place, only writes it).
    pub fn load(path: &Path) -> (Option<RuntimeState>, Option<String>) {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (None, None),
            Err(e) => {
                return (
                    None,
                    Some(format!("failed to read {}: {e}", path.display())),
                )
            }
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => match RuntimeState::from_value(&v) {
                Some(rt) => (Some(rt), None),
                None => (
                    None,
                    Some(format!("{} is missing required fields", path.display())),
                ),
            },
            Err(e) => (
                None,
                Some(format!("failed to parse {}: {e}", path.display())),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tmp_rovo_persist_test_{}_{}_{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ))
    }

    #[test]
    fn session_key_is_deterministic_and_distinguishes_sockets() {
        let a = session_key("/Users/alice/.config/herdr/herdr.sock");
        let b = session_key("/Users/alice/.config/herdr/herdr.sock");
        let c = session_key("/Users/alice/.config/herdr/work-session.sock");
        assert_eq!(a, b, "same socket path must hash to the same key");
        assert_ne!(
            a, c,
            "different socket paths (different sessions) must hash differently"
        );
        // Filesystem-safe: only lowercase hex digits, fixed length.
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn config_dir_and_state_dir_are_session_scoped() {
        // SAFETY: tests in this module run single-threaded enough for this
        // narrow env-var scope (no other test reads these two vars), and we
        // always restore them.
        let prev_socket = std::env::var_os("HERDR_SOCKET_PATH");
        std::env::set_var("HERDR_SOCKET_PATH", "/tmp/session-a.sock");
        let dir_a = state_dir();
        std::env::set_var("HERDR_SOCKET_PATH", "/tmp/session-b.sock");
        let dir_b = state_dir();
        match prev_socket {
            Some(v) => std::env::set_var("HERDR_SOCKET_PATH", v),
            None => std::env::remove_var("HERDR_SOCKET_PATH"),
        }
        assert_ne!(
            dir_a, dir_b,
            "different sessions must resolve to different state dirs"
        );
        assert!(dir_a.to_string_lossy().contains("sessions"));
    }

    #[test]
    fn config_missing_file_returns_defaults_without_error() {
        let path = tmp_path("missing_config.json");
        let (cfg, err) = Config::load(&path);
        assert_eq!(cfg, Config::default());
        assert!(err.is_none());
    }

    #[test]
    fn config_corrupt_file_falls_back_to_defaults_with_error() {
        let path = tmp_path("corrupt_config.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{ not valid json").unwrap();
        let (cfg, err) = Config::load(&path);
        assert_eq!(cfg, Config::default());
        assert!(err.is_some());
        let _ = fs::remove_file(&path);
    }

    /// The gap this closes: previously nothing wrote config.json until
    /// `arm`/`disarm` was run, so a fresh install had no inspectable/
    /// editable file at all. `ensure_bootstrapped` must create one, with the
    /// full set of defaults, the first time anything calls it on a missing
    /// path.
    #[test]
    fn ensure_bootstrapped_creates_file_with_full_defaults_when_missing() {
        let path = tmp_path("bootstrap_missing.json");
        assert!(!path.exists());

        let (cfg, err) = Config::ensure_bootstrapped(&path);
        assert!(err.is_none());
        assert_eq!(cfg, Config::default());
        assert!(path.exists(), "must have written the file to disk");

        // What actually landed on disk must round-trip back to the same
        // defaults, i.e. it is a real, complete, re-loadable config file -
        // not just an in-memory default returned without being persisted.
        let (reloaded, reload_err) = Config::load(&path);
        assert!(reload_err.is_none());
        assert_eq!(reloaded, Config::default());

        let _ = fs::remove_file(&path);
    }

    /// The specific thing being asked for: a freshly bootstrapped
    /// config.json must not contain `wakeup_bin`/`herdr_bin` keys at all
    /// (they're auto-detected, not a real default value worth showing), but
    /// setting one must make it appear in the saved JSON and round-trip back
    /// as an override, and `effective_*` must reflect the resolution either
    /// way.
    #[test]
    fn bin_override_fields_are_omitted_by_default_and_present_when_set() {
        let path = tmp_path("bin_override_omitted.json");
        let (cfg, _) = Config::ensure_bootstrapped(&path);
        assert_eq!(cfg.wakeup_bin, None);
        assert_eq!(cfg.herdr_bin, None);
        assert_eq!(cfg.effective_wakeup_bin(), DEFAULT_WAKEUP_BIN);
        assert_eq!(cfg.effective_herdr_bin(), DEFAULT_HERDR_BIN);

        let raw = fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("wakeup_bin"),
            "default config.json must not mention wakeup_bin at all: {raw}"
        );
        assert!(
            !raw.contains("herdr_bin"),
            "default config.json must not mention herdr_bin at all: {raw}"
        );
        // Every other field is still expected to be present for
        // discoverability - this is not a blanket "omit unset fields" rule.
        assert!(raw.contains("armed"));
        assert!(raw.contains("start_grace_seconds"));
        let _ = fs::remove_file(&path);

        let path2 = tmp_path("bin_override_present.json");
        let custom = Config {
            wakeup_bin: Some("/custom/wakeup".to_string()),
            ..Config::default()
        };
        custom.save(&path2).unwrap();
        let raw2 = fs::read_to_string(&path2).unwrap();
        assert!(
            raw2.contains("\"wakeup_bin\": \"/custom/wakeup\""),
            "an explicit override must be written to disk: {raw2}"
        );
        assert!(
            !raw2.contains("herdr_bin"),
            "herdr_bin was never overridden, so it must still be omitted: {raw2}"
        );

        let (reloaded, err) = Config::load(&path2);
        assert!(err.is_none());
        assert_eq!(reloaded.wakeup_bin.as_deref(), Some("/custom/wakeup"));
        assert_eq!(reloaded.effective_wakeup_bin(), "/custom/wakeup");
        assert_eq!(reloaded.herdr_bin, None);
        assert_eq!(reloaded.effective_herdr_bin(), DEFAULT_HERDR_BIN);
        let _ = fs::remove_file(&path2);
    }

    /// Must never clobber a file that already exists, whether it holds
    /// custom values or is outright corrupt - a user's own edits (or
    /// in-progress edit) are never silently overwritten just by something
    /// calling `ensure_bootstrapped`.
    #[test]
    fn ensure_bootstrapped_never_overwrites_an_existing_file() {
        let path = tmp_path("bootstrap_existing.json");
        let custom = Config {
            armed: false,
            display: true,
            stop_grace_seconds: 999,
            ..Config::default()
        };
        custom.save(&path).unwrap();

        let (cfg, err) = Config::ensure_bootstrapped(&path);
        assert!(err.is_none());
        assert_eq!(
            cfg, custom,
            "must load the existing custom values, not defaults"
        );
        let _ = fs::remove_file(&path);

        let path2 = tmp_path("bootstrap_corrupt.json");
        fs::create_dir_all(path2.parent().unwrap()).unwrap();
        fs::write(&path2, b"{ not valid json").unwrap();
        let (cfg2, err2) = Config::ensure_bootstrapped(&path2);
        assert!(err2.is_some(), "corrupt file must still be reported");
        assert_eq!(cfg2, Config::default(), "falls back to defaults in memory");
        let on_disk = fs::read_to_string(&path2).unwrap();
        assert_eq!(
            on_disk, "{ not valid json",
            "the corrupt file on disk must be left untouched, not repaired/overwritten"
        );
        let _ = fs::remove_file(&path2);
    }

    #[test]
    fn config_round_trips_through_save_and_load() {
        let path = tmp_path("roundtrip_config.json");
        let cfg = Config {
            armed: false,
            display: true,
            start_grace_seconds: 9,
            statuses: vec!["working".into(), "reviewing".into()],
            ..Config::default()
        };
        cfg.save(&path).unwrap();

        let (loaded, err) = Config::load(&path);
        assert!(err.is_none());
        assert_eq!(loaded, cfg);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_save_is_atomic_no_partial_writes_visible() {
        // The temp file must never collide with or clobber the target path
        // mid-write; after save() returns, the target is fully written.
        let path = tmp_path("atomic_config.json");
        let cfg = Config::default();
        cfg.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(serde_json::from_str::<Value>(&text).is_ok());
        // No leftover temp file.
        let tmp = path.with_file_name(format!(
            ".{}.tmp-{}",
            path.file_name().unwrap().to_str().unwrap(),
            std::process::id()
        ));
        assert!(!tmp.exists());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn config_ignores_wrong_typed_fields_and_uses_defaults_for_them() {
        let path = tmp_path("wrong_types_config.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{"armed": "not-a-bool", "start_grace_seconds": "nope", "statuses": []}"#,
        )
        .unwrap();
        let (cfg, err) = Config::load(&path);
        assert!(err.is_none()); // valid JSON, just wrong types for some keys
        assert_eq!(cfg.armed, Config::default().armed);
        assert_eq!(
            cfg.start_grace_seconds,
            Config::default().start_grace_seconds
        );
        assert_eq!(cfg.statuses, Config::default().statuses); // empty array falls back too
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn state_missing_file_is_none_none() {
        let path = tmp_path("missing_state.json");
        let (rt, err) = RuntimeState::load(&path);
        assert!(rt.is_none());
        assert!(err.is_none());
    }

    #[test]
    fn state_corrupt_file_does_not_panic_and_reports_error() {
        let path = tmp_path("corrupt_state.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not json at all").unwrap();
        let (rt, err) = RuntimeState::load(&path);
        assert!(rt.is_none());
        assert!(err.is_some());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn state_round_trips_through_save_and_load() {
        let path = tmp_path("roundtrip_state.json");
        let rt = RuntimeState {
            state: "Awake".to_string(),
            armed: true,
            watcher_pid: 4242,
            assertion_active: true,
            working_agents: vec!["claude@repo".to_string()],
            agent_count: 3,
            last_transition_unix: 1_783_670_000,
            last_error: None,
            checked_at_unix: 1_783_670_005,
            session_key: "0123456789abcdef".to_string(),
        };
        rt.save(&path).unwrap();
        let (loaded, err) = RuntimeState::load(&path);
        assert!(err.is_none());
        assert_eq!(loaded, Some(rt));
        let _ = fs::remove_file(&path);
    }
}
