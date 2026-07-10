//! wakeup-herdr - a Herdr plugin that keeps macOS awake while agents are working.
//!
//! Design: this is a small resident process that subscribes to Herdr's socket
//! event stream (`pane.agent_status_changed` + pane lifecycle). On *any* event it
//! re-fetches an authoritative agent snapshot over the *same socket* (the
//! `agent.list` RPC, not a CLI shellout) and feeds the result into a pure
//! state machine (see `state.rs`):
//!   - working starts, sustained past `--start-grace` -> acquire the `wakeup` assertion
//!   - working stops, sustained past `--grace`         -> release it and let the machine sleep
//!   - brief flickers in either direction do nothing, by design
//!
//! It never tracks per-agent state and never parses event payloads; an event is
//! just a "re-evaluate now" trigger. Policy lives here, mechanism lives in the
//! `wakeup` binary (spawned as `wakeup -i -w <our-pid>`, so it self-releases if we
//! ever die). No `herdr` process is spawned during normal operation; a CLI
//! shellout only happens for `--once`, or if `--allow-cli-fallback` is set and
//! the socket itself is unreachable.
//!
//! It runs in the background (no pane) and surfaces state as toast notifications
//! on transitions: "☕ Keeping awake" when it starts holding the assertion and
//! "💤 Sleep allowed" when it releases.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

mod opts;
use opts::Opts;

mod persist;

mod state;
use state::{Action, Input, StateMachine};

// Set by the signal handler so the main loop can clean up promptly.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: i32) {
    STOP.store(true, Ordering::SeqCst);
}

extern "C" {
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
}

fn install_signals() {
    // SIGINT=2, SIGHUP=1, SIGTERM=15 on macOS.
    unsafe {
        signal(2, on_signal);
        signal(1, on_signal);
        signal(15, on_signal);
    }
}

fn stopping() -> bool {
    STOP.load(Ordering::SeqCst)
}

fn main() {
    // `arm`/`disarm`/`state` are plain positional subcommands (Milestone 4),
    // handled before `Opts::parse()` since they act on the config/state
    // files directly rather than running the watcher. This is what lets
    // `disarm` survive a watcher restart and take effect on a *running*
    // watcher without a restart or signal: it just writes config.json, which
    // the watcher re-reads on every evaluation (see `App::reload_armed`).
    match std::env::args().nth(1).as_deref() {
        Some("arm") => return set_armed(true),
        Some("disarm") => return set_armed(false),
        Some("state") => return print_state(),
        _ => {}
    }

    let o = Opts::parse();

    if o.once {
        let snap = fetch_snapshot(&o);
        report_once(&snap, &o);
        return;
    }

    install_signals();
    let mut app = App::new(o);
    app.log(&format!(
        "start pid={} socket={} start_grace={:.0}s stop_grace={:.0}s backstop={:.0}s display={}",
        std::process::id(),
        app.o.socket,
        app.o.start_grace.as_secs_f64(),
        app.o.grace.as_secs_f64(),
        app.o.backstop.as_secs_f64(),
        app.o.display,
    ));
    app.run();
}

/// `wakeup-herdr arm` / `wakeup-herdr disarm`: flip the persisted `armed`
/// flag in config.json. Works whether or not the watcher is currently
/// running, and a running watcher picks up the change on its next
/// evaluation (see `App::reload_armed`) - so disarm both survives a watcher
/// restart and takes effect live without one.
fn set_armed(armed: bool) {
    let path = persist::config_path();
    let (mut cfg, err) = persist::Config::load(&path);
    if let Some(e) = &err {
        eprintln!("wakeup-herdr: config error (continuing with defaults): {e}");
    }
    cfg.armed = armed;
    match cfg.save(&path) {
        Ok(()) => println!("{}", if armed { "armed" } else { "disarmed" }),
        Err(e) => {
            eprintln!("wakeup-herdr: failed to save {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

/// `wakeup-herdr state`: print the watcher's last-persisted runtime state
/// (Milestone 4). Reading this never requires a running watcher, a socket
/// connection, or a `herdr` process; it only ever reads a local file, and a
/// corrupt/missing one is reported without panicking.
fn print_state() {
    match persist::RuntimeState::load(&persist::state_path()) {
        (Some(rt), _) => {
            println!("state: {} (armed={})", rt.state, rt.armed);
            println!("watcher_pid: {}", rt.watcher_pid);
            println!("assertion_active: {}", rt.assertion_active);
            println!(
                "agents: {} matching, {} total",
                rt.working_agents.len(),
                rt.agent_count
            );
            for w in &rt.working_agents {
                println!("  working: {w}");
            }
            println!("last_transition_unix: {}", rt.last_transition_unix);
            if let Some(e) = &rt.last_error {
                println!("last_error: {e}");
            }
        }
        (None, Some(e)) => println!("state: unavailable ({e})"),
        (None, None) => println!("state: no runtime state yet (watcher has not run)"),
    }
}

// --------------------------------------------------------------------------- //
// Snapshot of Herdr agents
// --------------------------------------------------------------------------- //

struct Snapshot {
    available: bool,
    working: Vec<String>,    // labels of working agents
    panes: BTreeSet<String>, // pane ids of all detected agents
    total: usize,
}

impl Snapshot {
    fn unavailable() -> Self {
        Snapshot {
            available: false,
            working: vec![],
            panes: BTreeSet::new(),
            total: 0,
        }
    }
}

/// CLI fallback: shells out to `herdr agent list`. Used only for `--once`,
/// diagnostics, and (opt-in) `--allow-cli-fallback` when the socket itself is
/// unreachable. Normal event-driven operation uses `agent_list_via_socket`
/// instead, so no Herdr process is spawned per evaluation.
fn herdr_agents_cli(o: &Opts) -> Snapshot {
    let out = run_capture(&o.herdr_bin, &["agent", "list"], Duration::from_secs(8));
    let out = match out {
        Some(s) if !s.trim().is_empty() => s,
        _ => return Snapshot::unavailable(),
    };
    match serde_json::from_str::<serde_json::Value>(&out) {
        Ok(v) => snapshot_from_response(&v, o),
        Err(_) => Snapshot::unavailable(),
    }
}

/// Parse the `{"result": {"agents": [...]}}` shape shared by both the
/// `herdr agent list` CLI output and the socket `agent.list` RPC reply.
fn snapshot_from_response(v: &serde_json::Value, o: &Opts) -> Snapshot {
    let agents = v
        .get("result")
        .and_then(|r| r.get("agents"))
        .and_then(|a| a.as_array());
    let agents = match agents {
        Some(a) => a,
        None => return Snapshot::unavailable(),
    };
    let mut working = Vec::new();
    let mut panes = BTreeSet::new();
    for a in agents {
        if let Some(p) = a.get("pane_id").and_then(|v| v.as_str()) {
            panes.insert(p.to_string());
        }
        let status = a
            .get("agent_status")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_lowercase();
        if o.statuses.iter().any(|s| s == &status) {
            working.push(agent_label(a));
        }
    }
    Snapshot {
        available: true,
        working,
        panes,
        total: agents.len(),
    }
}

/// The RPC id used for socket `agent.list` requests, per the improvement
/// plan's `herdr-wakeup:agent:list` convention.
const AGENT_LIST_ID: &str = "herdr-wakeup:agent:list";

/// Fetch a snapshot using an already-connected socket. Split out from
/// `agent_list_via_socket` so it is unit-testable with an in-memory
/// `UnixStream::pair()` instead of a real bound socket path.
fn agent_list_via_conn(conn: &mut Conn, o: &Opts, timeout: Duration) -> Snapshot {
    match conn.agent_list(AGENT_LIST_ID, timeout) {
        Some(v) => snapshot_from_response(&v, o),
        None => Snapshot::unavailable(),
    }
}

/// Fetch one snapshot over a fresh, short-lived socket connection instead of
/// shelling out to `herdr agent list`. The Herdr socket protocol is
/// single-request-per-connection (verified empirically: the server closes
/// the connection after answering one RPC, except when the request is
/// `events.subscribe`, which stays open to push events) - so every snapshot
/// gets its own connection rather than reusing the subscription's `Conn`.
fn agent_list_via_socket(o: &Opts, timeout: Duration) -> Snapshot {
    match UnixStream::connect(&o.socket) {
        Ok(stream) => agent_list_via_conn(&mut Conn::new(stream), o, timeout),
        Err(_) => Snapshot::unavailable(),
    }
}

/// Fetch a fresh snapshot: the socket `agent.list` RPC is the primary path,
/// falling back to the CLI only when `--allow-cli-fallback` is set and the
/// socket itself didn't answer. Shared by the running watcher (`App::
/// snapshot`) and `--once`/diagnostics, so `--once` also does "a fresh
/// socket check" per Milestone 4 rather than always shelling out.
fn fetch_snapshot(o: &Opts) -> Snapshot {
    let snap = agent_list_via_socket(o, Duration::from_secs(5));
    if snap.available || !o.allow_cli_fallback {
        snap
    } else {
        herdr_agents_cli(o)
    }
}

fn agent_label(a: &serde_json::Value) -> String {
    let name = a
        .get("agent")
        .and_then(|v| v.as_str())
        .or_else(|| a.get("terminal_id").and_then(|v| v.as_str()))
        .unwrap_or("agent");
    let cwd = a
        .get("foreground_cwd")
        .and_then(|v| v.as_str())
        .or_else(|| a.get("cwd").and_then(|v| v.as_str()))
        .unwrap_or("");
    let tail = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if tail.is_empty() {
        name.to_string()
    } else {
        format!("{name}@{tail}")
    }
}

fn report_once(snap: &Snapshot, o: &Opts) {
    if !snap.available {
        println!("herdr: unavailable");
    } else {
        println!(
            "herdr: {} agent(s), {} matching [{}]",
            snap.total,
            snap.working.len(),
            o.statuses.join(",")
        );
        for w in &snap.working {
            println!("  working: {w}");
        }
    }
    println!(
        "decision: {}",
        if snap.working.is_empty() {
            "allow sleep"
        } else {
            "keep awake"
        }
    );
}

// --------------------------------------------------------------------------- //
// wakeup child management
// --------------------------------------------------------------------------- //

struct Keeper {
    bin: String,
    args: Vec<String>,
    child: Option<Child>,
}

impl Keeper {
    fn new(bin: String, display: bool) -> Self {
        let pid = std::process::id();
        let mut args: Vec<String> = Vec::new();
        args.push(if display { "-di".into() } else { "-i".into() });
        args.push("-w".into());
        args.push(pid.to_string());
        Keeper {
            bin,
            args,
            child: None,
        }
    }
    fn running(&mut self) -> bool {
        match &mut self.child {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }
    fn start(&mut self) -> std::io::Result<()> {
        self.child = Some(Command::new(&self.bin).args(&self.args).spawn()?);
        Ok(())
    }
    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

// --------------------------------------------------------------------------- //
// App
// --------------------------------------------------------------------------- //

struct App {
    o: Opts,
    keeper: Keeper,
    sm: StateMachine,
    next_eval: Instant, // next self-initiated backstop evaluation
    subscribed: BTreeSet<String>,
    unavailable_since: Option<Instant>, // first time Herdr became unreachable
    herdr_gone: bool,                   // Herdr unreachable past --exit-after: time to quit
    config_path: PathBuf,
    state_path: PathBuf,
    config_error: Option<String>, // last config-load error, to log only on change
    last_transition_unix: u64,    // unix time of the last Acquire/Release
}

enum Poll {
    Activity,
    Timeout,
    Closed,
}

impl App {
    fn new(o: Opts) -> Self {
        let keeper = Keeper::new(o.wakeup_bin.clone(), o.display);
        let sm = StateMachine::new(o.start_grace, o.grace);
        App {
            o,
            keeper,
            sm,
            next_eval: Instant::now(),
            subscribed: BTreeSet::new(),
            unavailable_since: None,
            herdr_gone: false,
            config_path: persist::config_path(),
            state_path: persist::state_path(),
            config_error: None,
            last_transition_unix: unix_now(),
        }
    }

    fn log(&self, msg: &str) {
        if self.o.quiet {
            return;
        }
        println!("{} {msg}", now_ts());
        let _ = std::io::stdout().flush();
    }

    fn run(&mut self) {
        while !stopping() && !self.herdr_gone {
            let had_session = self.run_socket_session();
            if self.herdr_gone {
                break;
            }
            if !had_session {
                // No event-driven session ran this round (couldn't connect, or
                // the bootstrap agent.list/subscribe failed): retry sooner
                // while Herdr is unreachable, otherwise at the backstop
                // interval, so event-driven mode resumes as soon as Herdr is
                // reachable again (e.g. after a quick server restart).
                let nap = if self.unavailable_since.is_some() {
                    Duration::from_secs(5)
                } else {
                    self.o.backstop
                };
                let deadline = Instant::now() + nap;
                while !stopping() && !self.herdr_gone && Instant::now() < deadline {
                    let left = deadline.saturating_duration_since(Instant::now());
                    std::thread::sleep(Duration::from_millis(500).min(left));
                }
            }
            if stopping() || self.herdr_gone {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        self.shutdown();
    }

    /// Snapshot, subscribe, and run one event-driven session. No `herdr`
    /// process is spawned in this path (see `agent_list_via_socket`). Returns
    /// true iff a session actually ran, so the caller can reconnect quickly
    /// instead of using the longer "unreachable" nap.
    fn run_socket_session(&mut self) -> bool {
        // Bootstrap: learn current panes and the initial working set over a
        // short-lived socket connection, so no CLI shellout is needed just to
        // build the per-pane subscription list.
        let snap = self.snapshot();
        if !snap.available {
            self.apply(snap, "connect");
            return false;
        }

        // The subscription connection is separate and long-lived: the Herdr
        // socket protocol only answers one request per connection, except
        // `events.subscribe`, which keeps the connection open to push events.
        let stream = match UnixStream::connect(&self.o.socket) {
            Ok(s) => s,
            Err(_) => {
                self.apply(Snapshot::unavailable(), "connect");
                return false;
            }
        };
        let mut conn = Conn::new(stream);
        if !self.subscribe(&mut conn, &snap.panes) {
            // Still apply this snapshot even though the subscription ack
            // failed, so a transient subscribe hiccup doesn't also cost us a
            // decision this round.
            self.apply(snap, "connect");
            return false;
        }

        self.subscribed = snap.panes.clone();
        self.apply(snap, "connected");
        if self.herdr_gone {
            return true;
        }
        self.next_eval = Instant::now() + self.o.backstop;
        self.stream_loop(&mut conn);
        true
    }

    /// Fetch a fresh snapshot (see the free function `fetch_snapshot`).
    fn snapshot(&self) -> Snapshot {
        fetch_snapshot(&self.o)
    }

    /// Read events on the subscription connection until the pane set
    /// changes, the connection drops, or we stop.
    fn stream_loop(&mut self, conn: &mut Conn) {
        loop {
            if stopping() {
                return;
            }
            // Wake at least once a second so we notice signals promptly, but
            // only re-query Herdr on real events or when a deadline elapses.
            let to = Duration::from_millis(1000).min(self.until_deadline());
            match conn.wait(to) {
                Poll::Closed => {
                    self.log("socket closed; reconnecting");
                    return;
                }
                Poll::Timeout => {
                    if stopping() {
                        return;
                    }
                    if self.deadline_passed() {
                        let snap = self.snapshot();
                        self.apply(snap, "tick");
                        if self.herdr_gone {
                            return;
                        }
                        self.next_eval = Instant::now() + self.o.backstop;
                    }
                }
                Poll::Activity => {
                    conn.drain(self.o.debounce);
                    let snap = self.snapshot();
                    let changed = self.apply(snap, "event");
                    if self.herdr_gone {
                        return;
                    }
                    self.next_eval = Instant::now() + self.o.backstop;
                    if changed {
                        // pane membership changed -> reconnect to re-subscribe.
                        return;
                    }
                }
            }
        }
    }

    fn until_deadline(&self) -> Duration {
        let now = Instant::now();
        let mut d = self.next_eval.saturating_duration_since(now);
        if let Some(r) = self.sm.deadline() {
            d = d.min(r.saturating_duration_since(now));
        }
        d
    }

    fn deadline_passed(&self) -> bool {
        let now = Instant::now();
        now >= self.next_eval || self.sm.deadline().map(|r| now >= r).unwrap_or(false)
    }

    fn shutdown(&mut self) {
        self.keeper.stop();
        self.log("stopped; assertion released");
    }

    /// Feed an already-fetched snapshot into the state machine and toggle
    /// wakeup accordingly. Returns true if the pane set changed (so the
    /// caller knows to re-subscribe).
    fn apply(&mut self, snap: Snapshot, reason: &str) -> bool {
        let now = Instant::now();

        // Re-read `armed` from the config file on every evaluation (not just
        // at startup), so `wakeup-herdr disarm`/`arm` take effect on an
        // already-running watcher without a restart (Milestone 4).
        let armed = self.reload_armed();

        // Tie our lifecycle to the Herdr server: if it stays unreachable past the
        // tolerance window (surviving brief blips and restarts), quit. There are
        // no agents to track without Herdr, so lingering would be pointless.
        if snap.available {
            self.unavailable_since = None;
        } else if !self.o.exit_after.is_zero() {
            let since = *self.unavailable_since.get_or_insert(now);
            if now.duration_since(since) >= self.o.exit_after {
                self.log(&format!(
                    "[{reason}] herdr unreachable for {:.0}s; exiting",
                    now.duration_since(since).as_secs_f64()
                ));
                self.herdr_gone = true;
                self.write_state(&snap, armed);
                return false;
            }
        }

        let working = !snap.working.is_empty();
        // When disarmed, force an immediate release (bypassing stop_grace)
        // instead of stepping the normal state machine at all.
        let action = if armed {
            self.sm.step(
                Input {
                    available: snap.available,
                    working,
                },
                now,
            )
        } else {
            self.sm.force_off()
        };

        if self.o.verbose {
            self.log(&format!(
                "[{reason}] working=[{}] armed={armed} state={} action={:?}{}",
                snap.working.join(", "),
                self.sm.state(),
                action,
                self.sm
                    .last_error()
                    .map(|e| format!(" last_error={e:?}"))
                    .unwrap_or_default(),
            ));
        }

        match action {
            Action::Acquire => {
                match self.keeper.start() {
                    Ok(()) => {
                        let detail = summarize(&snap.working);
                        self.log(&format!("AWAKE   -> {detail}"));
                        self.toast("☕ Keeping awake", &detail);
                    }
                    Err(e) => self.log(&format!("failed to start wakeup: {e}")),
                }
                self.last_transition_unix = unix_now();
            }
            Action::Release => {
                self.keeper.stop();
                if armed {
                    self.log("SLEEP-OK <- all agents idle/blocked");
                    self.toast("💤 Sleep allowed", "no agents working");
                } else {
                    self.log("DISARMED <- released wake assertion");
                    self.toast("⏸ Disarmed", "wake assertions paused");
                }
                self.last_transition_unix = unix_now();
            }
            Action::None => {
                // No transition this round, but make sure the assertion is
                // actually still held whenever the state machine believes it
                // should be (recovers a crashed/killed `wakeup` child). Only
                // applies while armed: `force_off` already guarantees
                // `sm.holding()` is false whenever disarmed.
                if self.sm.holding() && !self.keeper.running() {
                    match self.keeper.start() {
                        Ok(()) => self.log("restarted wakeup (child had exited)"),
                        Err(e) => self.log(&format!("failed to restart wakeup: {e}")),
                    }
                }
            }
        }

        self.write_state(&snap, armed);

        // report pane-set change for re-subscribe
        snap.available && snap.panes != self.subscribed
    }

    // ----- Herdr visual surfaces ----- //

    fn toast(&self, title: &str, body: &str) {
        if self.o.no_notify {
            return;
        }
        let _ = run_status(
            &self.o.herdr_bin,
            &[
                "notification",
                "show",
                title,
                "--body",
                body,
                "--sound",
                "none",
            ],
        );
    }

    /// Subscribe to lifecycle + per-pane agent status events on an already-
    /// connected socket. Returns true iff the subscription was acknowledged.
    fn subscribe(&self, conn: &mut Conn, panes: &BTreeSet<String>) -> bool {
        let mut subs = vec![
            serde_json::json!({"type": "pane.created"}),
            serde_json::json!({"type": "pane.exited"}),
            serde_json::json!({"type": "pane.agent_detected"}),
        ];
        for p in panes {
            subs.push(serde_json::json!({"type": "pane.agent_status_changed", "pane_id": p}));
        }
        let req = serde_json::json!({
            "id": "wakeup-sub",
            "method": "events.subscribe",
            "params": {"subscriptions": subs},
        });
        if conn.send(&req).is_err() {
            return false;
        }
        // Expect the explicit subscription_started ack within a short window.
        matches!(conn.recv_json(Duration::from_secs(3)), Some(v) if subscription_started(&v))
    }

    /// Re-read the `armed` flag from config.json. Falls back to `true`
    /// (armed) on a missing/corrupt config, matching `Config::default()`; a
    /// corrupt config is only logged when the error text changes (entering
    /// or leaving the error), not on every single evaluation.
    fn reload_armed(&mut self) -> bool {
        let (cfg, err) = persist::Config::load(&self.config_path);
        if err != self.config_error {
            match &err {
                Some(e) => self.log(&format!("config error (using defaults): {e}")),
                None => self.log("config recovered"),
            }
            self.config_error = err;
        }
        cfg.armed
    }

    /// Persist the watcher's current runtime state atomically (Milestone 4).
    /// Nothing in this process ever reads it back for its own decisions -
    /// only the separate, short-lived `wakeup-herdr state` invocation does -
    /// so a slow or failing write here can never affect a wake/sleep
    /// decision; failures are just logged.
    fn write_state(&mut self, snap: &Snapshot, armed: bool) {
        let rt = persist::RuntimeState {
            state: self.sm.state().to_string(),
            armed,
            watcher_pid: std::process::id(),
            assertion_active: self.sm.holding(),
            working_agents: snap.working.clone(),
            agent_count: snap.total,
            last_transition_unix: self.last_transition_unix,
            last_error: self.sm.last_error().map(str::to_string),
        };
        if let Err(e) = rt.save(&self.state_path) {
            self.log(&format!("failed to write state file: {e}"));
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.keeper.stop();
    }
}

/// Current wall-clock time as Unix seconds, used for `last_transition_unix`
/// in the persisted runtime state. Defaults to 0 on the (essentially
/// impossible on a real system) case that the clock is before the epoch.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn summarize(working: &[String]) -> String {
    let n = working.len();
    let head: Vec<&str> = working.iter().take(3).map(|s| s.as_str()).collect();
    let mut s = format!("{n} working: {}", head.join(", "));
    if n > 3 {
        s.push_str(&format!(", +{}", n - 3));
    }
    s
}

// --------------------------------------------------------------------------- //
// Socket connection: newline-delimited JSON
// --------------------------------------------------------------------------- //

struct Conn {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl Conn {
    fn new(stream: UnixStream) -> Self {
        Conn {
            stream,
            buf: Vec::new(),
        }
    }

    fn send(&mut self, v: &serde_json::Value) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(v).unwrap();
        line.push(b'\n');
        self.stream.write_all(&line)
    }

    fn recv_json(&mut self, timeout: Duration) -> Option<serde_json::Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(line) = self.pop_line() {
                return serde_json::from_slice::<serde_json::Value>(&line).ok();
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            if matches!(
                self.wait(deadline.saturating_duration_since(now)),
                Poll::Closed
            ) {
                return None;
            }
        }
    }

    /// Send an `agent.list` RPC and wait for the reply matching `id`, per
    /// Milestone 3 of the improvement plan (socket snapshots instead of
    /// shelling out to `herdr agent list`). Any other message received while
    /// waiting (e.g. a subscription event arriving mid-request) is ignored:
    /// we are about to have a fresh snapshot anyway, so nothing is lost.
    fn agent_list(&mut self, id: &str, timeout: Duration) -> Option<serde_json::Value> {
        let req = serde_json::json!({"id": id, "method": "agent.list", "params": {}});
        self.send(&req).ok()?;
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let v = self.recv_json(deadline.saturating_duration_since(now))?;
            if v.get("id").and_then(|i| i.as_str()) == Some(id) {
                return Some(v);
            }
        }
    }

    /// Block up to `timeout` for at least one complete message.
    fn wait(&mut self, timeout: Duration) -> Poll {
        if self.has_line() {
            return Poll::Activity;
        }
        let _ = self.stream.set_read_timeout(Some(timeout));
        let mut tmp = [0u8; 8192];
        match self.stream.read(&mut tmp) {
            Ok(0) => Poll::Closed,
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                if self.has_line() {
                    Poll::Activity
                } else {
                    Poll::Timeout
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                Poll::Timeout
            }
            Err(_) => Poll::Closed,
        }
    }

    /// Swallow any further buffered/immediately-available messages so a burst of
    /// events collapses into a single evaluation.
    fn drain(&mut self, window: Duration) {
        self.consume_lines();
        let _ = self.stream.set_read_timeout(Some(window));
        let mut tmp = [0u8; 8192];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    self.buf.extend_from_slice(&tmp[..n]);
                    self.consume_lines();
                }
                Err(_) => break,
            }
        }
    }

    fn has_line(&self) -> bool {
        self.buf.contains(&b'\n')
    }

    fn consume_lines(&mut self) {
        while self.pop_line().is_some() {}
    }

    fn pop_line(&mut self) -> Option<Vec<u8>> {
        if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=i).collect();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            Some(line)
        } else {
            None
        }
    }
}

fn subscription_started(v: &serde_json::Value) -> bool {
    v.get("id").and_then(|id| id.as_str()) == Some("wakeup-sub")
        && v.get("result")
            .and_then(|r| r.get("type"))
            .and_then(|t| t.as_str())
            == Some("subscription_started")
}

// --------------------------------------------------------------------------- //
// small process helpers
// --------------------------------------------------------------------------- //

fn run_capture(bin: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let child = Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    let mut child = child;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut s = String::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_string(&mut s);
                }
                return if status.success() { Some(s) } else { None };
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(_) => return None,
        }
    }
}

fn run_status(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Local wall-clock HH:MM:SS for log lines, computed without spawning a process
/// per log. The local UTC offset is learned once (one `date +%z` at startup) and
/// cached; everything after is pure arithmetic on the system clock.
fn now_ts() -> String {
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    static OFFSET: OnceLock<i64> = OnceLock::new();
    let offset = *OFFSET.get_or_init(|| {
        run_capture("date", &["+%z"], Duration::from_secs(2))
            .and_then(|s| parse_utc_offset(s.trim()))
            .unwrap_or(0)
    });
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let day = (epoch + offset).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

fn parse_utc_offset(z: &str) -> Option<i64> {
    // e.g. "+0530" or "-0800"
    if z.len() < 5 {
        return None;
    }
    let sign = if z.starts_with('-') { -1 } else { 1 };
    let h: i64 = z.get(1..3)?.parse().ok()?;
    let m: i64 = z.get(3..5)?.parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_subscription_ack() {
        let v = serde_json::json!({
            "id": "wakeup-sub",
            "result": {"type": "subscription_started"}
        });
        assert!(subscription_started(&v));
    }

    #[test]
    fn rejects_unrelated_socket_activity_as_ack() {
        let v = serde_json::json!({
            "type": "pane.agent_status_changed",
            "pane_id": "w1:p1"
        });
        assert!(!subscription_started(&v));
    }

    #[test]
    fn parses_and_removes_one_buffered_line() {
        let (client, mut server) = UnixStream::pair().unwrap();
        server.write_all(b"{\"one\":1}\n{\"two\":2}\n").unwrap();
        let mut conn = Conn::new(client);

        let first = conn.recv_json(Duration::from_secs(1)).unwrap();
        assert_eq!(first.get("one").and_then(|v| v.as_i64()), Some(1));
        assert!(conn.has_line());

        let second = conn.recv_json(Duration::from_secs(1)).unwrap();
        assert_eq!(second.get("two").and_then(|v| v.as_i64()), Some(2));
    }

    fn test_opts(statuses: &[&str]) -> Opts {
        Opts {
            socket: String::new(),
            herdr_bin: "herdr".into(),
            wakeup_bin: "wakeup".into(),
            display: false,
            statuses: statuses.iter().map(|s| s.to_string()).collect(),
            start_grace: Duration::from_secs(5),
            grace: Duration::from_secs(30),
            backstop: Duration::from_secs(60),
            debounce: Duration::from_millis(400),
            exit_after: Duration::from_secs(120),
            no_notify: true,
            verbose: false,
            quiet: true,
            once: false,
            allow_cli_fallback: false,
        }
    }

    /// The real shape returned by the socket `agent.list` RPC, captured live
    /// against a running Herdr server (also matches the CLI's JSON output
    /// under the same `result.agents` path).
    fn sample_agent_list_response() -> serde_json::Value {
        serde_json::json!({
            "id": "herdr-wakeup:agent:list",
            "result": {
                "type": "agent_list",
                "agents": [
                    {
                        "terminal_id": "term_1",
                        "agent": "claude",
                        "agent_status": "working",
                        "pane_id": "w4:p14",
                        "cwd": "/Users/x/workspace/ai-forge-builder",
                        "foreground_cwd": "/Users/x/workspace/ai-forge-builder"
                    },
                    {
                        "terminal_id": "term_2",
                        "agent": "codex",
                        "agent_status": "idle",
                        "pane_id": "w4:p15",
                        "cwd": "/Users/x/workspace/other",
                        "foreground_cwd": "/Users/x/workspace/other"
                    }
                ]
            }
        })
    }

    #[test]
    fn snapshot_from_response_parses_real_agent_list_shape() {
        let o = test_opts(&["working"]);
        let snap = snapshot_from_response(&sample_agent_list_response(), &o);
        assert!(snap.available);
        assert_eq!(snap.total, 2);
        assert_eq!(snap.working, vec!["claude@ai-forge-builder".to_string()]);
        assert_eq!(
            snap.panes,
            BTreeSet::from(["w4:p14".to_string(), "w4:p15".to_string()])
        );
    }

    #[test]
    fn snapshot_from_response_honors_custom_statuses() {
        let o = test_opts(&["working", "idle"]);
        let snap = snapshot_from_response(&sample_agent_list_response(), &o);
        assert_eq!(snap.working.len(), 2);
    }

    #[test]
    fn snapshot_from_response_rejects_malformed_payloads_without_panicking() {
        let o = test_opts(&["working"]);
        for bad in [
            serde_json::json!({"unexpected": "shape"}),
            serde_json::json!({"result": {}}),
            serde_json::json!({"result": {"agents": "not-an-array"}}),
            serde_json::json!(null),
        ] {
            let snap = snapshot_from_response(&bad, &o);
            assert!(!snap.available);
            assert_eq!(snap.total, 0);
        }
    }

    #[test]
    fn agent_list_via_conn_ignores_unrelated_events_first() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let mut conn = Conn::new(client);

        // Simulate a subscription event arriving just before our RPC reply.
        server
            .write_all(b"{\"type\":\"pane.created\",\"pane_id\":\"w1:p1\"}\n")
            .unwrap();
        let response = sample_agent_list_response();
        server
            .write_all(format!("{}\n", response).as_bytes())
            .unwrap();

        let o = test_opts(&["working"]);
        let snap = agent_list_via_conn(&mut conn, &o, Duration::from_secs(2));
        assert!(snap.available);
        assert_eq!(snap.total, 2);
    }

    #[test]
    fn agent_list_via_conn_reports_unavailable_on_timeout() {
        let (client, _server) = UnixStream::pair().unwrap();
        let mut conn = Conn::new(client);
        let o = test_opts(&["working"]);
        // Nothing is ever written by the peer, so this should time out cleanly.
        let snap = agent_list_via_conn(&mut conn, &o, Duration::from_millis(200));
        assert!(!snap.available);
    }

    #[test]
    fn agent_list_via_socket_reports_unavailable_when_socket_path_is_bogus() {
        let o = Opts {
            socket: "/tmp/tmp_rovo_nonexistent_herdr_socket_for_tests.sock".into(),
            ..test_opts(&["working"])
        };
        let snap = agent_list_via_socket(&o, Duration::from_millis(200));
        assert!(!snap.available);
    }

    // ----------------------------------------------------------------- //
    // Milestone 4: config/state persistence, live armed override
    // ----------------------------------------------------------------- //

    fn test_app(statuses: &[&str]) -> App {
        let mut o = test_opts(statuses);
        // A harmless, near-instant no-op binary instead of the real `wakeup`,
        // so these tests never depend on (or actually hold) a real macOS
        // power assertion.
        o.wakeup_bin = "true".into();
        o.start_grace = Duration::from_secs(0);
        App::new(o)
    }

    /// A fresh temp dir per test, used for both config.json and state.json,
    /// so tests never share or race on `HERDR_PLUGIN_CONFIG_DIR`/`_STATE_DIR`
    /// (which are process-global env vars and would be flaky under parallel
    /// test execution). `App::config_path`/`state_path` are overridden
    /// directly instead.
    fn tmp_paths(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "tmp_rovo_app_test_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        (base.join("config.json"), base.join("state.json"))
    }

    fn working_snapshot() -> Snapshot {
        Snapshot {
            available: true,
            working: vec!["claude@repo".to_string()],
            panes: BTreeSet::from(["w1:p1".to_string()]),
            total: 1,
        }
    }

    fn idle_snapshot() -> Snapshot {
        Snapshot {
            available: true,
            working: vec![],
            panes: BTreeSet::from(["w1:p1".to_string()]),
            total: 1,
        }
    }

    fn cleanup(cfg_path: &std::path::Path) {
        let _ = fs::remove_dir_all(cfg_path.parent().unwrap());
    }

    use std::fs;

    /// Acceptance criterion: disarm survives a watcher restart. A config
    /// file with `armed: false` written *before* the watcher ever starts
    /// must prevent it from acquiring on a fresh `App`, exactly as if
    /// `disarm` had been run while it was already running.
    #[test]
    fn disarmed_config_prevents_acquire_from_a_fresh_start() {
        let (cfg_path, state_path) = tmp_paths("disarm_survives_restart");
        persist::Config {
            armed: false,
            ..persist::Config::default()
        }
        .save(&cfg_path)
        .unwrap();

        let mut app = test_app(&["working"]);
        app.config_path = cfg_path.clone();
        app.state_path = state_path;

        app.apply(working_snapshot(), "test");
        assert_eq!(app.sm.state(), state::State::Off);
        assert!(!app.sm.holding());

        cleanup(&cfg_path);
    }

    /// Disarming a currently-Awake watcher must release immediately (not
    /// wait out stop_grace), and re-arming must resume normal behavior.
    #[test]
    fn disarm_releases_mid_awake_and_rearm_resumes_normal_behavior() {
        let (cfg_path, state_path) = tmp_paths("disarm_mid_awake");
        let mut app = test_app(&["working"]);
        app.config_path = cfg_path.clone();
        app.state_path = state_path.clone();

        // start_grace is 0, but the state machine still needs a second
        // evaluation past Off -> PendingWake to check the (already-elapsed)
        // deadline and transition to Awake.
        app.apply(working_snapshot(), "test");
        app.apply(working_snapshot(), "test");
        assert_eq!(app.sm.state(), state::State::Awake);
        assert!(app.sm.holding());

        persist::Config {
            armed: false,
            ..persist::Config::default()
        }
        .save(&cfg_path)
        .unwrap();
        // Still "working" in the snapshot, but disarmed must win over that.
        app.apply(working_snapshot(), "test");
        assert_eq!(app.sm.state(), state::State::Off);
        assert!(!app.sm.holding());

        let (rt, err) = persist::RuntimeState::load(&state_path);
        assert!(err.is_none());
        let rt = rt.unwrap();
        assert!(!rt.armed);
        assert_eq!(rt.state, "Off");
        assert!(!rt.assertion_active);

        // Re-arm: normal acquire behavior must resume.
        persist::Config::default().save(&cfg_path).unwrap();
        app.apply(working_snapshot(), "test");
        app.apply(working_snapshot(), "test");
        assert_eq!(app.sm.state(), state::State::Awake);
        assert!(app.sm.holding());

        cleanup(&cfg_path);
    }

    /// Acceptance criterion: corrupt config falls back safely (defaults to
    /// armed) and does not panic or otherwise disrupt evaluation.
    #[test]
    fn corrupt_config_during_operation_falls_back_to_armed_without_panicking() {
        let (cfg_path, state_path) = tmp_paths("corrupt_config_live");
        fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        fs::write(&cfg_path, b"{ not valid json").unwrap();

        let mut app = test_app(&["working"]);
        app.config_path = cfg_path.clone();
        app.state_path = state_path;

        app.apply(working_snapshot(), "test");
        app.apply(working_snapshot(), "test");
        assert_eq!(app.sm.state(), state::State::Awake);

        cleanup(&cfg_path);
    }

    /// Acceptance criterion: corrupt state does not prevent watcher startup
    /// (or, here, any ongoing evaluation) - by design the watcher never reads
    /// state.json back for its own decisions, only writes it, so a pre-
    /// existing corrupt one is simply overwritten cleanly.
    #[test]
    fn corrupt_state_file_does_not_block_evaluation_and_gets_overwritten() {
        let (cfg_path, state_path) = tmp_paths("corrupt_state_live");
        fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        fs::write(&state_path, b"not json at all").unwrap();

        let mut app = test_app(&["working"]);
        app.config_path = cfg_path.clone();
        app.state_path = state_path.clone();

        app.apply(idle_snapshot(), "test");
        assert_eq!(app.sm.state(), state::State::Off);

        let (rt, err) = persist::RuntimeState::load(&state_path);
        assert!(err.is_none());
        assert_eq!(rt.unwrap().state, "Off");

        cleanup(&cfg_path);
    }

    /// State writes report every evaluation's outcome, including the
    /// working-agent labels and count, for `wakeup-herdr state` to display.
    #[test]
    fn state_file_reflects_working_agents_and_last_transition() {
        let (cfg_path, state_path) = tmp_paths("state_reflects_agents");
        let mut app = test_app(&["working"]);
        app.config_path = cfg_path.clone();
        app.state_path = state_path.clone();

        app.apply(working_snapshot(), "test");
        app.apply(working_snapshot(), "test");
        let (rt, _) = persist::RuntimeState::load(&state_path);
        let rt = rt.unwrap();
        assert_eq!(rt.state, "Awake");
        assert!(rt.assertion_active);
        assert_eq!(rt.working_agents, vec!["claude@repo".to_string()]);
        assert_eq!(rt.agent_count, 1);
        assert!(rt.last_transition_unix > 0);

        cleanup(&cfg_path);
    }
}
